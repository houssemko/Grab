//! HTTP fetch engine: attempt loops, range probing, piece fetching,
//! resume bitmaps and paced single/multi-connection downloads.
//! Mid-level module (all leaves + settings/cookies, one-way edge into
//! `torrent` for live-limit re-apply only): the download manager
//! spawns `run_download`; tests drive the pieces directly.

use crate::download_net::{DEFAULT_USER_AGENT, DownloadOptions};
use crate::download_pieces::{SegmentState, plan_pieces};
use crate::download_rate::{live_rate_limit, pace_chunk, parse_rate, progress_msg};
use crate::engine_msg::{DEST_EXISTS, EngineMsg};
use crate::file_names::{PIECE_MIN, percent_decode, piece_len, sane_filename};
use crate::runtime::lock_recover;
use futures_util::StreamExt as _;
use gettextrs::gettext;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Forward a parseable Last-Modified response header to the pump. Torrent
/// engines never call this; HTTP attempts send at most one per response.
fn send_last_modified(
    tx: &tokio::sync::mpsc::UnboundedSender<EngineMsg>,
    resp: &reqwest::Response,
) {
    if let Some(v) = resp.headers().get(reqwest::header::LAST_MODIFIED)
        && let Ok(s) = v.to_str()
        && let Ok(t) = httpdate::parse_http_date(s)
    {
        tx.send(EngineMsg::LastModified(t)).ok();
    }
}

/// First recorded worker error, or a generic interruption message when
/// no worker recorded one. Callers wrap it in their own `AttemptFail`
/// variant (changed vs throttled vs retryable drive different recovery),
/// so the helper returns the message and callers keep their variants.
fn take_first_err(first_err: &Mutex<Option<String>>) -> String {
    lock_recover(first_err)
        .take()
        .unwrap_or_else(|| gettext("Download interrupted"))
}

/// Shared inputs for one download's engine task. Groups the params every
/// attempt function needs instead of threading eight loose arguments.
pub(crate) struct FetchCtx {
    pub(crate) client: reqwest::Client,
    pub(crate) url: String,
    pub(crate) dest: std::path::PathBuf,
    pub(crate) opts: DownloadOptions,
    /// Attempt-scoped browser cookie jar (`None` = setting off or
    /// export unavailable). Resolved once per attempt, not per request.
    pub(crate) cookies: Option<std::sync::Arc<reqwest::cookie::Jar>>,
    pub(crate) timeout: Duration,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
}

pub(crate) async fn run_download(mut ctx: FetchCtx, connections: usize, mode: StartMode) {
    let timeout = Duration::from_secs(30);
    let mut tries = 3;
    // One export per attempt: the browser profile may have changed
    // since the last run, and jars are cheap next to downloads.
    if !ctx.opts.cookies_browser.is_empty() && ctx.opts.cookies_browser != "none" {
        let youtube_bin = crate::video::resolve_libraries()
            .map(|libs| libs.youtube)
            .ok();
        if let Some(bin) = youtube_bin {
            ctx.cookies =
                crate::cookies::jar_for_browser(&ctx.opts.cookies_browser, &bin, &ctx.url).await;
        }
    }
    match mode {
        StartMode::Single => {
            single_loop(&ctx, &mut tries, None, false).await;
        }
        StartMode::Fresh => {
            // Single connection skips probing outright: the probe's extra
            // request consumes single-use token URLs, and one stream
            // needs no total or range proof up front (completion is EOF,
            // progress indeterminate).
            if connections <= 1 {
                single_loop(&ctx, &mut tries, None, true).await;
                return;
            }
            match probe_ranges(&ctx.client, &ctx.url, ctx.cookies.as_ref(), timeout).await {
                Ok(total) => {
                    if plan_pieces(total, connections).is_empty() {
                        single_loop(&ctx, &mut tries, Some(total), true).await;
                    } else {
                        ctx.tx.send(EngineMsg::SegmentsInit { total }).ok();
                        if multi_loop(&ctx, total, None, connections, &mut tries).await {
                            let mut single_tries = 3;
                            single_loop(&ctx, &mut single_tries, Some(total), false).await;
                        }
                    }
                }
                Err(_) => {
                    single_loop(&ctx, &mut tries, None, true).await;
                }
            }
        }
        StartMode::Resume(st) => {
            let total = st.total;
            if multi_loop(&ctx, total, Some(st), connections, &mut tries).await {
                let mut single_tries = 3;
                single_loop(&ctx, &mut single_tries, Some(total), false).await;
            }
        }
    }
}

