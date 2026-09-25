//! Attempt orchestration: resolve, download legs, merge, live
//! capture and HLS extraction. Top of the video cluster, over every
//! leaf and mid-level module: the engine drives `run_video_download`
//! through the `video` facade.

use crate::attempt_gate::AttemptGate;
use crate::file_names::is_url_derived_name;
use crate::video_argv::{
    VideoJob, apply_proxy_env, container_truth_name, fallback_to_live_edge, hls_download_argv,
    live_capture_argv, live_remux_argv, merge_output_ext, unified_download_argv,
    unified_format_spec, unified_output_template, write_manifest,
};
use crate::video_plan::{StreamPlan, plan_streams};
use crate::video_probe::page_host;
use crate::video_progress::{
    PROGRESS_GRANULARITY, is_ytdlp_merge_line, last_log_line, leg_changed, parse_ytdlp_after_move,
    parse_ytdlp_template, piece_marks, trace_format_lines,
};
use crate::video_quality::default_video_filename;
use crate::video_spawn::{
    LiveScratchGuard, ProcessGroupGuard, discover_ytdlp_output, drain_stderr_to_tail,
    fetch_video_page, join_drain, reap_child, spawn_piped_ytdlp, ytdlp_command,
};
use crate::video_staging::{
    ResumePlan, ResumeQuery, VideoManifest, clean_dest_parts, collect_sidecar, dest_part_path,
    discover_unified_output, ensure_staging_dir, file_len, read_manifest, release_remux_lease,
    reserve_remux_temp, resume_plan, sidecar_path_for, staging_dir, sweep_partial_remuxes,
    sweep_staging_preserving_recordings,
};
use crate::video_tools::{VideoError, ensure_tool_versions, resolve_libraries};
use crate::video_types::{FetchedVideo, VideoOutcome};
use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use yt_dlp::model::Video;

/// What a stop means for the attempt it reaches.
///
/// This was a bare `oneshot<()>`, which could say "stop" but not *how* to
/// stop, so the worker had exactly one response: adopt the partial, remux
/// it, deliver. Row removal inherited that, and a removed row delivered a
/// file with no row behind it.
///
/// The worker applies the intent rather than asking the manager, because
/// the worker is the only party that knows whether it is capturing live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopIntent {
    /// Stop and keep what is recorded: a user pressing Stop, pause or
    /// cancel. A live capture adopts its partial and delivers it.
    Preserve,
    /// Stop and throw it away: the row is being removed. The worker reaps
    /// its recorder and returns without adopting, remuxing or delivering.
    /// It leaves the scratch, because the manager can only reclaim it
    /// safely once the task has actually returned.
    Discard,
}

