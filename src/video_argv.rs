//! Attempt inputs + argv builders: `VideoJob`, format-spec and
//! container-truth selection, unified/HLS/live argv construction.
//! Mid-level module (leaves + video_plan/staging/prefs/tools): the
//! runner and engine spawn consume these through the `video` facade.

use crate::video_prefs::subtitle_cli_args;
use crate::video_quality::quality_height;
use crate::video_staging::{VideoManifest, dest_part_path, manifest_path, ytdlp_output_template};
use crate::video_tools::{VideoError, ffmpeg_location_dir, ytdlp_identity_args};
use std::path::{Path, PathBuf};

pub(crate) async fn write_manifest(dir: &Path, manifest: &VideoManifest) -> Result<(), VideoError> {
    let text = serde_json::to_string_pretty(manifest).map_err(VideoError::staging)?;
    tokio::fs::write(manifest_path(dir), text)
        .await
        .map_err(VideoError::staging)
}

/// Inputs for one resolver-worker attempt. Built on the main thread in
/// `spawn_video`; everything the pipeline needs, nothing GTK.
#[derive(Debug, Clone)]
pub struct VideoJob {
    pub item_id: u64,
    pub page_url: String,
    /// yt-dlp id of the playlist entry this row was picked from, if any
    /// (see [`VideoChoices::playlist_item_id`](crate::media_types::VideoChoices::playlist_item_id)). The worker selects the
    /// entry when the page re-resolves playlist-shaped.
    pub playlist_item_id: Option<String>,
    pub quality: String,
    pub dest: PathBuf,
    /// Per-download speed cap in bytes/sec (`--ratelimit`), parsed once from
    /// the shared "speed limit" preference. `None` means unlimited (empty,
    /// `0`, or invalid input — the preferences row flags junk live). Live
    /// rows never take it (capping an endless capture would fall behind the
    /// live edge).
    pub speed_limit: Option<u64>,
    /// Stamp the finished file with the server's Last-Modified date
    /// (`--mtime`) instead of the download time. Opt-in preference, shared
    /// with the app's own Last-Modified handling; best-effort like the
    /// rest (fragment/CDN responses often carry no usable header, and the
    /// merge step can reset the stamp on multi-format legs). Live rows
    /// never take it: the live path remuxes through ffmpeg after capture,
    /// which would clobber any mtime yt-dlp set.
    pub keep_server_date: bool,
    /// Dialog-pinned video format id, if the user picked an exact format.
    /// `None` means the quality preset decides at attempt time.
    pub video_format_id: Option<String>,
    /// Whether the page is currently live. A live HLS capture finalizes
    /// and keeps its partial on stop instead of discarding it.
    pub is_live: bool,
    /// Record a live stream from its beginning (`--live-from-start`).
    /// Opt-in preference; only live rows take it (VOD legs have no live
    /// edge to rewind to). No-op on sites without DVR support.
    pub live_from_start: bool,
    /// Newest codecs first (AV1 over AVC1). False prefers compatible
    /// H.264 for players without newer decoders.
    pub newest_codecs: bool,
    /// Dialog audio-only choice for this row (no global default; the
    /// New Download dialog owns it). Skips video selection, merging,
    /// and subtitles; HLS takes its audio rendition, live records m4a.
    pub audio_only: bool,
    /// Audio extraction quality for audio-only rows (`--audio-quality`),
    /// from the "audio quality" preference. 0 is best, 10 is worst; 5 is
    /// yt-dlp's default, so the flag is omitted at 5 unless the user
    /// moves the row. Only audio-only VOD legs take it (there is no
    /// extraction step on live rows — they remux through ffmpeg after
    /// capture).
    pub audio_quality: i32,
    /// Raw browser-auth setting (`none` when off). Resolved to a
    /// `--cookies-from-browser` spec inside the worker.
    pub cookies_browser: String,
    /// Subtitle language code for sidecar downloads (`None` = off).
    /// Only legs carrying video content request subtitles; a missing
    /// language is a yt-dlp warning, never a failure.
    pub subtitles: Option<String>,
    /// Mux downloaded subtitles into the finished file (`--embed-subs`).
    /// Opt-in preference; live rows never take it (live captures record
    /// raw transport streams, no post-processing leg exists).
    pub embed_subs: bool,
    /// Cut SponsorBlock-flagged sponsor segments (`--sponsorblock-remove
    /// sponsor`). Opt-in preference; live rows never take it (the live
    /// edge cannot know future segments).
    pub sponsorblock_remove: bool,
    /// Mark SponsorBlock-flagged sponsor segments as chapters
    /// (`--sponsorblock-mark sponsor`). Opt-in preference; live rows never
    /// take it (the live edge cannot know future segments).
    pub sponsorblock_mark: bool,
    /// Remux the finished file into another container (`--remux-video`).
    /// Opt-in preference; `None` disables it and audio-only rows always
    /// resolve to `None` (no video leg exists). Live rows never take it
    /// (live captures record raw transport streams, no post-processing
    /// leg exists).
    pub remux_video: Option<String>,
    /// Write chapter markers into the finished file (`--embed-chapters`).
    /// Opt-in preference; live rows never take it (same reason as above).
    pub embed_chapters: bool,
    /// Proxy resolved at spawn time (`None` = direct). yt-dlp spawns
    /// take `--proxy` plus NO_PROXY from it.
    pub proxy: Option<crate::net_types::ResolvedProxy>,
}