/// Generous single-stream fallback with retries. `expected` is the probed
/// total when one is known, so a restarted 200 with a disagreeing length
/// is rejected instead of clobbering good bytes. `claim` marks the initial
/// fresh attempt (foreign file at dest fails fast); fallback singles pass
/// false since the prefix is ours.
async fn single_loop(ctx: &FetchCtx, tries: &mut i32, expected: Option<u64>, claim: bool) {
    // One-shot: only the first attempt may claim a missing file. Past
    // it, any bytes at dest are ours (or a sanctioned restart), so later
    // attempts keep truncate semantics.
    let mut claim = claim;
    loop {
        match attempt_once(ctx, expected, claim).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                ctx.tx.send(EngineMsg::Finished { size }).ok();
                return;
            }
            Err(e) => {
                // Retrying a taken path is futile (same dest): fail at
                // once so the pump can requeue under a fresh name.
                if e == DEST_EXISTS {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return;
                }
                claim = false;
                *tries -= 1;
                if *tries <= 0 {
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return;
                }
            }
        }
    }
}

/// Retry loop around segmented attempts. The resume bitmap lives in the
/// manager (fed by PieceDone), so each retry transparently refetches only
/// the still-missing pieces. On terminal failure the file is first shrunk
/// to the completed prefix, keeping any later single-stream resume correct.
/// Returns true when the server throttled parallel connections: the caller
/// continues single-stream after the UI thread shrinks the file to the
/// completed prefix and drops the bitmap (see FallbackSingle).
async fn multi_loop(
    ctx: &FetchCtx,
    total: u64,
    saved: Option<SegmentState>,
    max_workers: usize,
    tries: &mut i32,
) -> bool {
    // Working bitmap: attempts fold completed pieces into it, so each
    // retry refetches only still-missing ranges instead of everything.
    let mut st = saved.unwrap_or_else(|| SegmentState::new(total));
    loop {
        match attempt_multi(ctx, total, &mut st, max_workers).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                ctx.tx.send(EngineMsg::Finished { size }).ok();
                return false;
            }
            Err(AttemptFail::Throttled(_)) => {
                let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<()>(1);
                ctx.tx.send(EngineMsg::FallbackSingle { ack: ack_tx }).ok();
                // Wait until the UI thread truncated + dropped the bitmap:
                // starting single-stream any earlier could append over holes.
                // If we get aborted here (pause/cancel), there is nothing to do.
                let _ = ack_rx.recv().await;
                return true;
            }
            Err(AttemptFail::Changed(e)) => {
                // Different object than probed: retrying these ranges can
                // never succeed, so fail terminally without burning tries.
                // The pump drops the dead bitmap; the next retry (or Retry
                // button) starts fresh and re-probes the new file.
                ctx.tx.send(EngineMsg::FailedVersion(e)).ok();
                return false;
            }
            Err(AttemptFail::Retryable(e)) => {
                *tries -= 1;
                if *tries <= 0 {
                    // Same channel, FIFO per sender: truncation lands first.
                    ctx.tx.send(EngineMsg::TruncatePrefix).ok();
                    ctx.tx.send(EngineMsg::Failed(e)).ok();
                    return false;
                }
            }
        }
    }
}

