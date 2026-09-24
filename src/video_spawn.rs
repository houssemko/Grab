//! Spawn plumbing + fetch resolve: process-group spawns, pipe
//! drains, tree kills, output discovery and the probe/fetch entry
//! points. Mid-level module (leaves + probe/staging/argv): the
//! dialog and runner consume these through the `video` facade.

use crate::video_argv::{apply_proxy_env, proxy_cli_args};
use crate::video_probe::{
    page_host, parse_playlist_json, parse_single_video, pick_playlist_entry,
    playlist_resolve_error, retarget_story_items, sanitize_video_json,
};
use crate::video_progress::{last_log_line, trace_format_lines};
use crate::video_staging::{ensure_staging_dir, staging_root};
use crate::video_tools::{VideoError, ensure_tool_versions, ytdlp_identity_args};
use crate::video_types::{FetchedVideo, ProbeResult, VideoInfo};
use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::time::Duration;
use yt_dlp::client::deps::Libraries;
use yt_dlp::model::Video;

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
pub(crate) async fn fetch_raw_dump_json(
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
            reap_child(&mut child).await;
            return Err(VideoError::fetch(&e));
        }
        Err(_) => {
            reap_child(&mut child).await;
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
pub(crate) async fn fetch_video_page(
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
pub(crate) fn ytdlp_command(youtube_bin: &Path) -> tokio::process::Command {
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
pub(crate) fn spawn_piped_ytdlp(
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

/// Spawn a task draining a child process's stderr pipe: complete lines
/// are traced as they arrive (see [`trace_format_lines`]) and the last
/// 8 KiB are kept. Awaiting the returned handle yields the
/// lossy-decoded tail for error detail.
pub(crate) fn drain_stderr_to_tail(
    stderr: tokio::process::ChildStderr,
) -> tokio::task::JoinHandle<String> {
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
/// spawn), so one killpg signals the whole group instead of orphaning
/// ffmpeg mid-merge. Only the direct child is waitable — see
/// [`reap_child`], which pairs this with the wait.
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

/// [`kill_tree`], then wait for the child to be reaped.
///
/// `kill_tree` only *signals* the group, and says nothing about the
/// ffmpeg grandchildren in it — only the direct yt-dlp child is reaped
/// here. A signalled child also stays in the process table as a zombie
/// until something waits on it, so "the signal was sent" is not the same
/// as "the recorder is gone". Tokio's orphan queue would reap it
/// eventually; waiting here settles it while the caller is still in a
/// position to act on the answer.
///
/// The wait is deliberately **unbounded**, and that is a real property to
/// be aware of: SIGKILL is pending, not a termination deadline, so a
/// child wedged in uninterruptible I/O (a stuck network write, an fsync
/// against a dead mount) never exits and this blocks. It is kept that way
/// because every caller here either adopts, remuxes, or deletes the
/// scratch this process was writing, and proceeding on a merely
/// signalled child risks a remux reading a file that is still changing.
/// Bounding the wait was tried and reverted: it converted these paths
/// from "wait for the exit" into "proceed after a grace period", which is
/// a data-integrity regression. Fixing the hang properly means deciding
/// what an unconfirmed exit should do — quarantining the attempt so a
/// Retry cannot race the writer — which is a separate design question,
/// and not something to smuggle in as a timeout.
///
/// The wait result is discarded, so a `wait()` that errors leaves the
/// caller with no verdict: the reap is best-effort, and the guarantee
/// callers actually get is "the exit has been waited on", not "the exit
/// was observed to happen". Every caller is on an error or cancel path
/// either way, and propagating the error without a policy for what to do
/// next would be a half-applied contract — the same reasoning that
/// removed an earlier `bool` verdict from this helper.
pub(crate) async fn reap_child(child: &mut tokio::process::Child) {
    kill_tree(child);
    let _ = child.wait().await;
}

/// Join a pipe-drain task with a grace period: a dead child can leave
/// orphaned grandchildren holding the pipes (ffmpeg spawned by yt-dlp),
/// and awaiting them bare would hang forever. Falls back to aborting.
pub(crate) async fn join_drain<T>(task: tokio::task::JoinHandle<T>) -> Option<T> {
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
pub(crate) fn discover_ytdlp_output(dest: &Path, after_move: Option<&str>) -> Option<PathBuf> {
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
