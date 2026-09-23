//! Video-page downloads (YouTube, Vimeo, …) powered by yt-dlp.
//!
//! Grab only *extracts* with yt-dlp: format URLs are resolved here and the
//! bytes are pulled by the existing engine as ordinary queue items. Two
//! things shape this module:
//!
//! * **CDN URLs expire.** YouTube format links are valid for roughly
//!   [`FORMAT_URL_LIFETIME`] seconds after they are issued, so the persisted
//!   identity of a video download is the *page* URL, never the media URL.
//!   On restore, an expired (or missing) media URL is re-resolved with a
//!   fresh extraction before resuming.
//! * **The tools may be missing.** Neither the Flatpak nor tarball/dev
//!   builds bundle `yt-dlp` or ffmpeg; both are fetched on demand into the
//!   user library directory ([`user_lib_dir`]) by [`install_ytdlp`] and
//!   [`install_ffmpeg`]. Callers detect the gap with [`resolve_libraries`]
//!   and offer an install action.
//!
//! All yt-dlp work runs on Grab's shared Tokio runtime
//! ([`crate::runtime::tokio_rt`]) so no GTK thread is ever blocked.

use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::oneshot;
use yt_dlp::client::deps::Libraries;
use yt_dlp::model::Video;

pub use crate::video_argv::VideoJob;
/// Facade: attempt inputs + argv builders live in
/// [`video_argv`](crate::video_argv) now; these re-exports keep every
/// `crate::video::X` path working. (Names used only inside this module
/// or its tests stay imported below without re-export.)
use crate::video_argv::{
    apply_proxy_env, container_truth_name, fallback_to_live_edge, hls_download_argv,
    live_capture_argv, live_remux_argv, merge_output_ext, proxy_cli_args, unified_download_argv,
    unified_format_spec, unified_output_template, write_manifest,
};
/// Facade: stream planning lives in [`video_plan`](crate::video_plan)
/// now (no re-exports: the runner consumes it here, tests import it
/// directly).
use crate::video_plan::{StreamPlan, plan_streams};
/// Facade: preference combos + active-value resolution live in
/// [`video_prefs`](crate::video_prefs) now; these re-exports keep every
/// `crate::video::X` path working. (Names used only inside this module
/// or its tests stay imported below without re-export.)
pub use crate::video_prefs::{
    CODEC_PRIORITY_NEWEST, codec_priority_index, codec_priority_labels, codec_priority_value,
    cookies_browser_index, cookies_browser_labels, cookies_browser_value, remux_video_index,
    remux_video_labels, remux_video_value, subtitle_language_index, subtitle_language_labels,
    subtitle_language_value,
};
pub use crate::video_probe::{drive_direct_url, is_direct_file_url, is_http_url};
/// Facade: page probing + playlist parsing lives in
/// [`video_probe`](crate::video_probe) now; these re-exports keep every
/// `crate::video::X` path working. (Names used only inside this module
/// or its tests stay imported below without re-export.)
use crate::video_probe::{
    page_host, parse_playlist_json, parse_single_video, pick_playlist_entry,
    playlist_resolve_error, retarget_story_items, sanitize_video_json,
};
/// Facade: progress-line parsing lives in
/// [`video_progress`](crate::video_progress) now (no re-exports: the
/// runner consumes it here, tests import it directly).
use crate::video_progress::{
    PROGRESS_GRANULARITY, is_ytdlp_merge_line, last_log_line, leg_changed, parse_ytdlp_after_move,
    parse_ytdlp_template, piece_marks, trace_format_lines,
};
pub use crate::video_quality::{
    default_quality_index, default_video_filename, quality_for_height, quality_labels,
};
/// Facade: staging/parts/manifest/resume lives in
/// [`video_staging`](crate::video_staging) now; these re-exports keep
/// every `crate::video::X` path working. (Names used only inside this
/// module or its tests stay imported below without re-export.)
use crate::video_staging::{
    ResumePlan, ResumeQuery, VideoManifest, collect_sidecar, dest_part_path,
    discover_unified_output, ensure_staging_dir, file_len, read_manifest, resume_plan,
    sidecar_path_for,
};
pub use crate::video_staging::{clean_dest_parts, clean_staging, staging_dir, staging_root};
/// Facade: tool provisioning lives in [`video_tools`](crate::video_tools)
/// now; these re-exports keep every `crate::video::X` path working.
/// (Names used only inside this module or its tests stay imported
/// below without re-export.)
use crate::video_tools::VideoError;
use crate::video_tools::ensure_tool_versions;
use crate::video_tools::ytdlp_identity_args;
pub use crate::video_tools::{install_ffmpeg, install_ytdlp, latest_ytdlp_tag, resolve_libraries};