/// One segmented attempt: N workers pull 1 MB pieces off a shared queue
/// (fast connections steal more work, bounding straggler damage) while one
/// writer task sequences everything to disk. All futures run inside this
/// one task, so aborting the supervisor JoinHandle stops the whole attempt.
async fn attempt_multi(
    ctx: &FetchCtx,
    total: u64,
    st: &mut SegmentState,
    max_workers: usize,
) -> Result<(), AttemptFail> {
    if st.total != total {
        return Err(AttemptFail::Retryable(gettext("File changed on server")));
    }
    let missing: Vec<(u64, u64, u64)> = st.missing();
    if missing.is_empty() {
        return Ok(());
    }
    let expect_bytes: u64 = missing.iter().map(|(_, s, e)| e - s + 1).sum();
    // Ensure the file exists at full size so workers can write at offsets.
    // Missing pieces stay sparse until fetched; resume always re-runs this.
    // Never truncate here: completed pieces are already on disk.
    ensure_sized(&ctx.dest, total).await?;
    let queue: Arc<Mutex<VecDeque<(u64, u64, u64)>>> =
        Arc::new(Mutex::new(missing.into_iter().collect()));
    let failed = Arc::new(AtomicBool::new(false));
    let first_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let throttled = Arc::new(AtomicBool::new(false));
    let changed = Arc::new(AtomicBool::new(false));
    // Bound in-flight BYTES, not pieces: big pieces would otherwise hold
    // hundreds of MB between fetchers and writer on a slow disk.
    let depth = ((8 * PIECE_MIN) / piece_len(total)).clamp(2, 8) as usize;
    let (wtx, wrx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>, u64)>(depth);
    // Never exceed the configured connections: extra range requests are
    // what throttling hosts punish.
    let n_workers = lock_recover(&queue)
        .len()
        .min(max_workers.max(1))
        .clamp(1, 16);
    let mut workers = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let (ctx, wtx, queue, failed, first_err, throttled, changed) = (
            ctx,
            wtx.clone(),
            Arc::clone(&queue),
            Arc::clone(&failed),
            Arc::clone(&first_err),
            Arc::clone(&throttled),
            Arc::clone(&changed),
        );
        workers.push(async move {
            loop {
                if failed.load(Ordering::SeqCst) {
                    break;
                }
                let piece = lock_recover(&queue).pop_front();
                let Some((idx, s, e)) = piece else { break };
                match fetch_piece(ctx, s, e, total).await {
                    Ok(bytes) => {
                        if wtx.send((s, bytes, idx)).await.is_err() {
                            break; // Writer gone; attempt is over.
                        }
                    }
                    Err(AttemptFail::Throttled(msg)) => {
                        lock_recover(&first_err).get_or_insert(msg);
                        throttled.store(true, Ordering::SeqCst);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(AttemptFail::Changed(msg)) => {
                        lock_recover(&first_err).get_or_insert(msg);
                        changed.store(true, Ordering::SeqCst);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(AttemptFail::Retryable(msg)) => {
                        lock_recover(&first_err).get_or_insert(msg);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
    drop(wtx);
    let writer = {
        let mut wrx = wrx;
        let dest = ctx.dest.clone();
        // Fold completed pieces into the caller's bitmap as they land, so
        // a retry refetches only still-missing ranges instead of everything.
        let st = &mut *st;
        async move {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            let mut downloaded = st.completed_bytes();
            ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
            let pace_start = Instant::now();
            let mut paced: u64 = 0;
            let mut last_sent = Instant::now();
            let mut written: u64 = 0;
            let mut rate = live_rate_limit();
            use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
            while let Some((offset, bytes, idx)) = wrx.recv().await {
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|e| format!("Cannot write file: {e}"))?;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| format!("Cannot write file: {e}"))?;
                downloaded += bytes.len() as u64;
                written += bytes.len() as u64;
                ctx.tx.send(EngineMsg::PieceDone(idx)).ok();
                st.mark(idx);
                pace_chunk(&mut paced, pace_start, rate, bytes.len()).await;
                // ~20fps row updates: smooth determinate motion per HIG;
                // each tick is one label render on a handful of rows.
                if last_sent.elapsed() >= Duration::from_millis(50) {
                    rate = live_rate_limit();
                    ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
                    last_sent = Instant::now();
                }
            }
            file.flush()
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            ctx.tx.send(progress_msg(downloaded, Some(total))).ok();
            Ok::<u64, String>(written)
        }
    };
    let (wres, _) = tokio::join!(writer, futures_util::future::join_all(workers));
    let written = wres.map_err(AttemptFail::Retryable)?;
    if changed.load(Ordering::SeqCst) {
        return Err(AttemptFail::Changed(take_first_err(&first_err)));
    }
    if throttled.load(Ordering::SeqCst) {
        return Err(AttemptFail::Throttled(take_first_err(&first_err)));
    }
    if failed.load(Ordering::SeqCst) {
        return Err(AttemptFail::Retryable(take_first_err(&first_err)));
    }
    if written != expect_bytes {
        return Err(AttemptFail::Retryable(gettext("Incomplete download")));
    }
    Ok(())
}

/// True when the file has unallocated (sparse) regions. Parallel writes can
/// leave holes that a later append-at-EOF resume must not inherit: fully
/// written files always satisfy `blocks * 512 >= len`, so a shortfall proves
/// holes. Needs no new dependency (std `MetadataExt` only). On exotic
/// filesystems with unreliable block counts this degrades to "holes", i.e.
/// a safe full re-download, never silent corruption.
pub(crate) fn has_holes(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.blocks().saturating_mul(512) < m.len()
        })
        .unwrap_or(false)
}

async fn attempt_once(
    ctx: &FetchCtx,
    expected_total: Option<u64>,
    // True only for the first attempt of an initial fresh single-stream
    // run: a foreign file at `dest` must fail instead of being
    // truncated. Retries (our own bytes), 416 restarts (file removed
    // first), and fallback singles (our own prefix) pass false.
    claim: bool,
) -> Result<(), String> {
    // At most one restart: a 416 may only trigger a single delete-and-retry.
    let mut restarted = false;
    loop {
        let start = tokio::fs::metadata(&ctx.dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut req = stamp_request(
            ctx.client.get(&ctx.url),
            DEFAULT_USER_AGENT,
            ctx.cookies.as_ref(),
            &ctx.url,
        );
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
        }
        let resp = execute_with_timeout(&ctx.client, req, ctx.timeout).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The range is past EOF. That means "already complete" ONLY with
            // proof: our length matches the server total AND every byte is
            // really allocated. A SIGKILLed segmented download leaves a sparse
            // full-size file that must never take this shortcut.
            let claimed = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split('/').next_back())
                .and_then(|t| t.parse::<u64>().ok());
            // NOTE: no server round trip can rescue this branch. A length
            // match proves nothing about our bytes when the file has holes:
            // a sparse full-size file would be marked Done with zeros where
            // pieces are missing. Only fully allocated files take it.
            // (On compressed/deduped filesystems the block heuristic can
            // false-positive; that only costs a re-fetch, never corruption.)
            if start > 0 && claimed == Some(start) && !has_holes(&ctx.dest) {
                return Ok(());
            }
            if claimed.is_none() && start > 0 && !restarted {
                // Bare 416 (no usable Content-Range): ask for the length
                // directly before deleting anything that might be complete.
                let hreq = stamp_request(
                    ctx.client.head(&ctx.url),
                    DEFAULT_USER_AGENT,
                    ctx.cookies.as_ref(),
                    &ctx.url,
                );
                if let Ok(hresp) = execute_with_timeout(&ctx.client, hreq, ctx.timeout).await
                    && hresp.status().is_success()
                    && hresp.content_length() == Some(start)
                    && !has_holes(&ctx.dest)
                {
                    return Ok(());
                }
            }
            if restarted {
                // Even a plain GET gets 416: pathological server, stop looping.
                return Err(gettext("Server rejects range requests"));
            }
            let _ = std::fs::remove_file(&ctx.dest);
            restarted = true;
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("error")
            ));
        }
        send_last_modified(&ctx.tx, &resp);
        let partial = start > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        // Fresh runs must never truncate a file they did not create.
        if claim && start > 0 && !partial {
            return Err(DEST_EXISTS.to_string());
        }
        let total = response_total(
            resp.content_length(),
            resp.headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            partial,
            start,
        );
        if rejects_unexpected_restart(partial, start, expected_total, resp.content_length()) {
            return Err(gettext("Server returned an unexpected file size"));
        }
        // Unprobed resume the server answers from zero with a SMALLER object
        // than what we hold: a different file (login wall, throttle page),
        // not our download. Fail loudly and keep the partial bytes instead
        // of truncating them away for it.
        if !partial
            && start > 0
            && let Some(l) = resp.content_length()
            && l < start
        {
            return Err(gettext("Server restarted the download with a smaller file"));
        }
        let mut file = if partial {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&ctx.dest)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?
        } else if claim {
            // One-shot claim, same ownership as the guard above: only the
            // first attempt of an initial fresh run may fail on a foreign
            // file. Retries (claim=false) truncate our own empty file.
            // A file appearing after the metadata check is foreign, so
            // fail for requeue instead of truncating it.
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&ctx.dest)
                .await
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(DEST_EXISTS.to_string());
                }
                Err(e) => return Err(format!("Cannot write file: {e}")),
            }
        } else {
            tokio::fs::File::create(&ctx.dest)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?
        };
        if !partial
            && let Some(name) = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(filename_from_content_disposition)
        {
            ctx.tx.send(EngineMsg::SuggestName(name)).ok();
        }
        let mut downloaded = if partial { start } else { 0 };
        ctx.tx.send(progress_msg(downloaded, total)).ok();
        let mut stream = resp.bytes_stream();
        let pace_start = Instant::now();
        let mut paced: u64 = 0;
        let mut last_sent = Instant::now();
        let mut rate = live_rate_limit();
        // Chunked terminators surface as stream end, never as empty chunks:
        // a run of them is a stalled connection gaming the per-chunk
        // timeout, not data.
        let mut empty_streak: u32 = 0;
        use tokio::io::AsyncWriteExt as _;
        loop {
            let chunk = match tokio::time::timeout(ctx.timeout, stream.next()).await {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(format!("Download interrupted: {e}")),
                Ok(None) => break,
                Err(_) => return Err(gettext("Stalled connection timed out")),
            };
            if chunk.is_empty() {
                empty_streak += 1;
                if empty_streak > 32 {
                    return Err(gettext("Stalled connection timed out"));
                }
                continue;
            }
            empty_streak = 0;
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            downloaded += chunk.len() as u64;
            pace_chunk(&mut paced, pace_start, rate, chunk.len()).await;
            // ~20fps row updates: smooth determinate motion per HIG;
            // each tick is one label render on a handful of rows.
            if last_sent.elapsed() >= Duration::from_millis(50) {
                rate = live_rate_limit();
                ctx.tx.send(progress_msg(downloaded, total)).ok();
                last_sent = Instant::now();
            }
        }
        file.flush()
            .await
            .map_err(|e| format!("Cannot write file: {e}"))?;
        return match total {
            Some(t) if downloaded != t => Err(gettext("Incomplete download")),
            _ => Ok(()),
        };
    }
}