/// Fallback `-f` spec when the selected id is unknown to yt-dlp's fresh
/// extract (ids can rotate between the dialog resolve and this
/// attempt): same height semantics as the planner, resolved inside the
/// binary. Audio legs stay audio (`ba/b` degrades to best-single only
/// when no audio track exists); adopted single files degrade to
/// best-single instead of drifting into a bare audio track.
pub(crate) fn part_fallback_spec(quality: &str, video_part: bool, prefer_audio: bool) -> String {
    if video_part {
        return match quality_height(quality) {
            Some(h) => format!("bv*[height<={h}]"),
            None => "bv*".to_string(),
        };
    }
    if prefer_audio {
        "ba/b".to_string()
    } else {
        "b".to_string()
    }
}

/// Extractor ids are remote strings: allowlist to selector-safe chars
/// so a hostile id (hand-edited queue file, exotic extractor) can't
/// widen a yt-dlp `-f` set (`,`, `[]`, `()`, `/`, `+` all change set
/// semantics). Shared by the HLS and unified builders; rejection falls
/// back to preset chains, never row failure.
fn selector_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':'))
    .then_some(id)
}

/// Single-invocation `-f` spec for direct (non-HLS, non-live) downloads,
/// plus whether yt-dlp will merge. yt-dlp tries each `/`-separated merge
/// in order: planner pair first, then preset-video with the exact audio,
/// then the full preset pair. The middle option preserves a still-good
/// exact audio pick when only the video id rotated — the old per-leg
/// loop retried legs independently, and degrading both at once would
/// throw away bandwidth already validated. A missing (adopted-single)
/// video id downloads one format; anything failing the id allowlist
/// degrades to preset chains. Pure for tests.
pub(crate) fn unified_format_spec(
    video_id: Option<&str>,
    audio_id: &str,
    quality: &str,
    audio_only: bool,
) -> (String, bool) {
    let vfb = part_fallback_spec(quality, true, false);
    let afb = part_fallback_spec(quality, false, true);
    let aid = selector_id(audio_id);
    if audio_only {
        // Dialog audio-only choice: exact audio track first, audio
        // fallback after; never merges, never drifts into video.
        return match aid {
            Some(a) => (format!("{a}/ba/b"), false),
            None => ("ba/b".to_string(), false),
        };
    }
    match video_id {
        // Adopted single file (muxed direct): exact id first, best-single
        // fallback (never drift into a bare audio track).
        None => match aid {
            Some(a) => (format!("{a}/b"), false),
            None => ("b".to_string(), false),
        },
        // Split: planner pair, then preset-video with the exact audio,
        // then the full preset pair. Anything failing the id allowlist
        // degrades to the preset chain, never row failure.
        Some(raw) => match (selector_id(raw), aid) {
            (Some(vid), Some(a)) => (format!("{vid}+{a}/{vfb}+{a}/{vfb}+{afb}"), true),
            _ => (format!("{vfb}+{afb}"), true),
        },
    }
}