/// Run one attempt: resolve → download parts → merge → rename into place.
/// Returns the final size, or `None` when aborted (the pauser/canceller
/// already set the row status; the caller sends nothing).
///
/// # Errors
/// Returns a display-ready [`VideoError`]; the caller reports it as Failed.
pub async fn run_video_download(
    mut job: VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    mut abort: oneshot::Receiver<StopIntent>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
) -> Result<VideoOutcome, VideoError> {
    use crate::engine_msg::EngineMsg;

    let staging = staging_dir(job.item_id);
    // Keep the canonical path: `discover_unified_output` compares a
    // canonicalized `after_move` against it, and on symlinked roots
    // (`/tmp` → `/private/tmp`, Flatpak) the raw join would fail
    // closed to scan on every attempt.
    let staging = ensure_staging_dir(&staging)?;
    let libs = resolve_libraries()?;
    let (yt_version, ff_version) = ensure_tool_versions(&libs).await?;
    tracing::info!(
        item_id = job.item_id,
        host = %page_host(&job.page_url),
        quality = %job.quality,
        yt_dlp = %yt_version,
        ffmpeg = %ff_version,
        "starting video attempt"
    );
    // Attempt timeout: a full-length merge on a slow CPU dwarfs any
    // network timeout, so bound the whole attempt at five minutes.
    let timeout = Duration::from_secs(300);
    let youtube_bin = libs.youtube.clone();
    let ffmpeg_bin = libs.ffmpeg.clone();
    let phase = |text: String| {
        tx.send(EngineMsg::Phase(text)).ok();
    };

    // Resolve (with retries, always fresh: without a cache backend every
    // attempt re-extracts, so expired format URLs never survive a retry).
    phase(if job.audio_only {
        gettext("Resolving audio…")
    } else {
        gettext("Resolving media…")
    });
    let mut video: Option<Video> = None;
    for attempt in 0u32..3 {
        match fetch_video_page(
            &youtube_bin,
            &job.page_url,
            &job.cookies_browser,
            Duration::from_secs(300),
            job.proxy.as_ref(),
            job.playlist_item_id.as_deref(),
        )
        .await
        {
            Ok(FetchedVideo::Single(v)) => {
                video = Some(*v);
                break;
            }
            // No picked entry: the spawner expands the collection into
            // per-item rows instead of failing it (dialog-less rows
            // never see the picker).
            Ok(FetchedVideo::Playlist(pl)) => return Ok(VideoOutcome::Expand(pl)),
            Err(e) if attempt + 1 < 3 => {
                tracing::debug!("video resolve failed, retrying: {e}");
                tokio::time::sleep(Duration::from_secs(u64::from(attempt) + 1)).await;
            }
            Err(e) => return Err(e),
        }
    }
    let Some(video) = video else {
        return Err(VideoError::fetch("empty response"));
    };

    // Dialog-less rows skip the picker, so the row source
    // never marked them live: refresh from resolve metadata instead.
    // Without this a live row takes the HLS VOD path on an infinite
    // manifest (frozen "Resolving media…", wrong stop semantics) and
    // never reaches live capture. Flipping the job field (not a local)
    // keeps every downstream use — dispatch, from-start argv, abort
    // handling, remux gating — consistent; the row source itself is
    // untouched, so the next attempt re-detects idempotently.
    if !job.is_live && video.is_live.unwrap_or(false) {
        job.is_live = true;
        tx.send(EngineMsg::LiveDetected).ok();
    }

    // Dialog-less rows skip the picker, so their names are URL
    // stems ("watch"): rename to the title default now that metadata is
    // in. Dialog-seeded and typed names are untouched — only URL-derived
    // names qualify — and the pump dedupes the suggestion at Finished
    // with collision safety.
    if let Some(current) = job.dest.file_name().and_then(|n| n.to_str())
        && is_url_derived_name(current, &job.page_url)
    {
        // Live rows capture through the dest name with a hardcoded
        // mp4/m4a container: never suggest a remux extension there.
        let remux = if job.is_live {
            None
        } else {
            job.remux_video.as_deref()
        };
        let better = default_video_filename(&video.title, job.audio_only, remux);
        if better != current {
            tx.send(EngineMsg::SuggestName(better)).ok();
        }
    }

    // Select streams: newest codec first (AV1, then VP9/HEVC/AVC1 —
    // same ranking as yt-dlp's `+vcodec:av01` sort), best audio. Older
    // codecs stay as automatic fallback, never a failure. Rejections
    // (HLS/DRM/missing URL) degrade candidates to absent here; the plan
    // below decides between split, single-file and HLS from what's
    // fetchable (see [`plan_streams`] for the priority order:
    // pinned HLS, direct splits, single-part adoption, HLS preset).
    // A pinned format id (dialog pick) wins over the preset; when it
    // vanishes from fresh metadata the preset takes over again instead
    // of failing the row.
    let StreamPlan {
        video_sel,
        audio_sel,
        hls_sel,
    } = plan_streams(
        &video,
        &job.quality,
        job.audio_only,
        job.video_format_id.as_deref(),
        job.newest_codecs,
        job.item_id,
    );
    if let Some(hls) = hls_sel {
        // An abort that fired during resolve means stop-before-start:
        // for live rows there is deliberately no pauser preset waiting
        // on a message, so report instead of going quiet (the pump tail
        // would fail the row either way — this names the cause).
        if job.is_live && abort.try_recv().is_ok() {
            return Err(VideoError::interrupted());
        }
        tracing::info!(
            item_id = job.item_id,
            page_host = %page_host(&job.page_url),
            height = ?hls.height,
            "downloading HLS variant",
        );
        // Live captures go through yt-dlp with a kill-safe MPEG-TS
        // container (variant choice, keys, retries upstream; Stop is
        // kill + adopt + remux, no grace-period finalizing); VOD
        // captures go through yt-dlp's standard HLS path.
        if job.is_live {
            // The extractor's canonical URL, which is what the other legs
            // get in `comment` from --embed-metadata. Falls back to the
            // row's URL exactly as `VideoInfo::from` does, so all three
            // legs stamp the same provenance for the same video.
            let canonical = video
                .webpage_url
                .as_deref()
                .filter(|u| !u.is_empty())
                .unwrap_or(&job.page_url);
            return run_live_ytdlp(
                &youtube_bin,
                &ffmpeg_bin,
                &staging,
                &job,
                gate,
                canonical,
                &hls.format_id,
                abort,
                timeout,
                tx,
            )
            .await
            .map(|opt| opt.map_or(VideoOutcome::Aborted, VideoOutcome::Finished));
        }
        return run_hls_ytdlp(
            &youtube_bin,
            &ffmpeg_bin,
            &staging,
            &job,
            gate,
            &hls.format_id,
            abort,
            timeout,
            tx,
        )
        .await
        .map(|opt| opt.map_or(VideoOutcome::Aborted, VideoOutcome::Finished));
    }
    let Some(audio_sel) = audio_sel else {
        return Err(VideoError::unavailable_detail(&video.formats));
    };

    tracing::info!(
        item_id = job.item_id,
        video = ?video_sel.as_ref().map(|s| s.format_id.as_str()),
        audio = %audio_sel.format_id,
        "formats selected"
    );

    // Retry discipline from the sidecar. The manifest lives in staging
    // (scratch); the parts live beside the finished file (yt-dlp
    // defaults), so every file check builds off the destination.
    let manifest = read_manifest(&staging);
    // Split rows merge video+audio (both sizes known or neither is
    // trusted); an adopted single is one muxed file whose extractor
    // size is the whole file. Anything else leaves the total unknown
    // rather than understating it — an understated total would both
    // shrink the bar and trip the over-long Fresh wipe on valid bytes.
    let single = video_sel.is_none();
    let query = ResumeQuery {
        manifest: manifest.as_ref(),
        dest: &job.dest,
        staging: &staging,
        page_url: &job.page_url,
        quality: &job.quality,
        video: video_sel
            .as_ref()
            .map(|s| (s.format_id.as_str(), s.ext.as_str())),
        audio: (audio_sel.format_id.as_str(), audio_sel.ext.as_str()),
        total: if single {
            audio_sel.size
        } else {
            match (video_sel.as_ref().and_then(|s| s.size), audio_sel.size) {
                (Some(v), Some(a)) => Some(v + a),
                _ => None,
            }
        },
    };
    let plan = resume_plan(&query);
    match plan {
        ResumePlan::Finished => {
            return Ok(match file_len(&job.dest) {
                Some(n) => VideoOutcome::Finished(n),
                None => VideoOutcome::Aborted,
            });
        }
        ResumePlan::Fresh => {
            // Overwrite pre-flight (Parabolic parity): a finished file
            // already at `dest` means the atomic claim (rename_noreplace,
            // which never clobbers) fails at the end no matter what —
            // refuse before a wasted download so the pump requeues under
            // a fresh name. This arm's normal cleanup drops our own
            // shells too, since this row can never adopt them again.
            if job.dest.exists() {
                clean_dest_parts(&job.dest);
                return Err(VideoError::exists());
            }
            // Wipe the staging dir, not just known names: a previous
            // attempt's detached writers (pause winning the abort race) may
            // still hold the old inodes, so unlink first — they write
            // nowhere visible afterwards. Same-selection resume never
            // reaches this arm (see Resume below); only mismatches,
            // oversize parts, or unverifiable leftovers land here.
            let _ = tokio::fs::remove_dir_all(&staging).await;
            tokio::fs::create_dir_all(&staging)
                .await
                .map_err(VideoError::staging)?;
            // Dest-dir parts are Grab-namespaced (`<stem>.<kind>.<ext>`),
            // so a mismatch restarts clean instead of letting yt-dlp
            // resume into a foreign lookalike. The finished file itself
            // is never touched (rename_noreplace guards the claim).
            clean_dest_parts(&job.dest);
            // Record this attempt's selection up front: a pause from here
            // on leaves a matchable sidecar, so the next attempt resumes
            // instead of wiping.
            write_manifest(
                &staging,
                &VideoManifest {
                    page_url: job.page_url.clone(),
                    quality: job.quality.clone(),
                    video_format_id: video_sel.as_ref().map(|s| s.format_id.clone()),
                    video_ext: video_sel
                        .as_ref()
                        .map(|s| s.ext.clone())
                        .unwrap_or_default(),
                    audio_format_id: audio_sel.format_id.clone(),
                    audio_ext: audio_sel.ext.clone(),
                    final_bytes: None,
                },
            )
            .await?;
        }
        ResumePlan::Resume => {
            // Overwrite pre-flight, same as Fresh: the Finished check
            // above already ruled out an adoptable file, so anything at
            // `dest` is foreign or stale — refuse before a wasted
            // download so the pump requeues under a fresh name.
            if job.dest.exists() {
                clean_dest_parts(&job.dest);
                return Err(VideoError::exists());
            }
            phase(gettext("Resuming download…"));
        }
    }
    // One yt-dlp invocation downloads (and merges) the whole selection:
    // the `-f` merge spec carries the planner pair plus the preset
    // fallback pair, so yt-dlp itself retries stale ids. Progress is a
    // single downloaded/total stream like HTTP rows.
    let (spec, _merging) = unified_format_spec(
        video_sel.as_ref().map(|s| s.format_id.as_str()),
        audio_sel.format_id.as_str(),
        &job.quality,
        job.audio_only,
    );
    let combined_total = query.total;
    // NOTE: Grab's speed limit applies to VOD legs via `--ratelimit`
    // (a finite leg can be capped safely). Live capture still runs
    // unthrottled: capping an endless stream would fall behind the edge.
    // Retries and timeouts still follow the user's settings.
    run_unified_ytdlp(
        &youtube_bin,
        &ffmpeg_bin,
        &staging,
        &job,
        gate,
        &spec,
        video_sel.as_ref().map(|s| s.ext.as_str()),
        combined_total,
        &mut abort,
        timeout,
        tx,
    )
    .await
    .map(|opt| opt.map_or(VideoOutcome::Aborted, VideoOutcome::Finished))
}