/// Re-apply both torrent speed caps from settings. Both the download and
/// upload watchers call this so editing one key can never silently clear
/// the other.
pub(crate) fn apply_torrent_limits(s: &crate::settings::AppSettings) {
    crate::torrent::apply_live_limits(
        parse_rate(s.speed_limit().trim()),
        parse_rate(s.torrent_upload_limit().trim()),
    );
}

/// Shrink `path` to the longest completed piece prefix. Parallel writes can
/// leave holes; a later single-stream resume appends at EOF, which is only
/// correct on a contiguous prefix. Only ever shrinks.
pub(crate) fn truncate_to_prefix(path: &std::path::Path, st: &SegmentState) {
    let prefix = st.prefix_len();
    if let Ok(md) = std::fs::metadata(path)
        && md.len() > prefix
        && let Ok(f) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let _ = f.set_len(prefix);
    }
}

/// Open for writing (creating), sizing to `total` only when the size
/// differs. `File::create` would truncate already-downloaded pieces on
/// every resume/retry and silently corrupt the file while the bitmap still
/// claims those pieces as done.
async fn ensure_sized(dest: &std::path::Path, total: u64) -> Result<(), AttemptFail> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest)
        .await
        .map_err(|e| AttemptFail::Retryable(format!("Cannot write file: {e}")))?;
    if file.metadata().await.map(|m| m.len()).unwrap_or(u64::MAX) != total
        && let Err(e) = file.set_len(total).await
    {
        use std::io::ErrorKind::{FileTooLarge, StorageFull};
        // No room (or no sparse support) for full-size staging: the
        // single-stream path preallocates nothing, so downgrade to it.
        let msg = format!("Cannot write file: {e}");
        let storage = matches!(e.kind(), StorageFull | FileTooLarge);
        return Err(if storage {
            AttemptFail::Throttled(msg)
        } else {
            AttemptFail::Retryable(msg)
        });
    }
    Ok(())
}