/// Container yt-dlp may merge into: the video ext when supported,
/// mp4 otherwise (a working file beats a failed merge). The `-o`
/// template takes `%(ext)s` from the merge, so the claim never depends
/// on a guessed extension. Pure for tests.
pub(crate) fn merge_output_ext(video_ext: &str) -> String {
    match video_ext.to_ascii_lowercase().as_str() {
        "mp4" | "webm" | "mkv" | "flv" | "ogg" => video_ext.to_ascii_lowercase(),
        _ => "mp4".to_string(),
    }
}

/// Containers the finished-name backstop may correct toward: exactly
/// the set the worker plans or remuxes to. Anything else discovered
/// keeps today's name rather than surprising the row.
const TRUTHFUL_CONTAINERS: &[&str] = &["mp4", "webm", "mkv", "flv", "ogg", "m4a"];

/// Intake-name extensions Grab generates itself absent remux: the only
/// ones the backstop second-guesses. A remux-aware intake can also
/// generate mkv/webm, and a user can type anything — both keep their
/// intent here because neither is distinguishable from an explicit
/// choice, and both are already correct when the pref hasn't changed
/// mid-queue (the case this backstop exists for).
const GENERATED_INTAKE_EXTS: &[&str] = &["mp4", "m4a"];

/// Corrected row name when the discovered container disagrees with the
/// intake name: same stem, discovered extension. `None` when they agree
/// (case-insensitively), when either side lacks an extension, when the
/// intake name carries an explicit (non-generated) extension, or when
/// the discovered one isn't a planned container. Feeds the pump's
/// finished-name adoption via `SuggestName`, which dedupes it with
/// collision safety. Pure for tests.
pub(crate) fn container_truth_name(dest: &Path, discovered: &Path) -> Option<String> {
    let found_ext = discovered.extension().and_then(|e| e.to_str())?;
    let dest_ext = dest.extension().and_then(|e| e.to_str())?;
    if found_ext.eq_ignore_ascii_case(dest_ext) {
        return None;
    }
    if !GENERATED_INTAKE_EXTS
        .iter()
        .any(|e| dest_ext.eq_ignore_ascii_case(e))
    {
        return None;
    }
    if !TRUTHFUL_CONTAINERS
        .iter()
        .any(|e| found_ext.eq_ignore_ascii_case(e))
    {
        return None;
    }
    let stem = dest.file_stem().and_then(|s| s.to_str())?;
    if stem.is_empty() {
        return None;
    }
    Some(format!("{stem}.{}", found_ext.to_ascii_lowercase()))
}

/// Stable `-o` template for the unified download: inside the row's
/// staging dir (wiped wholesale on mismatch/remove/success), so no
/// part-namespace coordination is needed. yt-dlp resumes its own
/// `.part` shell beside it across attempts of the same selection.
pub(crate) fn unified_output_template(staging: &Path) -> PathBuf {
    staging.join("grab-media.%(ext)s")
}

/// Shared argv tail for the VOD download builders ([`unified_download_argv`]
/// and [`hls_download_argv`]): opt-in subtitle sidecars (never on
/// audio-only rows), proxy flags, and browser identity/cookie args.
fn push_vod_tail_args(args: &mut Vec<String>, job: &VideoJob) {
    // No subtitles on audio-only rows (nothing to caption).
    if !job.audio_only
        && let Some(lang) = job.subtitles.as_deref()
    {
        args.extend(subtitle_cli_args(lang));
    }
    args.extend(proxy_cli_args(job.proxy.as_ref()));
    args.extend(ytdlp_identity_args(
        &job.cookies_browser,
        None,
        &job.page_url,
    ));
}