/// One direct download through a single yt-dlp invocation: yt-dlp picks
/// the first working merge from the spec, merges with its own ffmpeg,
/// and cleans its temp parts itself. Grab claims the output into place
/// (EXDEV-safe, no clobber), collects the subtitle sidecar beside the
/// finished file, records the finished size, and wipes staging.
/// Returns the finished size, or `None` on user abort (the caller stays
/// quiet).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_unified_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    spec: &str,
    video_ext: Option<&str>,
    total: Option<u64>,
    abort: &mut oneshot::Receiver<StopIntent>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::engine_msg::EngineMsg;
    // Split rows merge (video ext mapped onto yt-dlp's supported set);
    // adopted singles download one file, nothing to merge.
    let merging = video_ext.is_some();
    let merge_ext = video_ext.map(merge_output_ext).unwrap_or_default();
    let out_template = unified_output_template(staging);
    let argv = unified_download_argv(job, spec, merging, &merge_ext, ffmpeg_bin, &out_template);
    // Single-counter progress with the same granularity gate the split
    // legs used: the pump renders identical rich detail off these pairs.
    // `done` is the banked leg sum from the attempt loop; cap it here
    // against the metadata total (retried ranges can overshoot) so the
    // bar never passes 100%.
    let sent = Arc::new(AtomicU64::new(0));
    let report = {
        let sent = Arc::clone(&sent);
        let tx = tx.clone();
        Arc::new(move |done: u64, _: u64| {
            let shown = match total {
                Some(t) if t > 0 => done.min(t),
                _ => done,
            };
            let prev = sent.load(Ordering::Relaxed);
            if shown.saturating_sub(prev) >= PROGRESS_GRANULARITY
                || total.is_some_and(|t| t > 0 && shown >= t)
            {
                sent.store(shown, Ordering::Relaxed);
                tx.send(EngineMsg::Progress {
                    downloaded: shown,
                    total,
                    uploaded: 0,
                    upload_bps: 0,
                })
                .ok();
            }
        })
    };
    let (done, after_move) = run_ytdlp_attempt(
        youtube_bin,
        &argv,
        report,
        Some(Arc::new({
            let tx = tx.clone();
            move || {
                tx.send(EngineMsg::Phase(gettext("Merging…"))).ok();
            }
        })),
        job.proxy.as_ref(),
        abort,
        timeout,
    )
    .await?;
    let Some(()) = done else {
        return Ok(None);
    };
    let final_tmp = discover_unified_output(staging, after_move.as_deref());
    let Some(final_tmp) = final_tmp else {
        return Err(VideoError::part_failed("no output file produced"));
    };
    // Measure reality, not the plan: a zero-byte "completed" download
    // must fail now, or the row would sit Done and empty forever.
    if file_len(&final_tmp) == Some(0) {
        return Err(VideoError::part_failed("empty stream"));
    }
    // Container-truth backstop: the intake name assumes mp4 (or the
    // remux target); a native webm merge — or a remux pref changed
    // mid-queue — would otherwise claim under a stale extension. Same
    // stem, so stem reservations hold; the pump dedupes and renames at
    // Finished with collision safety, and exotic typed names are never
    // second-guessed (see `container_truth_name`).
    if let Some(truer) = container_truth_name(&job.dest, &final_tmp) {
        tx.send(EngineMsg::SuggestName(truer)).ok();
    }
    // Atomic claim into place (EXDEV-safe, no clobber).
    // The linearization point, as on the live leg: either this wins and
    // the row is still here, or the removal already won and there is
    // nothing to deliver.
    if !gate.try_commit() {
        let _ = tokio::fs::remove_dir_all(staging).await;
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    // Best-effort subtitle sidecar beside the discovered output
    // (video rows only; audio-only rows never request them). Skipped
    // when embedding: the tracks are muxed into the file itself, so no
    // .srt is left alongside (uncollected sidecars die with staging).
    if !job.audio_only
        && !job.embed_subs
        && let Some(lang) = job.subtitles.as_deref()
    {
        collect_sidecar(&sidecar_path_for(&final_tmp, lang), &job.dest, lang);
    }
    // Sweep legacy dest-dir parts: pre-migration rows (or foreign
    // lookalikes the Fresh arm never saw, e.g. Resume with matching
    // manifest) would otherwise sit beside the finished file forever.
    // The just-collected `<stem>.<lang>.srt` never matches the part
    // namespace, and the finished file itself is exempt.
    clean_dest_parts(&job.dest);
    // Record the finished size so a later retry adopts the file.
    let final_bytes = file_len(&job.dest);
    if let Some(mut m) = read_manifest(staging) {
        m.final_bytes = final_bytes;
        let _ = write_manifest(staging, &m).await;
    }
    sweep_staging_preserving_recordings(staging);
    Ok(Some(final_bytes.unwrap_or(0)))
}