/// How the engine task should start this download.
#[derive(Debug)]
pub(crate) enum StartMode {
    /// Classic single stream (small/unknown size, no range support, or a
    /// contiguous partial file from a non-segmented session).
    Single,
    /// Fresh file: probe range support, then go multi or single.
    Fresh,
    /// Segmented resume from the in-memory bitmap (holes possible on disk).
    Resume(SegmentState),
}

/// One `Range: bytes=0-0` round trip: proves range support AND yields the
/// total (`Accept-Ranges` headers alone are unreliable). Any error means
/// "fall back to single-stream", never a user-facing failure.
/// Stamp one outbound request like the browser would: configured
/// user agent plus this attempt's exported browser cookies, if any.
/// Single choke point so a header can never reach some requests only.
pub(crate) fn stamp_request(
    mut req: reqwest::RequestBuilder,
    user_agent: &str,
    cookies: Option<&std::sync::Arc<reqwest::cookie::Jar>>,
    url: &str,
) -> reqwest::RequestBuilder {
    if !user_agent.trim().is_empty() {
        req = req.header("User-Agent", user_agent.trim());
    }
    if let Some(jar) = cookies
        && let Some(cookie) = crate::cookies::cookie_header_for(jar, url)
    {
        req = req.header("Cookie", cookie);
    }
    // Self-origin Referer: hotlink guards commonly accept the file's
    // own origin (browsers always send *some* referrer context, we have
    // none for pasted URLs). Reveals nothing the request doesn't
    // already carry; page-specific allowlists still refuse, honestly.
    if let Ok(parsed) = url.parse::<url::Url>()
        && let Some(host) = parsed.host_str()
    {
        let mut origin = format!("{}://{host}", parsed.scheme());
        if let Some(port) = parsed.port() {
            origin.push_str(&format!(":{port}"));
        }
        req = req.header("Referer", origin);
    }
    req
}