/// Facade: probe identity lives in [`video_types`](crate::video_types)
/// now; these re-exports keep every `crate::video::X` path working.
/// (Names used only inside this module or its tests stay imported
/// below without re-export.)
use crate::video_types::FetchedVideo;
pub use crate::video_types::{ProbeResult, VideoInfo, VideoOutcome, is_video_page, preview_fresh};

/// Whether a row name is just the page URL derived at intake:
/// dialog-less rows skip the picker, so their names are URL stems
/// ("watch"). Matches the derived stem modulo intake-dedupe ` (N)`
/// suffixes. Dialog-seeded and typed names never match (unless
/// perversely identical to the URL stem). Pure for tests.
pub(crate) fn is_url_derived_name(current: &str, page_url: &str) -> bool {
    let derived = crate::file_names::filename_from_url(page_url);
    current == derived || strip_dedupe_suffix(current) == derived
}

/// Intake-dedupe suffix stripped: `watch (12)` → `watch`,
/// `Clip (3).mp4` → `Clip.mp4`. ASCII-boundary operations only, so
/// non-ASCII titles are never split mid-codepoint. Pure for tests.
fn strip_dedupe_suffix(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i..])),
        _ => (name, None),
    };
    if let Some(open) = stem.rfind(" (") {
        let inner = &stem[open + 2..];
        if !inner.is_empty()
            && inner
                .strip_suffix(')')
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            let base = &stem[..open];
            if base.is_empty() {
                return name.to_string();
            }
            return match ext {
                Some(e) => format!("{base}{e}"),
                None => base.to_string(),
            };
        }
    }
    name.to_string()
}

/// How long one metadata extraction may take before it counts as failed.
/// Without a ceiling a throttled host parks the dialog on its spinner
/// forever; the error path (with Retry) is strictly more useful.
const FETCH_TIMEOUT_SECS: u64 = 60;