/// One yt-dlp spawn: parse template progress, collect the log tail,
/// capture `--print after_move:filepath`. `Ok((None, _))` is a user
/// abort (the caller stays quiet); timeouts and fetch failures are
/// errors. `on_merge` fires once on the first merge line, if given.
/// Shared by the unified direct path (renamed from the old per-part
/// attempt helper it replaces).
#[allow(clippy::too_many_arguments)]
async fn run_ytdlp_attempt(
    youtube_bin: &Path,
    argv: &[String],
    report: std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>,
    on_merge: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    proxy: Option<&crate::net_types::ResolvedProxy>,
    abort: &mut oneshot::Receiver<StopIntent>,
    timeout: Duration,
) -> Result<(Option<()>, Option<String>), VideoError> {
    use tokio::io::AsyncBufReadExt as _;
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(argv);
    apply_proxy_env(&mut cmd, proxy);
    let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
    let mut group = ProcessGroupGuard::new(&child);
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        // `downloaded_bytes` resets per format leg, so plain max would
        // cap merged progress at the largest single leg. Instead bank
        // each leg's max on its `finished` line and report the running
        // sum; the reset makes double-banking impossible. The outer
        // caller caps the sum against its own metadata total.
        let (mut banked, mut leg_max, mut total) = (0u64, 0u64, 0u64);
        let mut after_move = None::<String>;
        let mut merged = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if !merged && is_ytdlp_merge_line(&line) {
                merged = true;
                if let Some(cb) = on_merge.as_ref() {
                    cb();
                }
            }
            if after_move.is_none()
                && let Some(path) = parse_ytdlp_after_move(&line)
            {
                after_move = Some(path.to_string());
            }
            if let Some(p) = parse_ytdlp_template(&line) {
                if p.finished {
                    banked += p.downloaded.unwrap_or(0).max(leg_max);
                    leg_max = 0;
                } else {
                    if let Some(d) = p.downloaded {
                        leg_max = leg_max.max(d);
                    }
                    if let Some(t) = p.total {
                        total = total.max(t);
                    }
                    if total > 0 {
                        report(banked + leg_max, total);
                    }
                }
            }
        }
        after_move
    });
    let logs = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail = Vec::new();
        let mut pending = String::new();
        let mut buf = [0u8; 4096];
        loop {
            use tokio::io::AsyncReadExt as _;
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    trace_format_lines(&mut pending, &buf[..n]);
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 8192 {
                        tail.drain(..tail.len() - 8192);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let status = tokio::select! {
        biased;
        _ = &mut *abort => {
            reap_child(&mut child, &mut group).await;
            progress.abort();
            logs.abort();
            return Ok((None, None));
        }
        waited = tokio::time::timeout(timeout, child.wait()) => match waited {
            Ok(Ok(status)) => {
                // The leader is reaped, so release the PGID: holding it
                // across the drain joins below would leave a window in
                // which the OS could recycle it onto another group.
                group.disarm();
                status
            }
            Ok(Err(e)) => {
                reap_child(&mut child, &mut group).await;
                progress.abort();
                logs.abort();
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                reap_child(&mut child, &mut group).await;
                progress.abort();
                logs.abort();
                return Err(VideoError::part_failed("timed out"));
            }
        },
    };
    let after_move = progress.await.unwrap_or_default();
    let log_tail = logs.await.unwrap_or_default();
    if !status.success() {
        let detail = last_log_line(&log_tail, "yt-dlp reported failure");
        return Err(VideoError::part_failed(detail));
    }
    Ok((Some(()), after_move))
}

/// Remux a stopped live capture into place. Failures surface ffmpeg's
/// own last line.
///
/// ffmpeg writes `<dest>.part` and the result is renamed to `dest` only
/// once ffmpeg has actually succeeded. That is what separates a completed
/// recording from the debris of a crashed one: a bare `final.<n>.<ext>`
/// is always worth keeping, while a `final.<n>.<ext>.part` is worthless
/// and any sweep may reclaim it. Without the distinction, an attempt that
/// died mid-remux left a file indistinguishable from a finished capture,
/// and retention could only be bounded by deleting recordings too.
pub(crate) async fn remux_live_capture(
    ffmpeg_bin: &Path,
    ts_path: &Path,
    dest: &Path,
    audio_only: bool,
    timeout: Duration,
    page_url: &str,
) -> Result<(), VideoError> {
    let mut partial = dest.as_os_str().to_os_string();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    for with_bsf in [true, false] {
        // ffmpeg runs without `-y` and refuses an existing output, so the
        // bare retry must not inherit the first attempt's partial.
        let _ = tokio::fs::remove_file(&partial).await;
        let mut cmd = tokio::process::Command::new(ffmpeg_bin);
        cmd.args(live_remux_argv(
            ts_path, &partial, audio_only, with_bsf, page_url,
        ));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(VideoError::runtime)?;
        let mut group = ProcessGroupGuard::new(&child);
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| VideoError::runtime("ffmpeg gave no log pipe"))?;
        let logs = drain_stderr_to_tail(stderr);
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                // The leader is reaped, so release the PGID: holding it
                // across the drain joins below would leave a window in
                // which the OS could recycle it onto another group.
                group.disarm();
                status
            }
            Ok(Err(e)) => {
                reap_child(&mut child, &mut group).await;
                join_drain(logs).await;
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                reap_child(&mut child, &mut group).await;
                join_drain(logs).await;
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(VideoError::part_failed("timed out finalizing"));
            }
        };
        let log_tail = join_drain(logs).await.unwrap_or_default();
        if status.success() {
            // Only now is this a recording. A crash before this point
            // leaves a `.part`, which no sweep has to protect.
            return match tokio::fs::rename(&partial, dest).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = tokio::fs::remove_file(&partial).await;
                    Err(VideoError::runtime(&e))
                }
            };
        }
        let detail = last_log_line(&log_tail, "ffmpeg reported failure");
        if with_bsf {
            tracing::info!(error = %detail, "live remux without bsf, retrying bare");
            continue;
        }
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(VideoError::combine(detail));
    }
    unreachable!("bsf retry always returns");
}