/// Execute one request under a timeout. Timeouts surface the same
/// "Connection timed out" message at every call site so rows report one
/// consistent stall string; build and transport errors keep their own
/// text for the callers to wrap (`Err(String)` vs `Retryable`).
async fn execute_with_timeout(
    client: &reqwest::Client,
    req: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response, String> {
    let built = req.build().map_err(|e| e.to_string())?;
    match tokio::time::timeout(timeout, client.execute(built)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(gettext("Connection timed out")),
    }
}

async fn probe_ranges(
    client: &reqwest::Client,
    url: &str,
    cookies: Option<&std::sync::Arc<reqwest::cookie::Jar>>,
    timeout: Duration,
) -> Result<u64, String> {
    let req = stamp_request(
        client.get(url).header("Range", "bytes=0-0"),
        DEFAULT_USER_AGENT,
        cookies,
        url,
    );
    let resp = execute_with_timeout(client, req, timeout).await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(gettext("Range requests not supported"));
    }
    let value = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    parse_content_range(&value)
        .filter(|(s, _, _)| *s == 0)
        .map(|(_, _, t)| t)
        .ok_or_else(|| gettext("Bad Content-Range"))
}

/// Parse `Content-Range: bytes <start>-<end>/<total>`. Callers pin the
/// fields they require: a wrong start or end means the server answered a
/// different range than asked (pins all chunks to one file version).
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        total.parse::<u64>().ok()?,
    );
    (total > 0 && end >= start).then_some((start, end, total))
}