/// Fetch one page's raw `--dump-single-json` through a direct spawn
/// (same spawn/timeout/output semantics as the crate's extractors),
/// then parse leniently (see [`crate::video_probe::sanitize_video_json`]). Used instead of
/// the crate's `fetch_video_infos`, whose strict model breaks whenever
/// the binary's JSON gains or drops a field.
///
/// `flat_playlist` selects the probe mode: `true` lists collection
/// entries as stubs instead of extracting every one (minutes and
/// megabytes for big lists; a no-op for single videos) — the dialog's
/// collection probe. `false` runs full extraction — the download
/// worker's single-item resolve, where stub listings would come back
/// with no formats and fail every row.
async fn fetch_raw_dump_json(
    youtube_bin: &Path,
    url: &str,
    cookies_browser: &str,
    timeout: Duration,
    fetch_proxy: Option<&crate::net_types::ResolvedProxy>,
    flat_playlist: bool,
) -> Result<serde_json::Value, VideoError> {
    let mut args = vec!["--ignore-config".to_string(), "--no-progress".to_string()];
    if flat_playlist {
        args.push("--flat-playlist".to_string());
    }
    args.push("--dump-single-json".to_string());
    args.extend(proxy_cli_args(fetch_proxy));
    args.extend(ytdlp_identity_args(cookies_browser, None, url));
    // Spawned directly (tokio + timeout) rather than through the
    // crate's executor: same semantics — concurrent pipe drain,
    // timeout kill, nonzero exit as error — with the failure detail
    // taken from stderr instead of a wrapped crate error.
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(&args);
    apply_proxy_env(&mut cmd, fetch_proxy);
    let mut child = cmd.spawn().map_err(VideoError::fetch)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VideoError::fetch("yt-dlp gave no output pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VideoError::fetch("yt-dlp gave no log pipe"))?;
    // Drain both pipes concurrently: `--dump-single-json` output is
    // megabytes, and an unread pipe would stall yt-dlp once full.
    fn drain(
        stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    ) -> tokio::task::JoinHandle<Vec<u8>> {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut buf = Vec::new();
            let mut reader = tokio::io::BufReader::new(stream);
            reader.read_to_end(&mut buf).await.ok();
            buf
        })
    }
    let out_task = drain(stdout);
    let err_task = drain(stderr);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            kill_tree(&mut child);
            return Err(VideoError::fetch(&e));
        }
        Err(_) => {
            kill_tree(&mut child);
            let _ = child.wait().await;
            return Err(VideoError::fetch(gettext("the lookup timed out")));
        }
    };
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    if !status.success() {
        let detail = last_log_line(&String::from_utf8_lossy(&stderr), "yt-dlp reported failure");
        return Err(VideoError::fetch(detail));
    }
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&stdout)).map_err(VideoError::fetch)?;
    Ok(value)
}

/// Strict single-video model for the download worker, with one shaped
/// exception. Full extraction here, not `--flat-playlist`: stub listings
/// parse as a video with no formats, which is exactly the "the page
/// listed none" failure a queued story hit. Playlist-shaped output with
/// a picked entry id selects that entry (highlight rows, and story rows
/// whose segment page didn't parse at pick time, re-resolve the whole
/// tray); without one it returns the collection for worker-side
/// expansion into per-item rows instead of failing.
async fn fetch_video_page(
    youtube_bin: &Path,
    url: &str,
    cookies_browser: &str,
    timeout: Duration,
    fetch_proxy: Option<&crate::net_types::ResolvedProxy>,
    playlist_item_id: Option<&str>,
) -> Result<FetchedVideo, VideoError> {
    let value = fetch_raw_dump_json(
        youtube_bin,
        url,
        cookies_browser,
        timeout,
        fetch_proxy,
        false,
    )
    .await?;
    if let Some(playlist) = parse_playlist_json(&value, url) {
        if playlist_item_id.is_some() {
            if let Some(entry) = pick_playlist_entry(&value, playlist_item_id) {
                return parse_single_video(entry).map(|v| FetchedVideo::Single(Box::new(v)));
            }
            return Err(playlist_resolve_error(playlist_item_id));
        }
        return Ok(FetchedVideo::Playlist(playlist));
    }
    parse_single_video(value).map(|v| FetchedVideo::Single(Box::new(v)))
}