/// Why a live capture ended, which decides what scratch is redundant.
///
/// Raw media (the dest-side `live.` shell) and the staging dir (which
/// can hold the *completed* remux) are tracked separately on purpose:
/// conflating them deletes a finished recording on the one exit where
/// both are the user's only copy.
#[derive(Clone, Copy)]
enum Exit {
    /// The remuxed file was claimed at dest: the shell and the emptied
    /// staging dir are both redundant.
    Delivered,
    /// Dest was claimed mid-capture; the row requeues under a fresh
    /// name and records again, so this attempt's shell and its remux
    /// temp are both redundant.
    Requeued,
    /// Reaping the recorder failed, so no remux was ever attempted and
    /// this attempt claimed no temp. The raw shell may hold bytes the
    /// user wants.
    CaptureWaitFailed,
    /// The remux failed, so this attempt's `final.<n>.<ext>` is at best a
    /// partial and the raw shell is the only usable copy of the capture.
    RemuxFailed,
    /// The rename failed unexpectedly (permissions, a destination that
    /// stopped being a directory, I/O). Both the raw shell and this
    /// attempt's completed `final.<n>.<ext>` are the user's recording:
    /// keep both.
    RenameFailed,
    /// Nothing was recorded, so there is nothing to salvage.
    NothingRecorded,
    /// The row was removed while this attempt was finalizing. There is no
    /// row left for a delivered file to belong to, so the completed remux
    /// and the raw shell are both discarded.
    Discarded,
}

/// What a terminal exit does with the row's staging directory.
#[derive(Clone, Copy)]
enum Staging {
    /// Remove this attempt's own remux temp (when one was claimed), then
    /// drop the directory if that left it empty. Not recursive: a sibling
    /// `final.<n>.<ext>` is an earlier attempt's completed recording.
    Sweep,
    /// Leave this attempt's temp in place: it is the completed recording
    /// the final rename could not place.
    Keep,
}

/// Reclaim one live capture's scratch on any terminal exit.
///
/// The `.ytdl` downloader-state file is always *attempted* for removal.
/// yt-dlp writes it beside its `-o` target for fragment downloads and
/// deletes it only on a clean exit, but a killed capture (Stop, stall
/// timeout) never reaches that cleanup. It is pure scratch with a
/// second, sharper cost: Grab wipes the output and `.part` shell before
/// every attempt, so a stale state file would make the next attempt
/// resume fragment N against a shell that no longer exists — a corrupt
/// recording rather than merely litter.
///
/// Staging is **not** swept recursively. Each attempt remuxes into its
/// own `final.<n>.<ext>` (see [`remux_temp_path`]), so a sibling temp is
/// an earlier attempt's completed recording — often the only copy of a
/// capture that could not be placed at its destination. This removes
/// exactly this attempt's temp, then drops the directory only if that
/// left it empty. A whole-directory wipe is what made a Retry destroy
/// the recording it was retrying.
///
/// [`Staging::Keep`] is the one exit that leaves a temp in place, and it
/// passes no `final_tmp` to remove.
///
/// Kept dest-side media is reclaimed with the row via
/// `clean_dest_parts`, and kept staging by `clean_staging` when the row
/// is dropped. A row removed while its finalizer is still in flight is
/// also a known follow-up (see `DownloadManager::remove`).
///
/// Best-effort throughout: a sweep that races a vanished file (or hits
/// a read-only dir) is a no-op, never an error worth failing a row over.
async fn sweep_live_capture(
    out: &Path,
    part: &Path,
    state: &Path,
    staging: &Path,
    final_tmp: Option<&Path>,
    staging_mode: Staging,
    exit: Exit,
) {
    if matches!(
        exit,
        Exit::Delivered | Exit::Requeued | Exit::NothingRecorded
    ) {
        let _ = tokio::fs::remove_file(out).await;
        let _ = tokio::fs::remove_file(part).await;
    }
    let _ = tokio::fs::remove_file(state).await;
    if let Staging::Sweep = staging_mode
        && let Some(path) = final_tmp
    {
        let _ = tokio::fs::remove_file(path).await;
        release_remux_lease(path);
    }
    // Non-recursive: succeeds only when nothing else is in there, so a
    // sibling attempt's remux is never collateral.
    let _ = tokio::fs::remove_dir(staging).await;
}

/// Reap the recorder, and *only then* reclaim its scratch.
///
/// The order is the contract, not a stylistic choice: sweeping first
/// could delete a file the recorder is still writing. It is also nearly
/// invisible from the outside — a real `Child` offers no hook between the
/// two steps, and both orders leave the same end state whenever the reap
/// is quick. So the sequence is factored out to be driven by controlled
/// futures in `video_runner_tests.rs`, which is what pins it; production
/// calls it with the real ones.
///
/// The sweep is a closure rather than a future, so it cannot even be
/// constructed before the reap has completed, and swapping the two
/// arguments is a compile error instead of a silent inversion.
async fn reap_then_sweep<F, S, G>(reap: F, sweep: S)
where
    F: std::future::Future<Output = ()>,
    S: FnOnce() -> G,
    G: std::future::Future<Output = ()>,
{
    reap.await;
    sweep().await;
}