/// yt-dlp argv for one unified direct download: the single `-f` merge
/// spec into a staging temp, merged and converted by yt-dlp itself.
/// Pure for tests like the HLS/live builders (same `--`-before-URL
/// ordering rule).
pub(crate) fn unified_download_argv(
    job: &VideoJob,
    spec: &str,
    merging: bool,
    merge_ext: &str,
    ffmpeg_bin: &Path,
    out: &Path,
) -> Vec<String> {
    let mut args = vec![
        "--ignore-config".to_string(),
        "--no-playlist".to_string(),
        "--newline".to_string(),
        "--progress".to_string(),
        "--progress-template".to_string(),
        YTDLP_PROGRESS_TEMPLATE.to_string(),
        "-f".to_string(),
        spec.to_string(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--ffmpeg-location".to_string(),
        ffmpeg_location_dir(ffmpeg_bin),
        // Fragment downloads stay at yt-dlp's serial default: the app's
        // parallel-connections setting drives only its own segmented
        // HTTP engine, never fragment floods on strict hosts.
        "--print".to_string(),
        "after_move:filepath".to_string(),
    ];
    if let Some(limit) = job.speed_limit {
        // Opt-in throttle: cap this leg at the shared speed limit
        // (parsed once at spawn; empty/0/invalid means unlimited).
        args.push("--ratelimit".to_string());
        args.push(limit.to_string());
    }
    if job.keep_server_date {
        // Opt-in fidelity: stamp the finished file with the server's
        // Last-Modified date instead of the download time (same preference
        // as the app's own Last-Modified handling). `--mtime` is the CLI
        // spelling — the old `--updatetime` name survives only as
        // optparse's dest and is rejected as an unknown option.
        // Best-effort: yt-dlp stamps the downloaded file, so the merge
        // step can reset it on multi-format legs.
        args.push("--mtime".to_string());
    }
    if job.audio_only {
        // Dialog audio-only choice: extract the audio track to m4a
        // (native containers vary by codec — opus arrives as webm — so
        // merging to the .m4a dest name would mislabel bytes).
        args.push("--extract-audio".to_string());
        args.push("--audio-format".to_string());
        args.push("m4a".to_string());
        // Extraction quality from the "audio quality" preference (0 is
        // best, 10 is worst). 5 is yt-dlp's own default, so the flag is
        // a no-op unless the user moves the row — like the other
        // opt-ins, it is omitted at the default.
        if job.audio_quality != 5 {
            args.push("--audio-quality".to_string());
            args.push(job.audio_quality.to_string());
        }
    } else if merging {
        args.push("--merge-output-format".to_string());
        args.push(merge_ext.to_string());
        args.push("--embed-metadata".to_string());
    }
    if job.embed_subs {
        // Opt-in post-processing: mux downloaded subtitle tracks into the
        // finished file. A no-op when subtitle downloads are off.
        args.push("--embed-subs".to_string());
    }
    if job.sponsorblock_remove {
        // Opt-in post-processing: cut community-flagged sponsor segments
        // (SponsorBlock "sponsor" category only; the safe default).
        args.push("--sponsorblock-remove".to_string());
        args.push("sponsor".to_string());
    }
    if job.sponsorblock_mark {
        // Opt-in post-processing: mark community-flagged sponsor segments
        // as chapters (SponsorBlock "sponsor" category only; the safe default).
        args.push("--sponsorblock-mark".to_string());
        args.push("sponsor".to_string());
    }
    if job.embed_chapters {
        // Opt-in post-processing: write chapter markers into the file.
        args.push("--embed-chapters".to_string());
    }
    if let Some(fmt) = job.remux_video.as_deref() {
        // Opt-in post-processing: remux the finished file into another
        // container without re-encoding. Audio-only rows resolve to
        // `None` in download.rs (no video leg exists).
        args.push("--remux-video".to_string());
        args.push(fmt.to_string());
    }
    push_vod_tail_args(&mut args, job);
    args
}

/// yt-dlp `-f` spec for one HLS attempt over the page URL (yt-dlp
/// re-resolves and merges itself). `bv*` (any video, muxed included)
/// leads so direct muxed files win over lower splits; height caps
/// like the picker. A muxed pick may gain a redundant second audio
/// track via `+ba`, which players ignore — completeness beats purity.
pub(crate) fn hls_format_spec(quality: &str, pinned: Option<&str>) -> String {
    // Pinned ids go through the shared selector allowlist (see
    // `selector_id`); anything else falls through to the height rule
    // below instead of failing the row.
    if let Some(id) = pinned.and_then(selector_id) {
        return format!("{id}+ba/b");
    }
    match quality_height(quality) {
        Some(h) => format!("bv*[height<={h}]+ba/b"),
        None => "bv*+ba/b".to_string(),
    }
}

/// yt-dlp argv for a live capture: height-capped (or pinned) format in
/// a kill-safe MPEG-TS container, endless fragment retries bounded by
/// our timeout. No `--live-from-start` (record-now means the live
/// edge; from-start is experimental and YouTube/Twitch-only) and no
/// `--wait-for-video` (an unbounded wait loop is not a download
/// attempt). Pure for tests.
pub(crate) fn live_capture_argv(job: &VideoJob, hls_format_id: &str, out: &Path) -> Vec<String> {
    let mut args = vec![
        "--ignore-config".to_string(),
        "--no-playlist".to_string(),
        "--newline".to_string(),
        "--progress".to_string(),
        "--progress-template".to_string(),
        YTDLP_PROGRESS_TEMPLATE.to_string(),
        "-f".to_string(),
        hls_format_spec(&job.quality, Some(hls_format_id)),
        "--hls-use-mpegts".to_string(),
        "--fragment-retries".to_string(),
        "infinite".to_string(),
        "-o".to_string(),
        ytdlp_output_template(out),
    ];
    if job.is_live && job.live_from_start {
        // Opt-in live capture: record from the beginning of the stream
        // where the site supports it (DVR), instead of the live edge.
        // The is_live gate keeps the builder self-consistent even if a
        // future caller misroutes a VOD row here.
        args.push("--live-from-start".to_string());
    }
    args.extend(proxy_cli_args(job.proxy.as_ref()));
    args.extend(ytdlp_identity_args(
        &job.cookies_browser,
        None,
        &job.page_url,
    ));
    args
}

/// ffmpeg argv remuxing a stopped live capture (MPEG-TS bytes, possibly
/// still in the `.part` shell) into the finished file: stream-copy with
/// faststart for progressive playback. Pure for tests.
pub(crate) fn live_remux_argv(
    ts_path: &Path,
    dest: &Path,
    audio_only: bool,
    with_bsf: bool,
) -> Vec<String> {
    let mut argv = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostats".to_string(),
        "-i".to_string(),
        ts_path.to_string_lossy().into_owned(),
    ];
    if audio_only {
        argv.extend(["-map".to_string(), "0:a?".to_string()]);
    } else {
        argv.extend(["-map".to_string(), "0".to_string()]);
    }
    argv.extend([
        "-dn".to_string(),
        "-ignore_unknown".to_string(),
        "-c".to_string(),
        "copy".to_string(),
    ]);
    // TS almost always carries ADTS AAC, which MP4/M4A containers
    // reject without the fixup (mirroring yt-dlp's own FixupM3u8
    // line); anything else remuxes on the bare retry.
    if with_bsf {
        argv.extend(["-bsf:a".to_string(), "aac_adtstoasc".to_string()]);
    }
    argv.extend([
        "-movflags".to_string(),
        "+faststart".to_string(),
        "--".to_string(),
        dest.to_string_lossy().into_owned(),
    ]);
    argv
}