/// Extract metadata for one URL. Singles and collections share the
/// probe: the returned [`ProbeResult`] tells the dialog which preview
/// to show. The media URLs inside are only passed on to the download
/// step; the *page URL* is what survives restarts.
pub async fn fetch_video_infos(
    libs: Libraries,
    url: String,
    cookies_browser: String,
    newest_first: bool,
    fetch_proxy: Option<crate::net_types::ResolvedProxy>,
) -> Result<ProbeResult, VideoError> {
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let (yt_version, _ff_version) = ensure_tool_versions(&libs).await?;
        tracing::info!(yt_dlp = %yt_version, url_host = %page_host(&url), "resolving video page");
        let out = staging_root();
        ensure_staging_dir(&out)?;
        let mut value = match tokio::time::timeout(
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            fetch_raw_dump_json(
                &libs.youtube,
                &url,
                &cookies_browser,
                Duration::from_secs(300),
                fetch_proxy.as_ref(),
                true,
            ),
        )
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(VideoError::fetch(gettext("the lookup timed out")));
            }
        };
        // Playlist-shaped output never reaches the video model: its
        // entries are stubs the crate's strict structs would choke on.
        if let Some(mut playlist) = parse_playlist_json(&value, &url) {
            retarget_story_items(&url, &mut playlist);
            return Ok::<_, VideoError>(ProbeResult::Playlist(playlist));
        }
        sanitize_video_json(&mut value);
        let video: Video = serde_json::from_value(value).map_err(VideoError::fetch)?;
        Ok::<_, VideoError>(ProbeResult::Single(VideoInfo::from(
            &video,
            &url,
            newest_first,
        )))
    });
    match handle.await {
        Ok(r) => r,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// yt-dlp spawn with the house stdio/process-group setup: null stdin
/// (never interactive), piped stdout/stderr for capture, and its own
/// process group so timeouts can kill the whole tree via `kill_tree`.
fn ytdlp_command(youtube_bin: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(youtube_bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd
}

/// Spawn a [`ytdlp_command`]-configured child and take its stdout/stderr
/// pipes. Fails with a runtime error naming the missing pipe.
fn spawn_piped_ytdlp(
    mut cmd: tokio::process::Command,
) -> Result<
    (
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    ),
    VideoError,
> {
    let mut child = cmd.spawn().map_err(VideoError::runtime)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VideoError::runtime("yt-dlp gave no output pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VideoError::runtime("yt-dlp gave no log pipe"))?;
    Ok((child, stdout, stderr))
}

/// Run one attempt: resolve → download parts → merge → rename into place.
/// Returns the final size, or `None` when aborted (the pauser/canceller
/// already set the row status; the caller sends nothing).
///
/// # Errors
/// Returns a display-ready [`VideoError`]; the caller reports it as Failed.
pub async fn run_video_download(
    mut job: VideoJob,
    mut abort: oneshot::Receiver<()>,
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
        let better = default_video_filename(&video.title, &video.id, job.audio_only, remux);
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
            return run_live_ytdlp(
                &youtube_bin,
                &ffmpeg_bin,
                &staging,
                &job,
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
async fn run_unified_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    spec: &str,
    video_ext: Option<&str>,
    total: Option<u64>,
    abort: &mut oneshot::Receiver<()>,
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
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {}
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
    let _ = tokio::fs::remove_dir_all(staging).await;
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
    abort: &mut oneshot::Receiver<()>,
    timeout: Duration,
) -> Result<(Option<()>, Option<String>), VideoError> {
    use tokio::io::AsyncBufReadExt as _;
    let mut cmd = ytdlp_command(youtube_bin);
    cmd.args(argv);
    apply_proxy_env(&mut cmd, proxy);
    let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
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
            kill_tree(&mut child);
            let _ = child.wait().await;
            progress.abort();
            logs.abort();
            return Ok((None, None));
        }
        waited = tokio::time::timeout(timeout, child.wait()) => match waited {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                kill_tree(&mut child);
                progress.abort();
                logs.abort();
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                kill_tree(&mut child);
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
async fn remux_live_capture(
    ffmpeg_bin: &Path,
    ts_path: &Path,
    dest: &Path,
    audio_only: bool,
    timeout: Duration,
) -> Result<(), VideoError> {
    for with_bsf in [true, false] {
        let mut cmd = tokio::process::Command::new(ffmpeg_bin);
        cmd.args(live_remux_argv(ts_path, dest, audio_only, with_bsf));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(VideoError::runtime)?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| VideoError::runtime("ffmpeg gave no log pipe"))?;
        let logs = drain_stderr_to_tail(stderr);
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                kill_tree(&mut child);
                join_drain(logs).await;
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                kill_tree(&mut child);
                let _ = child.wait().await;
                join_drain(logs).await;
                return Err(VideoError::part_failed("timed out finalizing"));
            }
        };
        let log_tail = join_drain(logs).await.unwrap_or_default();
        if status.success() {
            return Ok(());
        }
        let detail = last_log_line(&log_tail, "ffmpeg reported failure");
        if with_bsf {
            tracing::info!(error = %detail, "live remux without bsf, retrying bare");
            continue;
        }
        return Err(VideoError::combine(detail));
    }
    unreachable!("bsf retry always returns");
}

/// One live capture through the yt-dlp binary: variant choice, audio
/// rendition, keys and fragment retries are yt-dlp's; the MPEG-TS
/// container keeps every kill point playable, so Stop is kill, adopt
/// and remux instead of grace-period finalizing.
///
/// Stalled captures yield their partial like before; an empty capture
/// fails. Returns the final size.
#[allow(clippy::too_many_arguments)]
async fn run_live_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    hls_format_id: &str,
    mut abort: oneshot::Receiver<()>,
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
    // of wasting an entire stream.
    if job.dest.exists() {
        return Err(VideoError::exists());
    }
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    // At most two attempts: the from-start capture the user asked for,
    // then -- only if it recorded nothing and wasn't stopped -- one retry
    // from the live edge. The downgrade flips solely `live_from_start`
    // (the other fields the capture argv reads stay as cloned), so every
    // other `job` use below stays valid on both attempts.
    let part = out.with_extension(format!("{ext}.part"));
    let mut downgraded: Option<VideoJob> = None;
    let src = loop {
        let attempt: &VideoJob = downgraded.as_ref().unwrap_or(job);
        // Fresh shell per attempt: a failed attempt must never leave a
        // stale (possibly empty) output for the retry to trip over.
        let _ = tokio::fs::remove_file(&out).await;
        let _ = tokio::fs::remove_file(&part).await;
        let mut cmd = ytdlp_command(youtube_bin);
        cmd.args(live_capture_argv(attempt, hls_format_id, &out));
        apply_proxy_env(&mut cmd, job.proxy.as_ref());
        let (mut child, stdout, stderr) = spawn_piped_ytdlp(cmd)?;
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
        // `aborted` gates the live-edge retry below: a stopped attempt must
        // never come back as a fresh capture. `&mut abort` keeps the receiver
        // usable for the second attempt when it didn't fire.
        let aborted = tokio::select! {
            biased;
            _ = &mut abort => {
                kill_tree(&mut child);
                let _ = child.wait().await;
                true
            }
            waited = tokio::time::timeout(timeout, child.wait()) => {
                match waited {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        kill_tree(&mut child);
                        progress.abort();
                        logs.abort();
                        return Err(VideoError::runtime(&e));
                    }
                    Err(_) => {
                        // A stalled live capture still yields what it got.
                        kill_tree(&mut child);
                        let _ = child.wait().await;
                    }
                }
                false
            }
        };
        let _ = join_drain(progress).await;
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
            let _ = tokio::fs::remove_dir_all(staging).await;
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
    let final_tmp = staging.join(format!("final.{ext}"));
    if let Err(e) = remux_live_capture(ffmpeg_bin, &src, &final_tmp, job.audio_only, timeout).await
    {
        let _ = tokio::fs::remove_dir_all(staging).await;
        return Err(e);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    // The killed recorder never renames its shell: sweep it now that the
    // remux is claimed, so stopped captures leave no litter beside the
    // finished file. A clean yt-dlp exit renamed it already (no-op).
    let _ = tokio::fs::remove_file(&part).await;
    let _ = tokio::fs::remove_dir_all(staging).await;
    Ok(file_len(&job.dest))
}

/// Spawn a task draining a child process's stderr pipe: complete lines
/// are traced as they arrive (see [`trace_format_lines`]) and the last
/// 8 KiB are kept. Awaiting the returned handle yields the
/// lossy-decoded tail for error detail.
fn drain_stderr_to_tail(stderr: tokio::process::ChildStderr) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail = Vec::new();
        let mut pending = String::new();
        let mut buf = [0u8; 4096];
        loop {
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
    })
}