/// One live capture through the yt-dlp binary: variant choice, audio
/// rendition, keys and fragment retries are yt-dlp's; the MPEG-TS
/// container keeps every kill point playable, so Stop is kill, adopt
/// and remux instead of grace-period finalizing.
///
/// Stalled captures yield their partial like before; an empty capture
/// fails. Returns the final size.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_live_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    page_url: &str,
    hls_format_id: &str,
    mut abort: oneshot::Receiver<StopIntent>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::engine_msg::EngineMsg;
    use tokio::io::AsyncBufReadExt as _;
    // Fresh capture: a crashed run's dest-dir live file must never be
    // resumed into (append-only stream — resume corrupts) nor adopted
    // as a fresh capture that recorded nothing. Staging still hosts the
    // remux temp below.
    let ext = if job.audio_only { "m4a" } else { "mp4" };
    // Capture beside the finished file (yt-dlp defaults): the `.part`
    // shell shows up in the user's folder while recording, and the
    // file-growth watcher announces "Recording…" off this path.
    let out = dest_part_path(&job.dest, "live", ext);
    // Overwrite pre-flight (Parabolic parity): a finished file already
    // at dest means the capture's rename claim fails at the end — refuse
    // before recording so the pump requeues under a fresh name instead
    // of wasting an entire stream. The refusal also reclaims this
    // stem's part-namespace scratch: the requeued row carries a fresh
    // name, so a crashed run's leftover `live.` shells and state would
    // otherwise never be swept by `clean_dest_parts` again. Only the
    // part namespace is touched — the finished file at dest is left be.
    if job.dest.exists() {
        clean_dest_parts(&job.dest);
        return Err(VideoError::exists());
    }
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    // Reclaim partial remuxes from attempts that died mid-ffmpeg. They are
    // worthless by construction -- a completed one is renamed to
    // `final.<n>.<ext>` -- so this bounds what a crash leaves behind without
    // ever putting a real recording at risk.
    sweep_partial_remuxes(staging);
    // At most two attempts: the from-start capture the user asked for,
    // then -- only if it recorded nothing and wasn't stopped -- one retry
    // from the live edge. The downgrade flips solely `live_from_start`
    // (the other fields the capture argv reads stay as cloned), so every
    // other `job` use below stays valid on both attempts.
    let part = out.with_extension(format!("{ext}.part"));
    let state = out.with_extension(format!("{ext}.ytdl"));
    // Covers the await windows a shutdown can cancel, so the state file
    // does not outlive the app. The recorded media is left for the user.
    // Held purely for its `Drop`, hence the underscore.
    let _scratch = LiveScratchGuard::new(&state);
    let mut downgraded: Option<VideoJob> = None;
    let src = loop {
        let attempt: &VideoJob = downgraded.as_ref().unwrap_or(job);
        // Fresh shell per attempt: a failed attempt must never leave a
        // stale (possibly empty) output for the retry to trip over. The
        // state file joins them for a sharper reason than litter — it
        // must not survive into a retry whose media shell was just
        // deleted, or yt-dlp resumes fragment N against a shell that no
        // longer exists and writes a corrupt recording.
        let _ = tokio::fs::remove_file(&out).await;
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&state).await;
        let mut cmd = ytdlp_command(youtube_bin);
        cmd.args(live_capture_argv(attempt, hls_format_id, &out));
        apply_proxy_env(&mut cmd, job.proxy.as_ref());
        let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
        // An abort drops this future mid-await and tokio does not kill a
        // child when its handle drops, so without this guard a shutdown
        // orphans the recorder — and the ffmpeg it may have started —
        // still writing to the capture.
        let mut group = ProcessGroupGuard::new(&child);
        let tx_p = tx.clone();
        // Recording indicator: live captures often emit no yt-dlp progress
        // lines for long stretches, leaving the row stuck on "Resolving
        // media…" while bytes land on disk. The growing output file is the
        // truth — announce once it has bytes (capped: minutes of silence
        // means the capture is dead and its own timeouts will fire).
        {
            let tx_rec = tx.clone();
            let out_rec = out.clone();
            // yt-dlp records into the `.part` shell and only renames to the
            // `-o` path at the end: the shell is what grows during capture.
            // The final path covers a capture that finalized instantly.
            let shell_rec = out.with_extension(format!("{ext}.part"));
            tokio::spawn(async move {
                for _ in 0..1200 {
                    let mut bytes = 0u64;
                    for p in [&shell_rec, &out_rec] {
                        bytes =
                            bytes.max(tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0));
                    }
                    if bytes > 0 {
                        tx_rec.send(EngineMsg::Phase(gettext("Recording…"))).ok();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
        }
        let progress = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let mut have = 0u64;
            // Announce once recording is confirmed: a parsed progress line
            // means transfer (same "Recording…" the file watcher sends, so
            // whichever fires first wins and the second is a no-op).
            let mut announced = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(p) = parse_ytdlp_template(&line) {
                    if !announced {
                        announced = true;
                        tx_p.send(EngineMsg::Phase(gettext("Recording…"))).ok();
                    }
                    if let Some(d) = p.downloaded {
                        have = have.max(d);
                    }
                    tx_p.send(EngineMsg::Progress {
                        downloaded: have,
                        total: None,
                        uploaded: 0,
                        upload_bps: 0,
                    })
                    .ok();
                }
            }
            have
        });
        let logs = drain_stderr_to_tail(stderr);
        // The numeric group id for the quiescence wait on the discard
        // path below: `reap_child` disarms the guard, so it must be read
        // while the guard is still armed.
        let pgid = group.pgid();
        // `aborted` gates the live-edge retry below: a stopped attempt must
        // never come back as a fresh capture. `&mut abort` keeps the receiver
        // usable for the second attempt when it didn't fire.
        let (aborted, discarded) = tokio::select! {
            biased;
            intent = &mut abort => {
                reap_child(&mut child, &mut group).await;
                match intent {
                    Ok(StopIntent::Preserve) => (true, false),
                    Ok(StopIntent::Discard) => (true, true),
                    Err(_) => {
                        // No sender remains to authorise anything: fail
                        // closed. Claim the gate so the pre-rename commit
                        // below cannot deliver either, and take the
                        // discarded path.
                        let _ = gate.discard();
                        (true, true)
                    }
                }
            }
            waited = tokio::time::timeout(timeout, child.wait()) => {
                match waited {
                    Ok(Ok(_)) => group.disarm(),
                    Ok(Err(e)) => {
                        // Reap, then reclaim — never the other way round.
                        // The recording may already exist, so the sweep
                        // keeps it and takes only the scratch around it.
                        progress.abort();
                        logs.abort();
                        reap_then_sweep(
                            reap_child(&mut child, &mut group),
                            || {
                                sweep_live_capture(
                                    &out,
                                    &part,
                                    &state,
                                    staging,
                                    None,
                                    Staging::Sweep,
                                    Exit::CaptureWaitFailed,
                                )
                            },
                        )
                        .await;
                        return Err(VideoError::runtime(&e));
                    }
                    Err(_) => {
                        // A stalled live capture still yields what it got.
                        reap_child(&mut child, &mut group).await;
                    }
                }
                (false, false)
            }
        };
        let _ = join_drain(progress).await;
        // A discard is not a stop. There is no row left to deliver to, so
        // the finalize path below must not run: adopting the partial and
        // remuxing it would place a file at a destination with no row
        // behind it. Only the direct child is reaped here; group
        // descendants may still be writing (see the quiescence wait below).
        //
        // The scratch is deliberately left. The manager reclaims it only
        // after this task has returned -- sweeping from inside a task that
        // may still be running is the race this avoids.
        if discarded {
            logs.abort();
            // `reap_child` waited only the direct child, and the guard only
            // *signalled* the group: a descendant may still be writing when
            // this returns and the manager reclaims the scratch, so wait
            // for the group, bounded, and report rather than assume.
            if let Some(pgid) = pgid
                && !crate::video_spawn::await_group_quiescence(pgid, timeout)
            {
                tracing::warn!(
                    "recorder process group did not quiesce; reclaiming anyway with a \
                     writer possibly still present"
                );
            }
            return Ok(None);
        }
        let log_tail = join_drain(logs).await.unwrap_or_default();
        // Whatever stopped the capture — user stop, stall, stream end, or
        // crash — adopt what landed: MPEG-TS needs no finalizing. yt-dlp
        // renames the `.part` shell on clean completion, so prefer the
        // finished name and fall back to the shell.
        let src = [out.clone(), part.clone()]
            .into_iter()
            .find(|p| file_len(p).is_some_and(|n| n > 0));
        let Some(src) = src else {
            // From-start attempt that never got going: retry once from the
            // live edge instead of failing the row, and say so on the row.
            // Only a startup miss qualifies — a mid-capture failure keeps
            // its error, so partial recordings are never discarded.
            // Staging is untouched here (nothing was recorded); the
            // terminal path below sweeps it.
            if fallback_to_live_edge(
                attempt.is_live,
                attempt.live_from_start,
                aborted,
                downgraded.is_some(),
            ) {
                tx.send(EngineMsg::Phase(gettext(
                    "\"Live from start\" isn't available for this stream — recording from the live edge…",
                )))
                .ok();
                let mut edge = job.clone();
                edge.live_from_start = false;
                downgraded = Some(edge);
                continue;
            }
            // Nothing was recorded, so there is no media to salvage —
            // drop whatever shells and state yt-dlp left behind.
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                None,
                Staging::Sweep,
                Exit::NothingRecorded,
            )
            .await;
            // Startup failure: surface yt-dlp's line, not a generic miss.
            let detail = log_tail
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("nothing recorded")
                .trim()
                .to_string();
            return Err(VideoError::part_failed(detail));
        };
        break src;
    };
    tx.send(EngineMsg::Phase(gettext("Finalizing…"))).ok();
    let final_tmp = match reserve_remux_temp(staging, ext) {
        Ok(path) => path,
        // No claimable slot: leave the recorded shell for salvage rather
        // than risk sharing one. This is the pre-existing behaviour.
        Err(e) => {
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                None,
                Staging::Sweep,
                Exit::RemuxFailed,
            )
            .await;
            return Err(e);
        }
    };
    if let Err(e) = remux_live_capture(
        ffmpeg_bin,
        &src,
        &final_tmp,
        job.audio_only,
        timeout,
        page_url,
    )
    .await
    {
        // The remuxed file never materialized, so the recorded shell is
        // the user's only copy of the capture: keep it for salvage and
        // drop only the scratch around it.
        sweep_live_capture(
            &out,
            &part,
            &state,
            staging,
            Some(&final_tmp),
            Staging::Sweep,
            Exit::RemuxFailed,
        )
        .await;
        return Err(e);
    }
    // The linearization point. `try_commit` is a CAS, so there is no window
    // between deciding and acting for a concurrent removal to slip into:
    // either this wins and the row is still here, or the removal already
    // won and there is nothing to deliver.
    if !gate.try_commit() {
        sweep_live_capture(
            &out,
            &part,
            &state,
            staging,
            Some(&final_tmp),
            Staging::Sweep,
            Exit::Discarded,
        )
        .await;
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // The name was claimed mid-capture; the row requeues under a
            // fresh name and records again, so the shell is redundant.
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                Some(&final_tmp),
                Staging::Sweep,
                Exit::Requeued,
            )
            .await;
            return Err(VideoError::exists());
        }
        Err(e) => {
            // An unexpected rename failure (permissions, I/O) leaves both
            // the recorded shell and the completed remux in staging as the
            // user's only copies: keep both for salvage. Staging is
            // deliberately left alone rather than swept — see
            // `sweep_live_capture` on why that stays a follow-up.
            sweep_live_capture(
                &out,
                &part,
                &state,
                staging,
                None,
                Staging::Keep,
                Exit::RenameFailed,
            )
            .await;
            return Err(VideoError::combine(&e));
        }
    }
    // Claimed: the remuxed file is at its final name, so the shell and
    // the state file are both redundant now.
    sweep_live_capture(
        &out,
        &part,
        &state,
        staging,
        Some(&final_tmp),
        Staging::Sweep,
        Exit::Delivered,
    )
    .await;
    Ok(file_len(&job.dest))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hls_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    gate: &std::sync::Arc<AttemptGate>,
    hls_format_id: &str,
    abort: oneshot::Receiver<StopIntent>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::engine_msg::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::engine_msg::EngineMsg;
    use tokio::io::AsyncBufReadExt as _;
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    // Overwrite pre-flight (Parabolic parity): refuse before transferring
    // when the claim target is already taken — rename_noreplace never
    // clobbers, so the run would only fail after a wasted download.
    if job.dest.exists() {
        return Err(VideoError::exists());
    }
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(hls_download_argv(job, hls_format_id, ffmpeg_bin, &job.dest));
    apply_proxy_env(&mut cmd, job.proxy.as_ref());
    let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
    let mut group = ProcessGroupGuard::new(&child);
    // Progress lines may land on either stream depending on version;
    // parse both, collect the log tail for failure diagnostics.
    let tx_p = tx.clone();
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let (mut max_dl, mut max_total, mut marked) = (0u64, None, 0u64);
        // Total the live block grid was built for, and bytes within the
        // current leg: `max_dl` stays monotonic across legs for the bar,
        // while `leg_have` resets so a second leg's map starts empty
        // instead of instantly filling from the previous leg's bytes.
        let (mut grid_total, mut leg_have) = (None::<u64>, 0u64);
        let mut after_move = None::<String>;
        let mut merged = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if is_ytdlp_merge_line(&line) && !merged {
                merged = true;
                tx_p.send(EngineMsg::Phase(gettext("Merging…"))).ok();
            } else if let Some(path) = parse_ytdlp_after_move(&line) {
                after_move = Some(path.to_string());
            } else if let Some(p) = parse_ytdlp_template(&line) {
                if let Some(t) = p.total {
                    if leg_changed(max_total, max_dl, t, p.downloaded) {
                        // New format leg (video→audio): fresh grid and a
                        // leg-relative byte basis (see `leg_changed`).
                        tx_p.send(EngineMsg::SegmentsInit { total: t }).ok();
                        marked = 0;
                        leg_have = 0;
                        grid_total = Some(t);
                    } else if t > 0 && grid_total.is_none_or(|g| t > g.saturating_mul(2)) {
                        // Same file, refined-up total (first estimates run
                        // tiny): rebuild the grid and re-derive marks from
                        // real bytes, or the map stays flood-lit on its
                        // stale small grid while the bar climbs. Downward
                        // wobble never rebuilds (the leg gate above owns
                        // drops); growth past 2x bounds the rebuilds to a
                        // handful per download.
                        tx_p.send(EngineMsg::SegmentsInit { total: t }).ok();
                        grid_total = Some(t);
                        let len = crate::file_names::piece_len(t);
                        marked = 0;
                        if let Some(count) = leg_have.checked_div(len) {
                            for idx in 0..count {
                                tx_p.send(EngineMsg::PieceDone(idx)).ok();
                                marked += 1;
                            }
                        }
                    }
                    max_total = Some(t.max(max_total.unwrap_or(0)));
                }
                if let Some(d) = p.downloaded
                    && let Some(grid) = grid_total
                    && grid > 0
                {
                    // Marks align with the displayed grid (not the running
                    // max): the grid may lag refined-up totals, and marks
                    // past its end are dropped by the row's bounds check.
                    leg_have = leg_have.max(d.min(grid));
                    let have = max_dl.max(d.min(max_total.unwrap_or(grid)));
                    max_dl = have;
                    for idx in
                        piece_marks(crate::file_names::piece_len(grid), &mut marked, leg_have)
                    {
                        tx_p.send(EngineMsg::PieceDone(idx)).ok();
                    }
                }
                tx_p.send(EngineMsg::Progress {
                    downloaded: max_dl,
                    total: max_total,
                    uploaded: 0,
                    upload_bps: 0,
                })
                .ok();
            }
        }
        (max_dl, max_total, after_move)
    });
    let logs = drain_stderr_to_tail(stderr);
    let status = tokio::select! {
        biased;
        _ = abort => {
            reap_child(&mut child, &mut group).await;
            progress.abort();
            logs.abort();
            sweep_staging_preserving_recordings(staging);
            return Ok(None);
        }
        waited = tokio::time::timeout(timeout, child.wait()) => match waited {
            Ok(Ok(status)) => {
                // The leader is reaped, so release the PGID: holding it
                // across the drain joins below would leave a window in
                // which the OS could recycle it onto another group.
                group.disarm();
                status
            }
            Ok(Err(e)) => {
                reap_child(&mut child, &mut group).await;
                progress.abort();
                logs.abort();
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                reap_child(&mut child, &mut group).await;
                progress.abort();
                logs.abort();
                return Err(VideoError::part_failed("timed out"));
            }
        },
    };
    let (mut _downloaded, _total, after_move) = progress.await.unwrap_or_default();
    let log_tail = logs.await.unwrap_or_default();
    if !status.success() {
        let detail = last_log_line(&log_tail, "yt-dlp reported failure");
        return Err(VideoError::part_failed(detail));
    }
    let final_tmp = discover_ytdlp_output(&job.dest, after_move.as_deref());
    let Some(final_tmp) = final_tmp else {
        return Err(VideoError::part_failed("no output file produced"));
    };
    // Same container-truth backstop as the unified path: the HLS merge
    // is mp4-only, but `--remux-video` is honored here too, so a remux
    // pref changed mid-queue would otherwise claim under a stale name.
    // (Live rows need none of this: ext is fixed to mp4/m4a on both
    // sides and live never remuxes, so divergence is impossible.)
    if let Some(truer) = container_truth_name(&job.dest, &final_tmp) {
        tx.send(EngineMsg::SuggestName(truer)).ok();
    }
    // Atomic claim into place (EXDEV-safe, no clobber).
    // The linearization point, as on the live leg: either this wins and
    // the row is still here, or the removal already won and there is
    // nothing to deliver. The part shells beside the finished file are
    // ours to sweep: no rename means no delivery happened.
    if !gate.try_commit() {
        clean_dest_parts(&job.dest);
        let _ = tokio::fs::remove_dir_all(staging).await;
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    // Best-effort subtitle sidecar: `-o` is the `hls` part template, so
    // collect `<stem>.hls.<lang>.srt` beside the finished file (outside
    // the part namespace, so retries and row removal keep it). Skipped
    // when embedding: the tracks are muxed into the file itself — and
    // the part sidecar is deleted outright, since unlike the unified
    // path it lives in the dest dir (no staging wipe reaches it).
    if !job.embed_subs
        && let Some(lang) = job.subtitles.as_deref()
    {
        collect_sidecar(
            &dest_part_path(&job.dest, "hls", &format!("{lang}.srt")),
            &job.dest,
            lang,
        );
    } else if let Some(lang) = job.subtitles.as_deref() {
        let _ =
            tokio::fs::remove_file(dest_part_path(&job.dest, "hls", &format!("{lang}.srt"))).await;
    }
    sweep_staging_preserving_recordings(staging);
    Ok(file_len(&job.dest))
}

#[cfg(test)]
#[path = "video_runner_tests.rs"]
mod tests;