/// Total size for a response: Content-Length, else the Content-Range total
/// for partial responses that omit it (chunked 206s). `None` only when
/// neither header says (fresh chunked 200s), where EOF is the only signal.
pub(crate) fn response_total(
    content_length: Option<u64>,
    content_range: Option<&str>,
    partial: bool,
    start: u64,
) -> Option<u64> {
    content_length
        .map(|t| if partial { t + start } else { t })
        .or_else(|| {
            if !partial {
                return None;
            }
            content_range
                .and_then(parse_content_range)
                .filter(|(s, _, _)| *s == start)
                .map(|(_, _, t)| t)
        })
}

/// A resumed range answered with a full 200 must still be the same object:
/// a disagreeing declared length means login wall or throttle page, and
/// `File::create` must not eat the good prefix for it.
pub(crate) fn rejects_unexpected_restart(
    partial: bool,
    start: u64,
    expected: Option<u64>,
    content_length: Option<u64>,
) -> bool {
    !partial && start > 0 && matches!((expected, content_length), (Some(t), Some(l)) if l != t)
}

/// Why a piece or segmented attempt failed. Throttled means the server is
/// rejecting parallel range requests (per-IP connection limits, common on
/// file hosts) while a single stream still works: downgrade, don't retry.
/// Changed means the object on the server is no longer the probed file:
/// retrying the same ranges can never succeed, so fail terminally and let
/// a later retry start fresh instead of looping forever.
pub(crate) enum AttemptFail {
    Retryable(String),
    Throttled(String),
    Changed(String),
}