/// yt-dlp's exact stderr line when `--live-from-start` meets a stream
/// Pure decision for the live-edge fallback: a from-start attempt that
/// recorded nothing and wasn't stopped retries once from the live edge
/// instead of failing the row. Anything else — a mid-capture failure, a
/// stopped attempt, or the second attempt itself — keeps its outcome, so
/// partial recordings are never discarded and Stop is never overridden.
/// Pure for tests.
pub(crate) fn fallback_to_live_edge(
    is_live: bool,
    live_from_start: bool,
    aborted: bool,
    already_retried: bool,
) -> bool {
    is_live && live_from_start && !aborted && !already_retried
}

/// One VOD HLS attempt through the yt-dlp binary: native fragment
/// handling (retries, parallel fragments, keys) plus merging and
/// audio extraction, with Grab parsing `--newline` progress. No
/// resume sidecar of its own — `.part` files in staging resume across
/// attempts instead. Returns the final size, or `None` when aborted.
#[allow(clippy::too_many_arguments)]
/// yt-dlp argv for one VOD HLS capture: the planner-pinned variant id
/// (never re-delegated to yt-dlp's sort, whose ie_pref/quality/source
/// tiebreaks can shadow height), merge/extract post-processing, then
/// proxy/identity. Pure for tests like the part/live builders (same
/// `--`-before-URL ordering rule).
pub(crate) fn hls_download_argv(
    job: &VideoJob,
    hls_format_id: &str,
    ffmpeg_bin: &Path,
    dest: &Path,
) -> Vec<String> {
    let out_template = dest_part_path(dest, "hls", "%(ext)s");
    let mut args = vec![
        "--ignore-config".to_string(),
        "--no-playlist".to_string(),
        "--newline".to_string(),
        "--progress".to_string(),
        "--progress-template".to_string(),
        YTDLP_PROGRESS_TEMPLATE.to_string(),
        "-f".to_string(),
        hls_format_spec(&job.quality, Some(hls_format_id)),
        "-o".to_string(),
        ytdlp_output_template(&out_template),
        "--ffmpeg-location".to_string(),
        ffmpeg_location_dir(ffmpeg_bin),
        // Serial fragments like every other yt-dlp leg (see the unified
        // builder): the connections setting is the app engine's own.
        "--print".to_string(),
        "after_move:filepath".to_string(),
    ];
    if let Some(limit) = job.speed_limit {
        // Same opt-in throttle as the unified VOD legs.
        args.push("--ratelimit".to_string());
        args.push(limit.to_string());
    }
    if job.keep_server_date {
        // Same opt-in Last-Modified stamping as the unified VOD legs
        // (best-effort: the merge step can reset the stamp).
        args.push("--mtime".to_string());
    }
    if job.audio_only {
        args.push("--extract-audio".to_string());
        args.push("--audio-format".to_string());
        args.push("m4a".to_string());
        // Same extraction quality as the unified audio-only legs;
        // omitted at yt-dlp's own default of 5.
        if job.audio_quality != 5 {
            args.push("--audio-quality".to_string());
            args.push(job.audio_quality.to_string());
        }
    } else {
        args.push("--merge-output-format".to_string());
        args.push("mp4".to_string());
    }
    if job.embed_subs {
        // Same opt-in post-processing as the unified VOD legs.
        args.push("--embed-subs".to_string());
    }
    if job.sponsorblock_remove {
        // Same opt-in post-processing as the unified VOD legs.
        args.push("--sponsorblock-remove".to_string());
        args.push("sponsor".to_string());
    }
    if job.sponsorblock_mark {
        // Same opt-in post-processing as the unified VOD legs.
        args.push("--sponsorblock-mark".to_string());
        args.push("sponsor".to_string());
    }
    if job.embed_chapters {
        // Same opt-in post-processing as the unified VOD legs.
        args.push("--embed-chapters".to_string());
    }
    if let Some(fmt) = job.remux_video.as_deref() {
        // Same opt-in post-processing as the unified VOD legs.
        args.push("--remux-video".to_string());
        args.push(fmt.to_string());
    }
    // Sidecar subtitles for HLS VOD rows (never audio-only; live rows
    // never reach this builder — they run through `live_capture_argv`,
    // which deliberately omits subtitles).
    push_vod_tail_args(&mut args, job);
    args
}
/// `--proxy` argv for one yt-dlp spawn. Empty when direct. Proxies are
/// unauthenticated (see `manual_proxy`): the URL never carries userinfo.
pub(crate) fn proxy_cli_args(proxy: Option<&crate::net_types::ResolvedProxy>) -> Vec<String> {
    match proxy.map(|p| p.cli_url.clone()) {
        Some(url) => vec!["--proxy".to_string(), url],
        None => Vec::new(),
    }
}

/// NO_PROXY env for one yt-dlp spawn. Set only when proxied; yt-dlp
/// honors it on a best-effort basis for the bypass list.
pub(crate) fn apply_proxy_env(
    cmd: &mut tokio::process::Command,
    proxy: Option<&crate::net_types::ResolvedProxy>,
) {
    if let Some(p) = proxy
        && !p.no_proxy_env.is_empty()
    {
        cmd.env("NO_PROXY", &p.no_proxy_env)
            .env("no_proxy", &p.no_proxy_env);
    }
}
/// Machine-readable progress lines: `--progress-template` with stable
/// fields beats parsing human `[download]` prose (percent formats and
/// unit spellings drift between versions). `[Grab];`-prefixed so
/// `parse_ytdlp_after_move` (bare absolute paths only) never mistakes
/// one for a filepath.
pub(crate) const YTDLP_PROGRESS_TEMPLATE: &str = "[Grab];%(progress.status)s;%(progress.downloaded_bytes)s;%(progress.total_bytes)s;%(progress.total_bytes_estimate)s;%(progress.speed)s;%(progress.eta)s";