/// SIGKILL a spawned downloader and the ffmpeg it may have started:
/// both run in a dedicated process group (`process_group(0)` at
/// spawn), so one killpg reaps the tree instead of orphaning ffmpeg
/// mid-merge.
pub(crate) fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Deliberately unconditional: the group outlives its leader by
        // design (ffmpeg grandchildren), so an exited child still leaves
        // a group worth signaling — a try_wait gate here would orphan
        // ffmpeg on every abort-after-exit. The pid-reuse race (recycled
        // pid that is also a group leader) needs churn no desktop hits.
        // SAFETY: constant signal number; ESRCH (raced exit) is harmless.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Join a pipe-drain task with a grace period: a dead child can leave
/// orphaned grandchildren holding the pipes (ffmpeg spawned by yt-dlp),
/// and awaiting them bare would hang forever. Falls back to aborting.
async fn join_drain<T>(task: tokio::task::JoinHandle<T>) -> Option<T> {
    let abort = task.abort_handle();
    tokio::select! {
        biased;
        done = task => done.ok(),
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            abort.abort();
            None
        }
    }
}

/// The finished file of one yt-dlp attempt: the `after_move` path
/// when it landed under staging, else the largest non-temp file.
/// Temp suffixes (parts, metadata sidecars) never qualify.
/// Adopt yt-dlp's finished HLS output: the `--print after_move:filepath`
/// line when trustworthy, else the largest finished `<stem>.hls.*` file
/// beside the destination. The fallback is stem-constrained (never the
/// largest whatever) because the search dir is now the user's folder.
fn discover_ytdlp_output(dest: &Path, after_move: Option<&str>) -> Option<PathBuf> {
    let dir = dest.parent().unwrap_or_else(|| Path::new(""));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{stem}.hls.");
    if let Some(path) = after_move
        && let Ok(canonical) = std::fs::canonicalize(path)
        && canonical.starts_with(dir)
        && canonical.is_file()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
        && name.starts_with(&prefix)
    {
        return Some(canonical);
    }
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with(&prefix)
                        // Subtitle sidecars (`<stem>.hls.<lang>.srt`) share
                        // the prefix but are never the media output.
                        && !matches!(
                            p.extension().and_then(|e| e.to_str()),
                            Some("part" | "ytdl" | "temp" | "tmp" | "frag" | "srt")
                        )
                })
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
async fn run_hls_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    hls_format_id: &str,
    abort: oneshot::Receiver<()>,
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
            kill_tree(&mut child);
            let _ = child.wait().await;
            progress.abort();
            logs.abort();
            let _ = tokio::fs::remove_dir_all(staging).await;
            return Ok(None);
        }
        waited = tokio::time::timeout(timeout, child.wait()) => match waited {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                kill_tree(&mut child);
                progress.abort();
                logs.abort();
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                kill_tree(&mut child);
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
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {}
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
    let _ = tokio::fs::remove_dir_all(staging).await;
    Ok(file_len(&job.dest))
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Scrub tool lookup so video spawns deterministically fail with
    /// MissingLibraries instead of depending on the dev machine (yt-dlp in
    /// PATH would hit the network). Serial suite only: the environment is
    /// process-global (same precedent as GRAB_QUEUE_FILE). Restores on drop.
    pub(crate) struct NoVideoTools {
        path: Option<std::ffi::OsString>,
        xdg: Option<std::ffi::OsString>,
    }

    impl NoVideoTools {
        pub(crate) fn apply() -> Self {
            let path = std::env::var_os("PATH");
            let xdg = std::env::var_os("XDG_DATA_HOME");
            // SAFETY: serial suite (cargo runs with --test-threads=1); no
            // other test reads PATH/XDG while this guard lives.
            unsafe {
                std::env::set_var("PATH", "/nonexistent-grab-test");
                std::env::set_var("XDG_DATA_HOME", "/nonexistent-grab-test-dir");
            }
            Self { path, xdg }
        }
    }

    impl Drop for NoVideoTools {
        fn drop(&mut self) {
            // SAFETY: same serial-suite context as apply().
            unsafe {
                match &self.path {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
                match &self.xdg {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "video_tests.rs"]
mod tests;