/// Fetch one `[start, end]` piece, retrying stalls. Verifies the server
/// still serves the probed file version via the Content-Range total.
/// Never offers server-advertised filenames: the engine writes segmented
/// data through `ctx.dest`, so a mid-download rename would desync the
/// running attempt (which keeps writing the old path) from the queue
/// (which records the new name). Only the single-stream path suggests names.
pub(crate) async fn fetch_piece(
    ctx: &FetchCtx,
    start: u64,
    end: u64,
    total: u64,
) -> Result<Vec<u8>, AttemptFail> {
    let timeout = ctx.timeout;
    use AttemptFail::{Changed, Retryable, Throttled};
    let mut last_err = Retryable(gettext("Empty response"));
    for _ in 0..PIECE_TRIES {
        let req = stamp_request(
            ctx.client
                .get(&ctx.url)
                .header("Range", format!("bytes={start}-{end}")),
            DEFAULT_USER_AGENT,
            ctx.cookies.as_ref(),
            &ctx.url,
        );
        let resp = match execute_with_timeout(&ctx.client, req, timeout).await {
            Ok(r) => r,
            Err(e) => {
                last_err = Retryable(e);
                continue;
            }
        };
        send_last_modified(&ctx.tx, &resp);
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            let code = resp.status().as_u16();
            // Per-IP connection limits speak 403/429/503 (and 509 on some
            // hosts) while a single stream still works: downgrade at once
            // instead of burning retries (and goodwill) against the limit.
            if matches!(code, 403 | 429 | 503) || code == 509 {
                return Err(Throttled(format!("Range rejected: HTTP {code}")));
            }
            last_err = Retryable(format!("Range rejected: HTTP {code}"));
            continue;
        }
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        match parse_content_range(&cr) {
            Some((s, e, t)) if s == start && e == end && t == total => {}
            Some((_, _, t)) if t != total => {
                // Different object than probed: retrying these ranges can
                // never succeed, so fail terminally right away.
                return Err(Changed(gettext("File changed on server")));
            }
            // Unparseable or wrong range: the host ignores ranges, so
            // downgrade to single-stream instead of retrying to Failed.
            _ => {
                return Err(Throttled(gettext("Server ignored range request")));
            }
        }
        let mut body = Vec::with_capacity((end - start + 1).min(2 * piece_len(total)) as usize);
        let mut stream = resp.bytes_stream();
        // Silence deadline, not a total one: slow-but-progressing pieces
        // survive, while a stalled connection fails after 10x timeout with
        // no bytes at all.
        let quiet_limit = timeout.saturating_mul(10);
        let mut last_progress = Instant::now();
        let failed: Option<AttemptFail> = loop {
            if last_progress.elapsed() >= quiet_limit {
                break Some(Retryable(gettext("Piece stalled")));
            }
            let idle = tokio::time::sleep(quiet_limit.saturating_sub(last_progress.elapsed()));
            tokio::pin!(idle);
            tokio::select! {
                _ = &mut idle => {
                    break Some(Retryable(gettext("Piece stalled")))
                }
                next = tokio::time::timeout(timeout, stream.next()) => match next {
                    Ok(Some(Ok(c))) => {
                        if body.len() + c.len() > (end - start + 1) as usize {
                            break Some(Retryable(gettext("Server sent too much data")));
                        }
                        body.extend_from_slice(&c);
                        // Empty chunks carry no bytes: only real data resets
                        // the silence clock, or keep-alives mask stalls.
                        if !c.is_empty() {
                            last_progress = Instant::now();
                        }
                    }
                    Ok(Some(Err(e))) => {
                        break Some(Retryable(format!("Download interrupted: {e}")))
                    }
                    Ok(None) => break None,
                    Err(_) => {
                        break Some(Retryable(gettext("Stalled connection timed out")))
                    }
                },
            }
        };
        if let Some(e) = failed {
            last_err = e;
            continue;
        }
        // A truncated stream would otherwise be recorded as a done piece
        // (zeros on disk) and skipped on every later resume.
        if body.len() as u64 != end - start + 1 {
            last_err = Retryable(gettext("Incomplete piece"));
            continue;
        }
        return Ok(body);
    }
    Err(last_err)
}
/// Filename from a `Content-Disposition` header (RFC 6266): prefers
/// `filename*=UTF-8''...`, falls back to quoted `filename="..."`, strips
/// any directory components servers sometimes include. `None` when absent
/// or unusable (caller keeps the URL-derived name).
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    fn basename(raw: &str) -> &str {
        raw.trim()
            .trim_matches('"')
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .trim()
    }
    let mut fallback = None;
    for part in value.split(';').map(str::trim) {
        // Parameter names are case-insensitive (`FILENAME=`, `FileName*=`).
        let Some((pname, pval)) = part.split_once('=') else {
            continue;
        };
        if pname.trim().eq_ignore_ascii_case("filename*") {
            // Form: filename*=UTF-8''%E2%82%ACrates.mp4 (charset'lang'data).
            let data = pval.split('\'').next_back().unwrap_or("").trim();
            let name = percent_decode(basename(data));
            if sane_filename(&name) {
                return Some(name);
            }
        } else if pname.trim().eq_ignore_ascii_case("filename") && fallback.is_none() {
            let name = basename(pval);
            if sane_filename(name) {
                fallback = Some(name.to_string());
            }
        }
    }
    fallback
}

/// Per-piece fetch attempts before a worker gives up on it.
const PIECE_TRIES: u32 = 3;
