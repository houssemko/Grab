use futures_util::StreamExt as _;
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GrabDownloadStatus")]
pub enum DownloadStatus {
    #[default]
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl DownloadStatus {
    /// Short human-readable label for the status, for list rows and toasts.
    pub fn label(self) -> &'static str {
        match self {
            DownloadStatus::Queued => "Queued",
            DownloadStatus::Downloading => "Downloading",
            DownloadStatus::Paused => "Paused",
            DownloadStatus::Done => "Done",
            DownloadStatus::Failed => "Failed",
            DownloadStatus::Cancelled => "Cancelled",
        }
    }
}

const QUEUE_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredStatus {
    Queued,
    Paused,
    Downloading,
    Failed,
    Done,
}

impl StoredStatus {
    fn from_item(status: DownloadStatus) -> Option<Self> {
        match status {
            DownloadStatus::Queued => Some(StoredStatus::Queued),
            DownloadStatus::Paused => Some(StoredStatus::Paused),
            DownloadStatus::Downloading => Some(StoredStatus::Downloading),
            DownloadStatus::Failed => Some(StoredStatus::Failed),
            DownloadStatus::Done => Some(StoredStatus::Done),
            DownloadStatus::Cancelled => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredItem {
    url: String,
    dest_dir: String,
    filename: String,
    status: StoredStatus,
    #[serde(default)]
    progress: f64,
    /// Completed 1 MB pieces for segmented resume across restarts (v2+).
    /// Absent on v1 files and for items that need no resume.
    #[serde(default)]
    segments: Option<StoredSegments>,
}

/// Persisted piece bitmap: `done[i]` covers
/// `[i * PIECE_SIZE, min((i+1) * PIECE_SIZE, total))`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredSegments {
    total: u64,
    done: Vec<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredQueue {
    version: u32,
    items: Vec<StoredItem>,
}

mod imp {
    use super::*;
    use gtk4::glib::subclass::prelude::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::DownloadItem)]
    pub struct DownloadItem {
        #[property(get, set)]
        pub id: Cell<u64>,
        #[property(get, set)]
        pub url: RefCell<String>,
        #[property(get, set)]
        pub filename: RefCell<String>,
        #[property(get, set)]
        pub dest_dir: RefCell<String>,
        #[property(get, set, builder(DownloadStatus::Queued))]
        pub status: Cell<DownloadStatus>,
        #[property(get, set)]
        pub progress: Cell<f64>,
        #[property(get, set)]
        pub speed: RefCell<String>,
        #[property(get, set)]
        pub eta: RefCell<String>,
        #[property(get, set)]
        pub detail: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DownloadItem {
        const NAME: &'static str = "GrabDownloadItem";
        type Type = super::DownloadItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for DownloadItem {}
}

glib::wrapper! {
    pub struct DownloadItem(ObjectSubclass<imp::DownloadItem>);
}

impl DownloadItem {
    /// Create a list item; prefer [`DownloadManager::enqueue`] which dedupes.
    pub fn new(id: u64, url: &str, filename: &str, dest_dir: &str) -> Self {
        glib::Object::builder()
            .property("id", id)
            .property("url", url)
            .property("filename", filename)
            .property("dest-dir", dest_dir)
            .build()
    }

    /// Full destination path (`dest_dir` joined with `filename`).
    pub fn file_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.dest_dir()).join(self.filename())
    }
}

fn restored_status(stored: StoredStatus) -> DownloadStatus {
    match stored {
        StoredStatus::Paused => DownloadStatus::Paused,
        StoredStatus::Failed => DownloadStatus::Failed,
        StoredStatus::Done => DownloadStatus::Done,
        _ => DownloadStatus::Queued,
    }
}

/// Append ` (n)` before the extension until `taken` returns false.
///
/// Example: `dedupe_filename("f.iso", |n| n == "f.iso")` returns `"f (1).iso"`.
pub fn dedupe_filename(filename: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(filename) {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rfind('.') {
        Some(i) if i > 0 => (&filename[..i], Some(&filename[i + 1..])),
        _ => (filename, None),
    };
    let mut n = 1;
    loop {
        let cand = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{filename} ({n})"),
        };
        if !taken(&cand) {
            return cand;
        }
        n += 1;
    }
}

fn sane_filename(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\0') && s != "." && s != ".."
}

/// Best-effort filename from a URL path, falling back to `index.html`.
/// Decode `%XX` escapes (RFC 5987 `filename*=`); leaves everything else
/// (including `+`) untouched. No new dependency for ten lines.
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut decoded = None;
        if bytes[i] == b'%' {
            if let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2)) {
                if let (Some(h), Some(l)) = (hex(h), hex(l)) {
                    decoded = Some(h << 4 | l);
                }
            }
        }
        if let Some(b) = decoded {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Filename from a `Content-Disposition` header (RFC 6266): prefers
/// `filename*=UTF-8''...`, falls back to quoted `filename="..."`, strips
/// any directory components servers sometimes include. `None` when absent
/// or unusable (caller keeps the URL-derived name).
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    let mut fallback = None;
    for part in value.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("filename*=") {
            // Form: filename*=UTF-8''%E2%82%ACrates.mp4 (charset'lang'data).
            let data = rest.split('\'').next_back().unwrap_or("").trim();
            let name = data.rsplit(['/', '\\']).next().unwrap_or("").trim();
            let name = percent_decode(name);
            if sane_filename(&name) {
                return Some(name);
            }
        } else if fallback.is_none() {
            if let Some(rest) = part.strip_prefix("filename=") {
                let name = rest
                    .trim()
                    .trim_matches('"')
                    .trim()
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("")
                    .trim();
                if sane_filename(name) {
                    fallback = Some(name.to_string());
                }
            }
        }
    }
    // Case-insensitive retry for `FILENAME=` variants servers emit.
    if fallback.is_none() {
        for part in value.split(';').map(str::trim) {
            if let Some(rest) = part
                .get(9..)
                .filter(|_| part[..9].eq_ignore_ascii_case("filename="))
            {
                let name = rest
                    .trim()
                    .trim_matches('"')
                    .trim()
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("")
                    .trim();
                if sane_filename(name) {
                    fallback = Some(name.to_string());
                    break;
                }
            }
        }
    }
    fallback
}

pub fn filename_from_url(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()).map(|s| s.to_string()))
        })
        .filter(|s| sane_filename(s))
        .unwrap_or_else(|| "index.html".to_string())
}

/// Normalize user input into a URL string, adding `https://` to bare hosts.
///
/// # Errors
/// Returns a display-ready message when the input is not a usable URL.
/// Maximum accepted URL length (bytes); browsers and servers rarely
/// tolerate more, and it bounds queue-file and UI memory.
pub const MAX_URL_LEN: usize = 2048;

pub fn normalize_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.len() > MAX_URL_LEN {
        return Err(format!("URL is too long (max {MAX_URL_LEN} characters)"));
    }
    if let Ok(u) = url::Url::parse(trimmed) {
        if matches!(u.scheme(), "http" | "https") {
            return Ok(u.to_string());
        }
        if trimmed.contains("://") {
            return Ok(u.to_string());
        }
    }
    let bare = !trimmed.contains("://")
        && !trimmed.contains(' ')
        && (trimmed.contains('.') || trimmed.starts_with("localhost"));
    if bare {
        let with_scheme = format!("https://{trimmed}");
        if let Ok(u) = url::Url::parse(&with_scheme) {
            return Ok(u.to_string());
        }
    }
    Err(format!("Invalid URL: {trimmed}"))
}

/// Reject non-http(s) URLs.
///
/// # Errors
/// Returns a display-ready message for unsupported schemes or bad URLs.
pub fn validate_url(url_str: &str) -> Result<(), String> {
    let u = url::Url::parse(url_str).map_err(|_| format!("Invalid URL: {url_str}"))?;
    match u.scheme() {
        "http" | "https" => Ok(()),
        "ftp" => Err("FTP is not supported (use http/https)".to_string()),
        s => Err(format!("Unsupported scheme: {s} (use http/https)")),
    }
}

#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    pub tries: i32,
    pub timeout: i32,
    pub limit_rate: String,
    pub user_agent: String,
    /// Parallel range connections for large downloads (1 = single stream).
    pub connections: i32,
}

impl DownloadOptions {
    /// Snapshot the network-related GSettings keys.
    pub fn from_settings(s: &gio::Settings) -> Self {
        Self {
            tries: s.int("retries"),
            timeout: s.int("timeout"),
            limit_rate: s.string("speed-limit").to_string(),
            user_agent: s.string("user-agent").to_string(),
            connections: s.int("connections"),
        }
    }
}

fn tokio_rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("grab-download")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

enum EngineMsg {
    Progress {
        downloaded: u64,
        total: Option<u64>,
    },
    Finished {
        size: u64,
    },
    Failed(String),
    /// A multi worker finished one piece; the UI thread records it for resume.
    PieceDone(u64),
    /// Multi attempt failed terminally: shrink the file to the completed
    /// prefix (kept bitmap stays valid for a later segmented retry).
    TruncatePrefix,
    /// Fresh multi probe succeeded; the UI thread creates the resume bitmap.
    SegmentsInit {
        total: u64,
    },
    /// The server is throttling parallel connections: the UI thread shrinks
    /// the file to the completed prefix and drops the bitmap, then acks so
    /// the engine may continue single-stream. Handshake (not fire-and-forget)
    /// so a concurrent pause/resume can never observe bitmap without file.
    FallbackSingle {
        ack: async_channel::Sender<()>,
    },
    /// Server-advertised filename (Content-Disposition). The UI thread adopts
    /// it when the current name is extensionless, after deduping.
    SuggestName(String),
}

/// Shared inputs for one download's engine task. Groups the params every
/// attempt function needs instead of threading eight loose arguments.
struct FetchCtx {
    client: &'static reqwest::Client,
    url: String,
    dest: std::path::PathBuf,
    opts: DownloadOptions,
    rate_limit: Option<u64>,
    timeout: Duration,
    tx: async_channel::Sender<EngineMsg>,
}

async fn run_download(ctx: FetchCtx, connections: usize, mode: StartMode) {
    let timeout = Duration::from_secs(ctx.opts.timeout.max(1) as u64);
    let mut tries = ctx.opts.tries.max(1);
    match mode {
        StartMode::Single => {
            single_loop(&ctx, &mut tries).await;
        }
        StartMode::Fresh => match probe_ranges(ctx.client, &ctx.url, &ctx.opts, timeout).await {
            Ok(total) if !plan_pieces(total, connections).is_empty() => {
                ctx.tx.send(EngineMsg::SegmentsInit { total }).await.ok();
                if multi_loop(&ctx, total, None, connections, &mut tries).await {
                    let mut single_tries = ctx.opts.tries.max(1);
                    single_loop(&ctx, &mut single_tries).await;
                }
            }
            _ => {
                single_loop(&ctx, &mut tries).await;
            }
        },
        StartMode::Resume(st) => {
            let total = st.total;
            if multi_loop(&ctx, total, Some(st), connections, &mut tries).await {
                let mut single_tries = ctx.opts.tries.max(1);
                single_loop(&ctx, &mut single_tries).await;
            }
        }
    }
}

/// Generous single-stream fallback with retries.
async fn single_loop(ctx: &FetchCtx, tries: &mut i32) {
    loop {
        match attempt_once(
            ctx.client,
            &ctx.url,
            &ctx.dest,
            &ctx.opts,
            ctx.rate_limit,
            ctx.timeout,
            &ctx.tx,
        )
        .await
        {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                ctx.tx.send(EngineMsg::Finished { size }).await.ok();
                return;
            }
            Err(e) => {
                *tries -= 1;
                if *tries <= 0 {
                    ctx.tx.send(EngineMsg::Failed(e)).await.ok();
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
    loop {
        match attempt_multi(ctx, total, saved.clone(), max_workers).await {
            Ok(()) => {
                let size = tokio::fs::metadata(&ctx.dest)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                ctx.tx.send(EngineMsg::Finished { size }).await.ok();
                return false;
            }
            Err(AttemptFail::Throttled(_)) => {
                let (ack_tx, ack_rx) = async_channel::bounded::<()>(1);
                ctx.tx
                    .send(EngineMsg::FallbackSingle { ack: ack_tx })
                    .await
                    .ok();
                // Wait until the UI thread truncated + dropped the bitmap:
                // starting single-stream any earlier could append over holes.
                // If we get aborted here (pause/cancel), there is nothing to do.
                let _ = ack_rx.recv().await;
                return true;
            }
            Err(AttemptFail::Retryable(e)) => {
                *tries -= 1;
                if *tries <= 0 {
                    // Same channel, FIFO per sender: truncation lands first.
                    ctx.tx.send(EngineMsg::TruncatePrefix).await.ok();
                    ctx.tx.send(EngineMsg::Failed(e)).await.ok();
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
    saved: Option<SegmentState>,
    max_workers: usize,
) -> Result<(), AttemptFail> {
    let st = saved.unwrap_or_else(|| SegmentState::new(total));
    if st.total != total {
        return Err(AttemptFail::Retryable("File changed on server".to_string()));
    }
    let missing: Vec<(u64, u64, u64)> = st.missing();
    if missing.is_empty() {
        return Ok(());
    }
    let expect_bytes: u64 = missing.iter().map(|(_, s, e)| e - s + 1).sum();
    // Ensure the file exists at full size so workers can write at offsets.
    // Missing pieces stay sparse until fetched; resume always re-runs this.
    // Never truncate here: completed pieces are already on disk.
    ensure_sized(&ctx.dest, total)
        .await
        .map_err(AttemptFail::Retryable)?;
    let queue: Arc<Mutex<VecDeque<(u64, u64, u64)>>> =
        Arc::new(Mutex::new(missing.into_iter().collect()));
    let failed = Arc::new(AtomicBool::new(false));
    let first_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let throttled = Arc::new(AtomicBool::new(false));
    // (offset, bytes, piece index); bounded so a slow disk throttles fetchers.
    let (wtx, wrx) = async_channel::bounded::<(u64, Vec<u8>, u64)>(8);
    let name_offered = Arc::new(AtomicBool::new(false));
    // Never exceed the configured connections: extra range requests are
    // what throttling hosts punish.
    let n_workers = queue
        .lock()
        .expect("Grab: piece queue poisoned (bug)")
        .len()
        .min(max_workers.max(1))
        .clamp(1, 16);
    let mut workers = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let (ctx, wtx, queue, failed, first_err, throttled, name_offered, name_tx) = (
            ctx,
            wtx.clone(),
            Arc::clone(&queue),
            Arc::clone(&failed),
            Arc::clone(&first_err),
            Arc::clone(&throttled),
            Arc::clone(&name_offered),
            ctx.tx.clone(),
        );
        workers.push(async move {
            loop {
                if failed.load(Ordering::SeqCst) {
                    break;
                }
                let piece = queue
                    .lock()
                    .expect("Grab: piece queue poisoned (bug)")
                    .pop_front();
                let Some((idx, s, e)) = piece else { break };
                match fetch_piece(ctx, s, e, total, &name_offered, &name_tx).await {
                    Ok(bytes) => {
                        if wtx.send((s, bytes, idx)).await.is_err() {
                            break; // Writer gone; attempt is over.
                        }
                    }
                    Err(AttemptFail::Throttled(msg)) => {
                        first_err
                            .lock()
                            .expect("Grab: error slot poisoned (bug)")
                            .get_or_insert(msg);
                        throttled.store(true, Ordering::SeqCst);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(AttemptFail::Retryable(msg)) => {
                        first_err
                            .lock()
                            .expect("Grab: error slot poisoned (bug)")
                            .get_or_insert(msg);
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
    drop(wtx);
    let writer = {
        let wrx = wrx;
        let dest = ctx.dest.clone();
        async move {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            let mut downloaded = st.completed_bytes();
            ctx.tx
                .send(EngineMsg::Progress {
                    downloaded,
                    total: Some(total),
                })
                .await
                .ok();
            let pace_start = Instant::now();
            let mut paced: u64 = 0;
            let mut last_sent = Instant::now();
            let mut written: u64 = 0;
            use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
            while let Ok((offset, bytes, idx)) = wrx.recv().await {
                file.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|e| format!("Cannot write file: {e}"))?;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| format!("Cannot write file: {e}"))?;
                downloaded += bytes.len() as u64;
                written += bytes.len() as u64;
                ctx.tx.send(EngineMsg::PieceDone(idx)).await.ok();
                if let Some(rate) = ctx.rate_limit {
                    paced += bytes.len() as u64;
                    let wait = paced as f64 / rate as f64 - pace_start.elapsed().as_secs_f64();
                    if wait > 0.0 {
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    }
                }
                if last_sent.elapsed() >= Duration::from_millis(100) {
                    ctx.tx
                        .send(EngineMsg::Progress {
                            downloaded,
                            total: Some(total),
                        })
                        .await
                        .ok();
                    last_sent = Instant::now();
                }
            }
            file.flush()
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            ctx.tx
                .send(EngineMsg::Progress {
                    downloaded,
                    total: Some(total),
                })
                .await
                .ok();
            Ok::<u64, String>(written)
        }
    };
    let (wres, _) = tokio::join!(writer, futures_util::future::join_all(workers));
    let written = wres.map_err(AttemptFail::Retryable)?;
    if throttled.load(Ordering::SeqCst) {
        let msg = first_err
            .lock()
            .expect("Grab: error slot poisoned (bug)")
            .take()
            .unwrap_or_else(|| "Download interrupted".to_string());
        return Err(AttemptFail::Throttled(msg));
    }
    if failed.load(Ordering::SeqCst) {
        let msg = first_err
            .lock()
            .expect("Grab: error slot poisoned (bug)")
            .take()
            .unwrap_or_else(|| "Download interrupted".to_string());
        return Err(AttemptFail::Retryable(msg));
    }
    if written != expect_bytes {
        return Err(AttemptFail::Retryable("Incomplete download".to_string()));
    }
    Ok(())
}
/// True when the file has unallocated (sparse) regions. Parallel writes can
/// leave holes that a later append-at-EOF resume must not inherit: fully
/// written files always satisfy `blocks * 512 >= len`, so a shortfall proves
/// holes. Needs no new dependency (std `MetadataExt` only). On exotic
/// filesystems with unreliable block counts this degrades to "holes", i.e.
/// a safe full re-download, never silent corruption.
fn has_holes(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.blocks().saturating_mul(512) < m.len()
        })
        .unwrap_or(false)
}

async fn attempt_once(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    opts: &DownloadOptions,
    rate_limit: Option<u64>,
    timeout: Duration,
    tx: &async_channel::Sender<EngineMsg>,
) -> Result<(), String> {
    // At most one restart: a 416 may only trigger a single delete-and-retry.
    let mut restarted = false;
    loop {
        let start = tokio::fs::metadata(dest)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut req = client.get(url);
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
        }
        if !opts.user_agent.trim().is_empty() {
            req = req.header("User-Agent", opts.user_agent.trim());
        }
        let resp = match tokio::time::timeout(
            timeout,
            client.execute(req.build().map_err(|e| e.to_string())?),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err("Connection timed out".to_string()),
        };
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
            if start > 0 && claimed == Some(start) && !has_holes(dest) {
                return Ok(());
            }
            if restarted {
                // Even a plain GET gets 416: pathological server, stop looping.
                return Err("Server rejects range requests".to_string());
            }
            let _ = std::fs::remove_file(dest);
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
        let partial = start > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        let total = resp
            .content_length()
            .map(|t| if partial { t + start } else { t });
        let mut file = if partial {
            tokio::fs::OpenOptions::new().append(true).open(dest).await
        } else {
            tokio::fs::File::create(dest).await
        }
        .map_err(|e| format!("Cannot write file: {e}"))?;
        if !partial {
            if let Some(name) = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(filename_from_content_disposition)
            {
                tx.send(EngineMsg::SuggestName(name)).await.ok();
            }
        }
        let mut downloaded = if partial { start } else { 0 };
        tx.send(EngineMsg::Progress { downloaded, total })
            .await
            .ok();
        let mut stream = resp.bytes_stream();
        let pace_start = Instant::now();
        let mut paced: u64 = 0;
        let mut last_sent = Instant::now();
        use tokio::io::AsyncWriteExt as _;
        loop {
            let chunk = match tokio::time::timeout(timeout, stream.next()).await {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(format!("Download interrupted: {e}")),
                Ok(None) => break,
                Err(_) => return Err("Stalled connection timed out".to_string()),
            };
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            downloaded += chunk.len() as u64;
            if let Some(rate) = rate_limit {
                paced += chunk.len() as u64;
                let wait = paced as f64 / rate as f64 - pace_start.elapsed().as_secs_f64();
                if wait > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
            }
            if last_sent.elapsed() >= Duration::from_millis(100) {
                tx.send(EngineMsg::Progress { downloaded, total })
                    .await
                    .ok();
                last_sent = Instant::now();
            }
        }
        file.flush()
            .await
            .map_err(|e| format!("Cannot write file: {e}"))?;
        return match total {
            Some(t) if downloaded != t => Err("Incomplete download".to_string()),
            _ => Ok(()),
        };
    }
}

fn parse_rate(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return None;
    }
    let (num, mult) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 1024u64),
        b'M' | b'm' => (&s[..s.len() - 1], 1024 * 1024),
        b'G' | b'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        b'0'..=b'9' => (s, 1),
        _ => return None,
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0)
        .map(|v| (v * mult as f64) as u64)
}

fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn fmt_eta(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Piece size for segmented downloads. Small enough that one slow
/// connection only ever delays the tail by ~1 MB of progress.
const PIECE_SIZE: u64 = 1024 * 1024;
/// A file is split only when it holds at least this much per connection
/// (aria2-style: connections x MIN_SEGMENT), keeping small downloads on the
/// cheaper single-stream path.
const MIN_SEGMENT: u64 = 4 * 1024 * 1024;
/// Per-piece fetch attempts before a worker gives up on it.
const PIECE_TRIES: u32 = 3;

/// How many connections a download may use: at least 2 to bother splitting,
/// at most 16, and never more than one per MIN_SEGMENT of file.
fn split_count(total: u64, connections: usize) -> usize {
    (connections.max(1) as u64).min(total / MIN_SEGMENT).min(16) as usize
}

/// Split `total` bytes into 1 MB `(start, end)` pieces (inclusive ends).
/// Empty when the file is too small to split: caller uses single-stream.
fn plan_pieces(total: u64, connections: usize) -> Vec<(u64, u64)> {
    if split_count(total, connections) < 2 || total == 0 {
        return Vec::new();
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + PIECE_SIZE).min(total) - 1;
        pieces.push((start, end));
        start = end + 1;
    }
    pieces
}

/// Session-only resume bitmap for one segmented download. Never persisted:
/// the queue file keeps no piece state, so cross-session restores always
/// resume single-stream (files are truncated to a contiguous prefix first,
/// which keeps that path correct).
#[derive(Debug, Clone)]
pub(crate) struct SegmentState {
    total: u64,
    done: Vec<bool>,
}

impl SegmentState {
    fn new(total: u64) -> Self {
        Self {
            total,
            done: vec![false; total.div_ceil(PIECE_SIZE) as usize],
        }
    }

    fn mark(&mut self, idx: u64) {
        if let Some(slot) = self.done.get_mut(idx as usize) {
            *slot = true;
        }
    }

    /// Missing `(piece index, start, end)` ranges, in order.
    fn missing(&self) -> Vec<(u64, u64, u64)> {
        let mut out = Vec::new();
        for (i, done) in self.done.iter().enumerate() {
            if !done {
                let start = i as u64 * PIECE_SIZE;
                out.push((i as u64, start, (start + PIECE_SIZE).min(self.total) - 1));
            }
        }
        out
    }

    /// Contiguous completed prefix, in bytes.
    fn prefix_len(&self) -> u64 {
        self.done.iter().take_while(|b| **b).count() as u64 * PIECE_SIZE
    }

    /// Forget every piece from the first gap on, keeping the bitmap
    /// consistent with a file truncated to the completed prefix. Used
    /// wherever the file is shrunk while the bitmap is kept.
    fn forget_beyond_prefix(&mut self) {
        let mut gap = false;
        for slot in self.done.iter_mut() {
            if !*slot {
                gap = true;
            } else if gap {
                *slot = false;
            }
        }
    }

    /// Bytes already on disk according to the bitmap.
    fn completed_bytes(&self) -> u64 {
        self.done
            .iter()
            .enumerate()
            .map(|(i, d)| {
                if *d {
                    PIECE_SIZE.min(self.total.saturating_sub(i as u64 * PIECE_SIZE))
                } else {
                    0
                }
            })
            .sum()
    }
}

/// Shrink `path` to the longest completed piece prefix. Parallel writes can
/// leave holes; a later single-stream resume appends at EOF, which is only
/// correct on a contiguous prefix. Only ever shrinks.
fn truncate_to_prefix(path: &std::path::Path, st: &SegmentState) {
    let prefix = st.prefix_len();
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > prefix {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                let _ = f.set_len(prefix);
            }
        }
    }
}

/// Open for writing (creating), sizing to `total` only when the size
/// differs. `File::create` would truncate already-downloaded pieces on
/// every resume/retry and silently corrupt the file while the bitmap still
/// claims those pieces as done.
async fn ensure_sized(dest: &std::path::Path, total: u64) -> Result<(), String> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest)
        .await
        .map_err(|e| format!("Cannot write file: {e}"))?;
    if file.metadata().await.map(|m| m.len()).unwrap_or(u64::MAX) != total {
        file.set_len(total)
            .await
            .map_err(|e| format!("Cannot write file: {e}"))?;
    }
    Ok(())
}

/// How the engine task should start this download.
#[derive(Debug)]
enum StartMode {
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
async fn probe_ranges(
    client: &reqwest::Client,
    url: &str,
    opts: &DownloadOptions,
    timeout: Duration,
) -> Result<u64, String> {
    let mut req = client.get(url).header("Range", "bytes=0-0");
    if !opts.user_agent.trim().is_empty() {
        req = req.header("User-Agent", opts.user_agent.trim());
    }
    let resp = match tokio::time::timeout(
        timeout,
        client.execute(req.build().map_err(|e| e.to_string())?),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("Connection timed out".to_string()),
    };
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err("Range requests not supported".to_string());
    }
    let value = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    parse_range_total(&value, 0).ok_or_else(|| "Bad Content-Range".to_string())
}

/// Parse `Content-Range: bytes <start>-<end>/<total>`, checking the range
/// starts where we asked (pins all chunks to one file version).
fn parse_range_total(value: &str, expect_start: u64) -> Option<u64> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    if start.parse::<u64>().ok()? != expect_start {
        return None;
    }
    let total: u64 = total.parse().ok()?;
    (total > 0).then_some(total)
}

/// Why a piece or segmented attempt failed. Throttled means the server is
/// rejecting parallel range requests (per-IP connection limits, common on
/// file hosts) while a single stream still works: downgrade, don't retry.
enum AttemptFail {
    Retryable(String),
    Throttled(String),
}

/// Fetch one `[start, end]` piece, retrying stalls. Verifies the server
/// still serves the probed file version via the Content-Range total.
async fn fetch_piece(
    ctx: &FetchCtx,
    start: u64,
    end: u64,
    total: u64,
    name_offered: &Arc<AtomicBool>,
    name_tx: &async_channel::Sender<EngineMsg>,
) -> Result<Vec<u8>, AttemptFail> {
    let timeout = ctx.timeout;
    use AttemptFail::{Retryable, Throttled};
    let mut last_err = Retryable("Empty response".to_string());
    for _ in 0..PIECE_TRIES {
        let mut req = ctx
            .client
            .get(&ctx.url)
            .header("Range", format!("bytes={start}-{end}"));
        if !ctx.opts.user_agent.trim().is_empty() {
            req = req.header("User-Agent", ctx.opts.user_agent.trim());
        }
        let built = match req.build() {
            Ok(r) => r,
            Err(e) => {
                last_err = Retryable(e.to_string());
                continue;
            }
        };
        let resp = match tokio::time::timeout(timeout, ctx.client.execute(built)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                last_err = Retryable(e.to_string());
                continue;
            }
            Err(_) => {
                last_err = Retryable("Connection timed out".to_string());
                continue;
            }
        };
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            let code = resp.status().as_u16();
            // Per-IP connection limits speak 403/429/503 (and 509 on some
            // hosts) while a single stream still works: downgrade, not fail.
            let throttled = matches!(code, 403 | 429 | 503) || code == 509;
            last_err = if throttled {
                Throttled(format!("Range rejected: HTTP {code}"))
            } else {
                Retryable(format!("Range rejected: HTTP {code}"))
            };
            continue;
        }
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if parse_range_total(&cr, start) != Some(total) {
            last_err = Retryable("File changed on server".to_string());
            continue;
        }
        if !name_offered.swap(true, Ordering::SeqCst) {
            if let Some(name) = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(filename_from_content_disposition)
            {
                name_tx.send(EngineMsg::SuggestName(name)).await.ok();
            }
        }
        let mut body = Vec::with_capacity((end - start + 1).min(2 * PIECE_SIZE) as usize);
        let mut stream = resp.bytes_stream();
        let piece_deadline = tokio::time::sleep(timeout.saturating_mul(10));
        tokio::pin!(piece_deadline);
        let failed: Option<AttemptFail> = loop {
            tokio::select! {
                _ = &mut piece_deadline => {
                    break Some(Retryable("Piece stalled".to_string()))
                }
                next = tokio::time::timeout(timeout, stream.next()) => match next {
                    Ok(Some(Ok(c))) => {
                        if body.len() + c.len() > (end - start + 1) as usize + 1024 {
                            break Some(Retryable("Server sent too much data".to_string()));
                        }
                        body.extend_from_slice(&c);
                    }
                    Ok(Some(Err(e))) => {
                        break Some(Retryable(format!("Download interrupted: {e}")))
                    }
                    Ok(None) => break None,
                    Err(_) => {
                        break Some(Retryable("Stalled connection timed out".to_string()))
                    }
                },
            }
        };
        if let Some(e) = failed {
            last_err = e;
            continue;
        }
        return Ok(body);
    }
    Err(last_err)
}

pub struct DownloadManager {
    store: gio::ListStore,
    settings: gio::Settings,
    running: RefCell<HashMap<u64, tokio::task::JoinHandle<()>>>,
    next_id: Cell<u64>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
    batch: Cell<bool>,
    /// Resume bitmaps for segmented downloads (session-only, main thread).
    segment_state: RefCell<HashMap<u64, SegmentState>>,
}

/// Queue + engine owner: persists the queue, spawns downloads, notifies the UI.
impl DownloadManager {
    /// Create a manager over `store`; call [`DownloadManager::restore_queue`] once.
    pub fn new(store: gio::ListStore, settings: gio::Settings) -> Rc<Self> {
        Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
            batch: Cell::new(false),
            segment_state: RefCell::new(HashMap::new()),
        })
    }

    /// UI refresh callback, invoked after every state change.
    pub fn set_on_change(&self, cb: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(cb));
    }

    fn changed(&self) {
        if let Some(cb) = self.on_change.borrow().as_ref() {
            cb();
        }
    }

    fn alloc_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// The underlying download list.
    pub fn store(&self) -> &gio::ListStore {
        &self.store
    }

    /// Find an item by id.
    pub fn find(&self, id: u64) -> Option<DownloadItem> {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .find(|it| it.id() == id)
    }

    /// Validate, dedupe and queue a download, starting it when a slot is free.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    pub fn enqueue(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(url)?;
        validate_url(&url)?;
        let dir = dest_dir
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.effective_download_dir());
        let name = filename
            .filter(|s| sane_filename(s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| filename_from_url(&url));
        let name = dedupe_filename(&name, |n| {
            std::path::Path::new(&dir).join(n).exists()
                || (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .any(|it| it.dest_dir() == dir && it.filename() == n)
        });
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        Ok(self.insert(item))
    }

    /// Re-queue one persisted entry, preserving its intent (paused/failed stay).
    /// A validated piece bitmap resumes segmented instead of restarting.
    ///
    /// # Errors
    /// Returns a display-ready message when the stored entry is invalid.
    pub fn restore_existing(
        self: &Rc<Self>,
        url: &str,
        dest_dir: &str,
        filename: &str,
        status: StoredStatus,
        segments: Option<SegmentState>,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(url)?;
        validate_url(&url)?;
        if !sane_filename(filename) {
            return Err(format!("Invalid filename in queue: {filename}"));
        }
        if !std::path::Path::new(dest_dir).is_absolute() {
            return Err(format!("Invalid destination in queue: {dest_dir}"));
        }
        let item = DownloadItem::new(self.alloc_id(), &url, filename, dest_dir);
        item.set_status(restored_status(status));
        if let Some(st) = segments {
            self.segment_state.borrow_mut().insert(item.id(), st);
        }
        Ok(self.insert(item))
    }

    fn insert(self: &Rc<Self>, item: DownloadItem) -> DownloadItem {
        self.store.append(&item);
        self.persist_queue();
        self.changed();
        self.start_next();
        item
    }

    fn insert_history(self: &Rc<Self>, url: String, dir: String, name: String, progress: f64) {
        let Ok(url) = normalize_url(&url) else {
            eprintln!("Grab: skipping history entry with bad URL");
            return;
        };
        if validate_url(&url).is_err() || !sane_filename(&name) {
            eprintln!("Grab: skipping invalid history entry for {name}");
            return;
        }
        if !std::path::Path::new(&dir).is_absolute() {
            eprintln!("Grab: skipping history entry with relative destination");
            return;
        }
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        item.set_progress(progress.clamp(0.0, 1.0));
        item.set_status(DownloadStatus::Done);
        let size = std::fs::metadata(item.file_path())
            .map(|m| m.len())
            .unwrap_or(0);
        item.set_detail(if size > 0 {
            format!("Finished • {}", fmt_bytes(size))
        } else {
            "Finished".to_string()
        });
        self.insert(item);
    }

    /// Whether finish/fail desktop notifications are enabled.
    pub fn notifications_enabled(&self) -> bool {
        self.settings.boolean("show-notifications")
    }

    /// Configured folder, or the system Downloads folder when empty.
    pub fn effective_download_dir(&self) -> String {
        let configured = self.settings.string("download-dir").to_string();
        if !configured.is_empty() {
            return configured;
        }
        glib::user_special_dir(glib::UserDirectory::Downloads)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_string())
    }

    /// Simultaneous-download limit (at least 1).
    pub fn max_concurrent(&self) -> usize {
        self.settings.int("max-concurrent").max(1) as usize
    }

    fn start_next(self: &Rc<Self>) {
        while self.running.borrow().len() < self.max_concurrent() {
            let next = (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .find(|it| it.status() == DownloadStatus::Queued);
            match next {
                Some(item) => self.spawn(item),
                None => break,
            }
        }
    }

    fn spawn(self: &Rc<Self>, item: DownloadItem) {
        let opts = DownloadOptions::from_settings(&self.settings);
        let dest = item.file_path();
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let limit = opts.limit_rate.trim();
        let rate = if limit.is_empty() || limit == "0" {
            None
        } else {
            match parse_rate(limit) {
                Some(r) => Some(r),
                None => {
                    item.set_status(DownloadStatus::Failed);
                    item.set_detail(format!("Invalid speed limit: {limit}"));
                    self.changed();
                    return;
                }
            }
        };
        let url = item.url().to_string();
        let connections = (opts.connections.max(1) as usize).min(16);
        let timeout = Duration::from_secs(opts.timeout.max(1) as u64);
        // A saved bitmap means this item wrote non-contiguous pieces: only a
        // segmented resume is correct (single-stream appends at EOF).
        let mode = match self.segment_state.borrow().get(&item.id()).cloned() {
            Some(st) => {
                // The bitmap is only valid if the file still holds at least
                // the completed prefix (and nothing beyond the total): the
                // user may have deleted, truncated, or replaced the partial
                // file while paused. A stale bitmap would skip pieces that
                // are no longer on disk, so drop it and start over instead.
                let len = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                if len < st.prefix_len() || len > st.total {
                    self.segment_state.borrow_mut().remove(&item.id());
                    let _ = std::fs::remove_file(&dest);
                    StartMode::Fresh
                } else {
                    let frac =
                        (st.completed_bytes() as f64 / st.total.max(1) as f64).clamp(0.0, 1.0);
                    item.set_progress(frac);
                    StartMode::Resume(st)
                }
            }
            None => match std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) {
                0 => StartMode::Fresh,
                _ => StartMode::Single,
            },
        };
        let (tx, rx) = async_channel::unbounded();
        let ctx = FetchCtx {
            client: http_client(),
            url,
            dest,
            opts,
            rate_limit: rate,
            timeout,
            tx,
        };
        let handle = tokio_rt().spawn(run_download(ctx, connections, mode));
        self.running.borrow_mut().insert(item.id(), handle);
        item.set_status(DownloadStatus::Downloading);
        let host = url::Url::parse(&item.url())
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        item.set_detail(if host.is_empty() {
            "Starting…".to_string()
        } else {
            format!("Connecting to {host}…")
        });
        self.changed();

        let this = Rc::clone(self);
        let id = item.id();
        let t0 = Instant::now();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    EngineMsg::Progress { downloaded, total } => {
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let bps = downloaded as f64 / t0.elapsed().as_secs_f64().max(0.001);
                        let speed = format!("{}/s", fmt_bytes(bps as u64));
                        match total {
                            Some(t) if t > 0 => {
                                let frac = (downloaded as f64 / t as f64).clamp(0.0, 1.0);
                                item.set_progress(frac);
                                item.set_speed(speed.clone());
                                let eta = if bps > 0.0 {
                                    fmt_eta((t.saturating_sub(downloaded) as f64 / bps) as u64)
                                } else {
                                    "—".to_string()
                                };
                                item.set_eta(eta.clone());
                                item.set_detail(format!(
                                    "{}% • {} • ETA {}",
                                    (frac * 100.0) as u64,
                                    speed,
                                    eta
                                ));
                            }
                            _ => {
                                item.set_detail(format!("{} • {}", fmt_bytes(downloaded), speed));
                            }
                        }
                    }
                    EngineMsg::Finished { size } => {
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_progress(1.0);
                            item.set_status(DownloadStatus::Done);
                            item.set_detail(if size > 0 {
                                format!("Finished • {}", fmt_bytes(size))
                            } else {
                                "Finished".to_string()
                            });
                            this.segment_state.borrow_mut().remove(&id);
                            this.notify_finished(&item, true, None);
                        }
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Failed);
                            item.set_detail(e.clone());
                            this.notify_finished(&item, false, Some(e));
                        }
                        break;
                    }
                    EngineMsg::SegmentsInit { total } => {
                        this.segment_state
                            .borrow_mut()
                            .insert(id, SegmentState::new(total));
                    }
                    EngineMsg::PieceDone(idx) => {
                        if let Some(st) = this.segment_state.borrow_mut().get_mut(&id) {
                            st.mark(idx);
                        }
                    }
                    EngineMsg::TruncatePrefix => {
                        if let Some(st) = this.segment_state.borrow_mut().get_mut(&id) {
                            truncate_to_prefix(&item.file_path(), st);
                            // The file just lost everything past the prefix;
                            // the bitmap must forget it too, or a later resume
                            // would skip pieces that are no longer on disk.
                            st.forget_beyond_prefix();
                        }
                    }
                    EngineMsg::FallbackSingle { ack } => {
                        // Server throttled parallel connections: shrink to the
                        // completed prefix and forget the bitmap, all here on
                        // the main thread so no spawn can observe a half-done
                        // transition. The engine waits for this ack before it
                        // appends single-stream at EOF.
                        if let Some(st) = this.segment_state.borrow().get(&id) {
                            truncate_to_prefix(&item.file_path(), st);
                        }
                        this.segment_state.borrow_mut().remove(&id);
                        ack.send(()).await.ok();
                    }
                    EngineMsg::SuggestName(name) => {
                        // Adopt the server-advertised name only while the
                        // download is fresh: the engine writes through an open
                        // handle, so renaming the path underneath is safe, and
                        // resume offsets are unaffected.
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let current = item.filename().to_string();
                        if current != "index.html" && current.contains('.') {
                            continue;
                        }
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        let dir = item.dest_dir().to_string();
                        let taken = |n: &str| {
                            std::path::Path::new(&dir).join(n).exists()
                                || (0..this.store.n_items())
                                    .filter_map(|i| {
                                        this.store.item(i).and_downcast::<DownloadItem>()
                                    })
                                    .any(|it| it.dest_dir() == dir && it.filename() == n)
                        };
                        let final_name = dedupe_filename(&name, taken);
                        if final_name == current {
                            continue;
                        }
                        let old_path = item.file_path();
                        let new_path = std::path::Path::new(&dir).join(&final_name);
                        if old_path != new_path {
                            if old_path.exists() && std::fs::rename(&old_path, &new_path).is_err() {
                                continue;
                            }
                            item.set_filename(final_name);
                            this.persist_queue();
                            this.changed();
                        }
                    }
                }
            }
            this.running.borrow_mut().remove(&id);
            this.persist_queue();
            this.changed();
            this.start_next();
        });
    }

    fn notify_finished(&self, item: &DownloadItem, ok: bool, hint: Option<String>) {
        if !self.notifications_enabled() {
            return;
        }
        if let Some(app) = gio::Application::default() {
            let n = gio::Notification::new(if ok {
                "Download finished"
            } else {
                "Download failed"
            });
            let mut body = format!(
                "{} → {}",
                item.filename(),
                item.file_path().to_string_lossy()
            );
            if !ok {
                if let Some(h) = hint {
                    body.push_str(&format!("\n{h}"));
                }
            }
            n.set_body(Some(&body));
            n.set_default_action_and_target_value("app.present", None);
            app.send_notification(Some(&format!("dl-{}", item.id())), &n);
        }
    }

    /// Pause a running download, freeing its slot for the next queued item.
    pub fn pause(self: &Rc<Self>, id: u64) {
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            if item.status() == DownloadStatus::Downloading {
                item.set_status(DownloadStatus::Paused);
                item.set_detail(format!("Paused • {}%", (item.progress() * 100.0) as u64));
            }
        }
        // Persist the bitmap too: a kill while paused must resume segmented.
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Re-queue a paused download.
    pub fn resume(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id) {
            if item.status() == DownloadStatus::Paused {
                item.set_status(DownloadStatus::Queued);
                self.persist_queue();
                self.changed();
                self.start_next();
            }
        }
    }

    /// Cancel a download; retry with [`DownloadManager::retry`].
    pub fn cancel(self: &Rc<Self>, id: u64) {
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        let had_segments = self.segment_state.borrow_mut().remove(&id).is_some();
        if let Some(item) = self.find(id) {
            if had_segments {
                let _ = std::fs::remove_file(item.file_path());
            }
            item.set_status(DownloadStatus::Cancelled);
            item.set_detail("Cancelled".to_string());
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Re-queue a failed or cancelled download.
    pub fn retry(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id) {
            match item.status() {
                DownloadStatus::Failed | DownloadStatus::Cancelled => {
                    item.set_progress(0.0);
                    item.set_speed(String::new());
                    item.set_eta(String::new());
                    item.set_detail(String::new());
                    item.set_status(DownloadStatus::Queued);
                    self.persist_queue();
                    self.changed();
                    self.start_next();
                }
                _ => {}
            }
        }
    }

    /// Cancel and drop a row; restore with [`DownloadManager::unremove`].
    pub fn remove(self: &Rc<Self>, id: u64) {
        self.cancel(id);
        if let Some(pos) = (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<DownloadItem>()
                .map(|it| it.id() == id)
                .unwrap_or(false)
        }) {
            self.store.remove(pos);
        }
        self.persist_queue();
        self.changed();
    }

    /// Re-insert a previously removed download (Undo). Restores the prior
    /// status except `Downloading`, which restarts as `Queued`.
    pub fn unremove(
        self: &Rc<Self>,
        url: String,
        dest_dir: String,
        filename: String,
        status: DownloadStatus,
        progress: f64,
        detail: String,
    ) -> DownloadItem {
        let item = DownloadItem::new(self.alloc_id(), &url, &filename, &dest_dir);
        item.set_progress(progress.clamp(0.0, 1.0));
        item.set_detail(detail);
        item.set_status(match status {
            DownloadStatus::Downloading => DownloadStatus::Queued,
            s => s,
        });
        self.insert(item.clone());
        item
    }

    /// Move the downloaded file to Trash, then remove the row.
    ///
    /// # Errors
    /// Returns a display-ready message when trashing fails.
    pub fn delete_download(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let item = self
            .find(id)
            .ok_or_else(|| "Download not found".to_string())?;
        match gio::File::for_path(item.file_path()).trash(gio::Cancellable::NONE) {
            Ok(()) => {}
            Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
            Err(e) => return Err(format!("Could not move {} to Trash: {e}", item.filename())),
        }
        self.remove(id);
        Ok(())
    }

    /// Cancel every queued, downloading or paused item.
    pub fn cancel_all(self: &Rc<Self>) {
        self.for_matching(
            |s| {
                matches!(
                    s,
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            },
            |m, id| m.cancel(id),
        );
    }

    /// Re-queue every failed or cancelled item.
    pub fn retry_failed(self: &Rc<Self>) {
        self.for_matching(
            |s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled),
            |m, id| m.retry(id),
        );
    }

    fn for_matching(
        self: &Rc<Self>,
        matches: impl Fn(DownloadStatus) -> bool,
        mut op: impl FnMut(&Rc<Self>, u64),
    ) {
        let ids: Vec<u64> = (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| matches(it.status()))
            .map(|it| it.id())
            .collect();
        for id in ids {
            op(self, id);
        }
    }

    /// Whether any item is queued, downloading or paused.
    pub fn has_active(&self) -> bool {
        self.any_status(|s| {
            matches!(
                s,
                DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
            )
        })
    }

    /// Whether any item failed or was cancelled.
    pub fn has_failed(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled))
    }

    fn any_status(&self, pred: impl Fn(DownloadStatus) -> bool) -> bool {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .any(|it| pred(it.status()))
    }

    fn queue_file() -> std::path::PathBuf {
        if let Some(p) = std::env::var_os("GRAB_QUEUE_FILE") {
            return std::path::PathBuf::from(p);
        }
        let mut dir = glib::user_data_dir();
        dir.push("grab");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("queue.json")
    }

    fn persist_queue(&self) {
        if self.batch.get() {
            return;
        }
        let mut items = Vec::new();
        for i in 0..self.store.n_items() {
            if let Some(it) = self.store.item(i).and_downcast::<DownloadItem>() {
                if let Some(status) = StoredStatus::from_item(it.status()) {
                    let segments =
                        self.segment_state
                            .borrow()
                            .get(&it.id())
                            .map(|st| StoredSegments {
                                total: st.total,
                                done: st.done.clone(),
                            });
                    items.push(StoredItem {
                        url: it.url().to_string(),
                        dest_dir: it.dest_dir().to_string(),
                        filename: it.filename().to_string(),
                        status,
                        progress: it.progress(),
                        segments,
                    });
                }
            }
        }
        let data = StoredQueue {
            version: QUEUE_VERSION,
            items,
        };
        let text = match serde_json::to_string_pretty(&data) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("Grab: could not serialize download queue: {e}");
                return;
            }
        };
        let tmp = Self::queue_file().with_extension("json.tmp");
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            Ok(())
        };
        match write_tmp() {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, Self::queue_file()) {
                    eprintln!("Grab: could not replace download queue: {e}");
                    return;
                }
                if let Some(parent) = Self::queue_file().parent() {
                    if let Ok(dir) = std::fs::File::open(parent) {
                        let _ = dir.sync_all();
                    }
                }
            }
            Err(e) => eprintln!("Grab: could not persist download queue: {e}"),
        }
    }

    /// Load the persisted queue (cap: 1000 items / 10 MB), then resume.
    pub fn restore_queue(self: &Rc<Self>) {
        if Self::queue_file().exists() {
            const MAX_QUEUE_BYTES: u64 = 10_000_000;
            const MAX_QUEUE_ITEMS: usize = 1000;
            if std::fs::metadata(Self::queue_file())
                .map(|m| m.len() > MAX_QUEUE_BYTES)
                .unwrap_or(true)
            {
                eprintln!("Grab: ignoring oversized download queue");
                return;
            }
            let Ok(text) = std::fs::read_to_string(Self::queue_file()) else {
                return;
            };
            let Ok(queue) = serde_json::from_str::<StoredQueue>(&text) else {
                eprintln!("Grab: ignoring unreadable download queue");
                return;
            };
            if queue.version == 0 || queue.version > QUEUE_VERSION {
                eprintln!("Grab: ignoring download queue version {}", queue.version);
                return;
            }
            if queue.items.len() > MAX_QUEUE_ITEMS {
                eprintln!(
                    "Grab: truncating download queue ({} items)",
                    queue.items.len()
                );
            }
            self.batch.set(true);
            for item in queue.items.into_iter().take(MAX_QUEUE_ITEMS) {
                match item.status {
                    StoredStatus::Done => {
                        self.insert_history(item.url, item.dest_dir, item.filename, item.progress);
                    }
                    status => {
                        // A stored bitmap resumes segmented; anything
                        // misshapen is dropped (single-stream fallback stays
                        // correct via the spawn-time file checks).
                        let segments = match item.segments {
                            Some(s)
                                if s.total > 0
                                    && s.done.len() == s.total.div_ceil(PIECE_SIZE) as usize =>
                            {
                                Some(SegmentState {
                                    total: s.total,
                                    done: s.done,
                                })
                            }
                            _ => None,
                        };
                        if let Err(e) = self.restore_existing(
                            &item.url,
                            &item.dest_dir,
                            &item.filename,
                            status,
                            segments,
                        ) {
                            eprintln!("Grab: skipping queue entry: {e}");
                        }
                    }
                }
            }
            self.batch.set(false);
            self.persist_queue();
            self.changed();
        }
    }

    /// Abort running tasks and persist the queue for the next launch.
    pub fn shutdown(&self) {
        let handles: Vec<_> = self.running.borrow_mut().drain().map(|(_, h)| h).collect();
        for handle in &handles {
            handle.abort();
        }
        // Engine tasks touch only the tokio runtime (never the main thread),
        // so joining them here is prompt and deadlock-free.
        tokio_rt().block_on(async {
            for handle in handles {
                let _ = handle.await;
            }
        });
        let ids: Vec<u64> = self.segment_state.borrow().keys().cloned().collect();
        for id in ids {
            if let Some(item) = self.find(id) {
                if let Some(st) = self.segment_state.borrow_mut().get_mut(&id) {
                    truncate_to_prefix(&item.file_path(), st);
                    st.forget_beyond_prefix();
                }
            }
        }
        // Persist BEFORE returning: the bitmaps are what let the next launch
        // resume segmented instead of restarting. The map itself stays: the
        // engine futures still draining will re-persist as they exit, and
        // dropping the bitmaps here would make those rewrites lose them.
        // (Fresh process exit frees the map anyway; shutdown only runs once.)
        self.persist_queue();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static QUEUE_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_queue_file(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "grab-q-{:?}-{tag}.json",
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        std::env::set_var("GRAB_QUEUE_FILE", &p);
        p
    }

    fn test_settings() -> gio::Settings {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        gio::Settings::new("io.github.houssemko.Grab")
    }

    /// One throwaway HTTP fixture: temp dirs, payload file, and a running
    /// throttled_server.py. On success the test kills the server and removes
    /// `dir`; on failure everything stays behind (ranges.log replay).
    struct Fixture {
        dir: std::path::PathBuf,
        dl: std::path::PathBuf,
        payload: Vec<u8>,
        port: u16,
        server: Rc<RefCell<std::process::Child>>,
    }

    fn spawn_fixture(
        tag: &str,
        served_name: &str,
        payload_len: u32,
        sleep_secs: &str,
        extra_args: &[&str],
        port_offset: u16,
    ) -> Fixture {
        let dir = std::env::temp_dir().join(format!("grab-{tag}-{}", std::process::id()));
        let srv = dir.join("srv");
        let dl = dir.join("dl");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
        std::fs::write(srv.join(served_name), &payload).unwrap();
        let port = 20000 + (std::process::id() % 5000) as u16 + port_offset;
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(format!(
            "{}/tests/throttled_server.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(port.to_string())
        .arg(srv.join(served_name))
        .arg(dir.join("ranges.log"))
        .arg(sleep_secs);
        for a in extra_args {
            cmd.arg(a);
        }
        let server = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 range server");
        let mut ready = false;
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ready, "test HTTP server did not listen on port {port}");
        Fixture {
            dir,
            dl,
            payload,
            port,
            server: Rc::new(RefCell::new(server)),
        }
    }

    /// Fail the current test, killing its server first (dirs stay for logs).
    fn abort(server: &Rc<RefCell<std::process::Child>>, msg: &str) -> ! {
        eprintln!("TEST FAILURE: {msg}");
        let _ = server.borrow_mut().kill();
        std::process::exit(1);
    }

    /// Watchdog + run + quiescence drain shared by every main-loop test: the
    /// drain pumps until the context goes quiet so no pending future is left
    /// for another test's loop to trip over.
    fn run_loop(main_loop: &glib::MainLoop, watchdog_secs: u64) {
        let watchdog = main_loop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(watchdog_secs));
            if watchdog.is_running() {
                eprintln!("TEST TIMEOUT");
                std::process::exit(2);
            }
        });
        main_loop.run();
        let ctx = glib::MainContext::default();
        let mut idle_rounds = 0;
        while idle_rounds < 50 {
            if ctx.iteration(false) {
                idle_rounds = 0;
            } else {
                idle_rounds += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    fn splits_pieces() {
        // Too small: single-stream fallback.
        assert!(plan_pieces(0, 4).is_empty());
        assert!(plan_pieces(1024, 4).is_empty());
        assert!(plan_pieces(8 * 1024 * 1024 - 1, 4).is_empty());
        // Just under one segment per connection still splits if >= 2 fit.
        assert!(!plan_pieces(16 * 1024 * 1024 - 1, 4).is_empty());
        // One connection disables splitting entirely.
        assert!(plan_pieces(100 * 1024 * 1024, 1).is_empty());
        assert!(plan_pieces(100 * 1024 * 1024, 0).is_empty());
        // 100 MB at 4 connections: full 1 MB coverage, inclusive ends.
        let pieces = plan_pieces(100 * 1024 * 1024, 4);
        assert_eq!(pieces.len(), 100);
        assert_eq!(pieces[0], (0, 1024 * 1024 - 1));
        assert_eq!(pieces[99].1, 100 * 1024 * 1024 - 1);
        for w in pieces.windows(2) {
            assert_eq!(w[0].1 + 1, w[1].0);
        }
        // Uneven tail.
        let pieces = plan_pieces(9_000_000, 4);
        assert_eq!(pieces.len(), 9);
        assert_eq!(pieces.last().unwrap().1, 8_999_999);
        // Worker split count follows connections x 4 MB.
        assert_eq!(split_count(100 * 1024 * 1024, 4), 4);
        assert_eq!(split_count(100 * 1024 * 1024, 99), 16);
        assert_eq!(split_count(1024, 4), 0);
    }

    #[test]
    fn parses_range_totals() {
        assert_eq!(parse_range_total("bytes 0-0/12345", 0), Some(12345));
        assert_eq!(
            parse_range_total("bytes 1048576-2097151/12345", 1048576),
            Some(12345)
        );
        // Wrong start (different file version) or garbage rejected.
        assert_eq!(parse_range_total("bytes 0-99/12345", 50), None);
        assert_eq!(parse_range_total("bytes */12345", 0), None);
        assert_eq!(parse_range_total("nonsense", 0), None);
        assert_eq!(parse_range_total("bytes 0-0/0", 0), None);
    }

    #[test]
    fn forgets_bitmap_beyond_prefix() {
        let mut st = SegmentState::new(4 * PIECE_SIZE);
        st.mark(0);
        st.mark(1);
        st.mark(3);
        st.forget_beyond_prefix();
        assert_eq!(
            st.missing(),
            vec![
                (2, 2 * PIECE_SIZE, 3 * PIECE_SIZE - 1),
                (3, 3 * PIECE_SIZE, 4 * PIECE_SIZE - 1),
            ]
        );
        assert_eq!(st.prefix_len(), 2 * PIECE_SIZE);
    }

    #[test]
    fn truncates_to_prefix() {
        let dir = std::env::temp_dir().join(format!("grab-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.bin");
        std::fs::write(&file, vec![7u8; 3 * PIECE_SIZE as usize]).unwrap();
        // Pieces 0,1 done, 2 missing: shrink to 2 MB.
        let mut st = SegmentState::new(3 * PIECE_SIZE);
        st.mark(0);
        st.mark(1);
        truncate_to_prefix(&file, &st);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_SIZE);
        // Already short: untouched.
        truncate_to_prefix(&file, &st);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_SIZE);
        // Nothing done: emptied.
        let st = SegmentState::new(3 * PIECE_SIZE);
        truncate_to_prefix(&file, &st);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_content_disposition() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"movie.mp4\""),
            Some("movie.mp4".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename=movie.mp4"),
            Some("movie.mp4".to_string())
        );
        // RFC 5987 encoding wins over the plain fallback.
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E2%82%ACrates.mp4"
            ),
            Some("€rates.mp4".to_string())
        );
        // Directory components stripped (some servers send Windows paths).
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"C:\\\\dl\\\\movie.mp4\""),
            Some("movie.mp4".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; FILENAME=\"upper.mp4\""),
            Some("upper.mp4".to_string())
        );
        // Garbage rejected: traversal, empty, missing.
        assert_eq!(filename_from_content_disposition("attachment"), None);
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"../evil.sh\""),
            Some("evil.sh".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"\""),
            None
        );
        assert_eq!(filename_from_content_disposition(""), None);
    }

    #[test]
    fn detects_sparse_holes() {
        let dir = std::env::temp_dir().join(format!("grab-holes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sparse = dir.join("sparse.bin");
        let f = std::fs::File::create(&sparse).unwrap();
        f.set_len(20_000_000).unwrap();
        assert!(has_holes(&sparse));
        std::fs::write(&sparse, vec![9u8; 100]).unwrap();
        assert!(!has_holes(&sparse));
        assert!(!has_holes(&dir.join("missing.bin")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_rates() {
        assert_eq!(parse_rate(""), None);
        assert_eq!(parse_rate("0"), None);
        assert_eq!(parse_rate("500K"), Some(500 * 1024));
        assert_eq!(parse_rate("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_rate("1.5M"), Some(1572864));
        assert_eq!(parse_rate("3G"), Some(3 * 1024 * 1024 * 1024));
        assert_eq!(parse_rate("1024"), Some(1024));
        assert_eq!(parse_rate("junk"), None);
        assert_eq!(parse_rate("-5K"), None);
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn formats_eta() {
        assert_eq!(fmt_eta(0), "0s");
        assert_eq!(fmt_eta(45), "45s");
        assert_eq!(fmt_eta(125), "2m5s");
        assert_eq!(fmt_eta(3723), "1h2m");
    }

    #[test]
    fn filenames() {
        assert_eq!(filename_from_url("https://example.com/a/b.iso"), "b.iso");
        assert_eq!(filename_from_url("https://example.com/"), "index.html");
        assert_eq!(filename_from_url("not a url"), "index.html");
    }

    #[test]
    fn urls() {
        assert!(validate_url("https://example.com/f.iso").is_ok());
        assert!(validate_url("ftp://example.com/f").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("--post-file=x").is_err());
    }

    #[test]
    fn normalizes_bare_hosts() {
        assert_eq!(
            normalize_url("example.com/f.iso").as_deref(),
            Ok("https://example.com/f.iso")
        );
        assert_eq!(
            normalize_url("  example.com  ").as_deref(),
            Ok("https://example.com/")
        );
        assert_eq!(
            normalize_url("http://example.com/f.iso").as_deref(),
            Ok("http://example.com/f.iso")
        );
        assert_eq!(
            normalize_url("localhost:8080/f.iso").as_deref(),
            Ok("https://localhost:8080/f.iso")
        );
        assert!(normalize_url("not a url").is_err());
        assert!(normalize_url("").is_err());
        assert_eq!(
            normalize_url("example .com").unwrap_err(),
            "Invalid URL: example .com"
        );
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(
            normalize_url(&long).unwrap_err(),
            format!("URL is too long (max {MAX_URL_LEN} characters)")
        );
        let maxed = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN - 20));
        assert!(normalize_url(&maxed).is_ok());
    }

    #[test]
    fn delete_download_removes_file_and_row() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("del");
        let settings = test_settings();
        // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
        let dir = glib::user_data_dir().join(format!("grab-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gone.bin");
        std::fs::write(&file, b"bye").unwrap();

        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store.clone(), settings);
        let item = DownloadItem::new(
            7,
            "https://example.com/gone.bin",
            "gone.bin",
            &dir.to_string_lossy(),
        );
        item.set_status(DownloadStatus::Done);
        store.append(&item);

        assert!(manager.delete_download(7).is_ok());
        assert!(!file.exists());
        assert_eq!(store.n_items(), 0);
        // Undo the test's own Trash litter.
        let trash = glib::user_data_dir().join("Trash");
        let _ = std::fs::remove_file(trash.join("files/gone.bin"));
        let _ = std::fs::remove_file(trash.join("info/gone.bin.trashinfo"));
        let item2 = DownloadItem::new(
            8,
            "https://example.com/missing.bin",
            "missing.bin",
            &dir.to_string_lossy(),
        );
        item2.set_status(DownloadStatus::Done);
        store.append(&item2);
        assert!(manager.delete_download(8).is_ok());
        assert_eq!(store.n_items(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedupes() {
        let taken = |n: &str| matches!(n, "f.iso" | "f (1).iso");
        assert_eq!(dedupe_filename("g.iso", taken), "g.iso");
        assert_eq!(dedupe_filename("f.iso", taken), "f (2).iso");
        assert_eq!(dedupe_filename("README", taken), "README");
        let taken = |n: &str| n == "README";
        assert_eq!(dedupe_filename("README", taken), "README (1)");
        let taken = |_: &str| false;
        assert_eq!(dedupe_filename(".profile", taken), ".profile");
        assert_eq!(dedupe_filename("a.tar.gz", taken), "a.tar.gz");
    }

    #[test]
    fn restore_keeps_exact_filename() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("restore-exact");
        test_settings();
        let dir = std::env::temp_dir().join(format!("grab-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_s = dir.to_string_lossy().into_owned();
        std::fs::write(dir.join("ubuntu.iso"), b"partial").unwrap();
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(99, holder);
        let item = manager
            .restore_existing(
                "https://example.com/ubuntu.iso",
                &dir_s,
                "ubuntu.iso",
                StoredStatus::Downloading,
                None,
            )
            .unwrap();
        assert_eq!(item.filename(), "ubuntu.iso");
        assert_eq!(item.status(), DownloadStatus::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_roundtrip_and_mapping() {
        assert_eq!(
            StoredStatus::from_item(DownloadStatus::Done),
            Some(StoredStatus::Done)
        );
        assert!(StoredStatus::from_item(DownloadStatus::Cancelled).is_none());
        for s in [
            DownloadStatus::Queued,
            DownloadStatus::Paused,
            DownloadStatus::Downloading,
            DownloadStatus::Failed,
        ] {
            assert!(StoredStatus::from_item(s).is_some());
        }
        let q = StoredQueue {
            version: QUEUE_VERSION,
            items: vec![
                StoredItem {
                    url: "https://example.com/a.iso".to_string(),
                    dest_dir: "/tmp/dl".to_string(),
                    filename: "a.iso".to_string(),
                    status: StoredStatus::Queued,
                    progress: 0.0,
                    segments: None,
                },
                StoredItem {
                    url: "https://example.com/b.iso".to_string(),
                    dest_dir: "/tmp/dl".to_string(),
                    filename: "b.iso".to_string(),
                    status: StoredStatus::Done,
                    progress: 1.0,
                    segments: None,
                },
            ],
        };
        let text = serde_json::to_string(&q).unwrap();
        let back: StoredQueue = serde_json::from_str(&text).unwrap();
        assert_eq!(back.version, QUEUE_VERSION);
        assert_eq!(back.items.len(), 2);
        assert_eq!(back.items[1].filename, "b.iso");
        let legacy = r#"{"version":1,"items":[{"url":"https://example.com/c.iso","dest_dir":"/tmp","filename":"c.iso","status":"done"}]}"#;
        let back: StoredQueue = serde_json::from_str(legacy).unwrap();
        assert!((back.items[0].progress - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn history_restore_roundtrip() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("history");
        let settings = test_settings();
        let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let done = DownloadItem::new(1, "https://example.com/old.iso", "old.iso", "/tmp/dl");
        done.set_progress(1.0);
        done.set_status(DownloadStatus::Done);
        done.set_detail("Finished".to_string());
        m1.store().append(&done);
        m1.persist_queue();
        assert!(qf.exists());

        let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        m2.restore_queue();
        assert_eq!(m2.store().n_items(), 1);
        let it = m2.store().item(0).and_downcast::<DownloadItem>().unwrap();
        assert_eq!(it.filename(), "old.iso");
        assert_eq!(it.status(), DownloadStatus::Done);
        assert!((it.progress() - 1.0).abs() < f64::EPSILON);
        // Atomic persist leaves no tmp debris behind.
        assert!(!qf.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn sane_filenames() {
        assert!(sane_filename("a.iso"));
        assert!(sane_filename("my file (1).tar.gz"));
        // Backslash is an ordinary (legal, harmless) char on Linux.
        assert!(sane_filename("a\\b"));
        assert!(!sane_filename(""));
        assert!(!sane_filename("."));
        assert!(!sane_filename(".."));
        assert!(!sane_filename("/etc/passwd"));
        assert!(!sane_filename("a/b"));
        assert!(!sane_filename("a\0b"));
    }

    #[test]
    fn restore_rejects_bad_filenames() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("restore-bad");
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        for bad in ["/etc/passwd", "..", ".", "a/b", ""] {
            assert!(manager
                .restore_existing(
                    "https://example.com/f.iso",
                    "/tmp/dl",
                    bad,
                    StoredStatus::Queued,
                    None,
                )
                .is_err());
        }
        assert!(manager
            .restore_existing(
                "https://example.com/f.iso",
                "relative/dir",
                "f.iso",
                StoredStatus::Queued,
                None,
            )
            .is_err());
        assert_eq!(manager.store().n_items(), 0);
    }

    #[test]
    fn batch_restore_hundred_done() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("batch");
        let settings = test_settings();
        let items: Vec<StoredItem> = (0..100)
            .map(|i| StoredItem {
                url: format!("https://example.com/f{i}.iso"),
                dest_dir: "/tmp/dl".to_string(),
                filename: format!("f{i}.iso"),
                status: StoredStatus::Done,
                progress: 1.0,
                segments: None,
            })
            .collect();
        let queue = StoredQueue {
            version: QUEUE_VERSION,
            items,
        };
        std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
        let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        m.restore_queue();
        assert_eq!(m.store().n_items(), 100);
        let it = m.store().item(99).and_downcast::<DownloadItem>().unwrap();
        assert_eq!(it.filename(), "f99.iso");
        assert_eq!(it.status(), DownloadStatus::Done);
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn cancel_frees_slot_immediately() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("cancel-slot");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let a = DownloadItem::new(41, "https://example.com/a.bin", "a.bin", "/tmp/dl");
        a.set_status(DownloadStatus::Downloading);
        manager.store().append(&a);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(41, holder);
        let b = DownloadItem::new(42, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
        manager.store().append(&b);
        manager.cancel(41);
        assert_eq!(a.status(), DownloadStatus::Cancelled);
        assert!(!manager.running.borrow().contains_key(&41));
        assert_eq!(b.status(), DownloadStatus::Downloading);
        assert!(manager.running.borrow().contains_key(&42));
        // Drain B's UI future on THIS thread: its Failed message is already
        // queued (dead port fails fast). Leaving it pending would let another
        // test's MainLoop poll it on the wrong thread and abort on glib's
        // thread-affinity guard.
        let ctx = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while manager.running.borrow().contains_key(&42) && std::time::Instant::now() < deadline {
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!manager.running.borrow().contains_key(&42));
        assert_eq!(b.status(), DownloadStatus::Failed);
    }

    #[test]
    fn pause_starts_next_queued() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("pause-next");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let a = DownloadItem::new(61, "https://example.com/a.bin", "a.bin", "/tmp/dl");
        a.set_status(DownloadStatus::Downloading);
        manager.store().append(&a);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(61, holder);
        let b = DownloadItem::new(62, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
        manager.store().append(&b);
        manager.pause(61);
        assert_eq!(a.status(), DownloadStatus::Paused);
        assert!(!manager.running.borrow().contains_key(&61));
        assert_eq!(b.status(), DownloadStatus::Downloading);
        assert!(manager.running.borrow().contains_key(&62));
        let ctx = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while manager.running.borrow().contains_key(&62) && std::time::Instant::now() < deadline {
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!manager.running.borrow().contains_key(&62));
    }

    #[test]
    fn restore_preserves_intent() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("intent");
        let settings = test_settings();
        let items: Vec<StoredItem> = [
            ("p.iso", StoredStatus::Paused),
            ("f.iso", StoredStatus::Failed),
            ("d.iso", StoredStatus::Done),
        ]
        .into_iter()
        .map(|(f, status)| StoredItem {
            url: format!("https://example.com/{f}"),
            dest_dir: "/tmp/dl".to_string(),
            filename: f.to_string(),
            status,
            progress: 0.5,
            segments: None,
        })
        .collect();
        let queue = StoredQueue {
            version: QUEUE_VERSION,
            items,
        };
        std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
        let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        m.restore_queue();
        assert_eq!(m.store().n_items(), 3);
        assert!(m.running.borrow().is_empty());
        let status_of = |name: &str| {
            (0..m.store().n_items())
                .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
                .find(|it| it.filename() == name)
                .map(|it| it.status())
        };
        assert_eq!(status_of("p.iso"), Some(DownloadStatus::Paused));
        assert_eq!(status_of("f.iso"), Some(DownloadStatus::Failed));
        assert_eq!(status_of("d.iso"), Some(DownloadStatus::Done));
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn restored_status_mapping() {
        assert_eq!(
            restored_status(StoredStatus::Paused),
            DownloadStatus::Paused
        );
        assert_eq!(
            restored_status(StoredStatus::Failed),
            DownloadStatus::Failed
        );
        assert_eq!(
            restored_status(StoredStatus::Queued),
            DownloadStatus::Queued
        );
        assert_eq!(
            restored_status(StoredStatus::Downloading),
            DownloadStatus::Queued
        );
    }

    #[test]
    fn retry_persists_immediately() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("retry-persist");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let item = DownloadItem::new(51, "https://example.com/r.bin", "r.bin", "/tmp/dl");
        item.set_status(DownloadStatus::Cancelled);
        manager.store().append(&item);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(52, holder);
        manager.retry(51);
        assert_eq!(item.status(), DownloadStatus::Queued);
        assert_eq!(manager.running.borrow().len(), 1);
        let text = std::fs::read_to_string(&qf).unwrap();
        let queue: StoredQueue = serde_json::from_str(&text).unwrap();
        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.items[0].filename, "r.bin");
        assert_eq!(queue.items[0].status, StoredStatus::Queued);
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn manager_pause_resume_cancel() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("lifecycle");
        let settings = test_settings();

        let dir = std::env::temp_dir().join(format!("grab-test-{}", std::process::id()));
        let srv = dir.join("srv");
        let dl = dir.join("dl");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(srv.join("t.bin"), &payload).unwrap();

        let port = 20000 + (std::process::id() % 5000) as u16;
        let server_log = dir.join("server.log");
        let server_log_file = std::fs::File::create(&server_log).unwrap();
        let ranges_log = dir.join("ranges.log");
        let server_py = format!("{}/tests/throttled_server.py", env!("CARGO_MANIFEST_DIR"));
        let server = std::process::Command::new("python3")
            .arg(&server_py)
            .arg(port.to_string())
            .arg(srv.join("t.bin"))
            .arg(&ranges_log)
            .stdout(std::process::Stdio::null())
            .stderr(server_log_file)
            .spawn()
            .expect("python3 throttled server");
        let mut ready = false;
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ready, "test HTTP server did not listen on port {port}");

        settings.set_boolean("show-notifications", false).unwrap();

        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        let url = format!("http://127.0.0.1:{port}/t.bin");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        let server = Rc::new(RefCell::new(server));
        glib::MainContext::default().spawn_local(async move {
            let server_kill = Rc::clone(&server);
            let path = dl.join("t.bin");
            let path_dbg = path.clone();
            let fail = move |msg: &str| -> ! {
                eprintln!("TEST FAILURE: {msg}");
                eprintln!(
                    "TEST server log:\n{}",
                    std::fs::read_to_string(&server_log).unwrap_or_default()
                );
                eprintln!(
                    "TEST partial: {:?}",
                    std::fs::metadata(&path_dbg).map(|m| m.len())
                );
                let _ = server_kill.borrow_mut().kill();
                std::process::exit(1);
            };
            let item = manager
                .enqueue(&url, Some(&dest), Some("t.bin"))
                .unwrap_or_else(|e| fail(&e));
            let id = item.id();

            glib::timeout_future(std::time::Duration::from_secs(1)).await;
            if item.status() != DownloadStatus::Downloading {
                fail("expected Downloading after 1s");
            }
            manager.pause(id);
            assert_eq!(item.status(), DownloadStatus::Paused);
            let size_a = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            glib::timeout_future(std::time::Duration::from_millis(600)).await;
            let size_b = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            assert_eq!(size_a, size_b, "bytes moved while paused");

            manager.resume(id);
            let mut waited = 0;
            while item.status() != DownloadStatus::Done && waited < 300 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Done {
                fail(&format!("expected Done, got {:?}", item.status()));
            }
            if std::fs::read(&path).unwrap() != payload {
                fail("bytes differ");
            }
            let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
            if !ranges.lines().any(|l| l.starts_with("bytes=")) {
                fail("resume never sent a Range request (206 path untested)");
            }

            let item2 = manager.enqueue(&url, Some(&dest), Some("t2.bin")).unwrap();
            manager.cancel(item2.id());
            assert_eq!(item2.status(), DownloadStatus::Cancelled);

            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        run_loop(&main_loop, 90);
    }

    #[test]
    fn segmented_multi_connection_download() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("segmented");
        let settings = test_settings();
        settings.set_int("connections", 4).unwrap();

        // 20 MB clears the 4 x 4 MB split threshold; unthrottled loopback
        // keeps the test to a few seconds.
        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture("seg", "big.bin", 20_000_000, "0", &[], 13);

        settings.set_boolean("show-notifications", false).unwrap();
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let path = dl.join("big.bin");
            let item = manager
                .enqueue(&url, Some(&dest), Some("big.bin"))
                .unwrap_or_else(|e| abort(&server, &e));
            let mut waited = 0;
            while item.status() != DownloadStatus::Done && waited < 600 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item.status()));
            }
            if std::fs::read(&path).unwrap() != payload {
                abort(&server, "bytes differ");
            }
            if !item.detail().starts_with("Finished \u{2022} ") {
                abort(
                    &server,
                    &format!("expected size in detail, got {:?}", item.detail()),
                );
            }
            // Distinct bounded ranges prove parallel segmented fetching
            // (a single stream would log one open-ended "bytes=0" line).
            let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
            let mut distinct = std::collections::HashSet::new();
            for line in ranges.lines().filter(|l| l.starts_with("bytes=")) {
                if line.contains('-') && !line.ends_with('-') && line.split('-').count() == 2 {
                    distinct.insert(line.to_string());
                }
            }
            if distinct.len() < 2 {
                abort(
                    &server,
                    &format!("expected 2+ distinct ranges, log: {ranges:?}"),
                );
            }
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn adopts_content_disposition_filename() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("disposition");
        let settings = test_settings();

        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture(
            "cd",
            "clip.mp4",
            300_000,
            "0.05",
            &["attachment; filename=\"movie.mp4\""],
            17,
        );

        settings.set_boolean("show-notifications", false).unwrap();
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        // Extensionless path, like CDN /videoplayback links.
        let url = format!("http://127.0.0.1:{port}/getfile");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let item = manager
                .enqueue(&url, Some(&dest), None)
                .unwrap_or_else(|e| abort(&server, &e));
            assert_eq!(item.filename(), "getfile");
            let mut waited = 0;
            while (item.filename() != "movie.mp4" || item.status() != DownloadStatus::Done)
                && waited < 600
            {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.filename() != "movie.mp4" {
                abort(
                    &server,
                    &format!("expected rename, got {:?}", item.filename()),
                );
            }
            if item.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item.status()));
            }
            let new_path = dl.join("movie.mp4");
            if std::fs::read(&new_path).unwrap() != payload {
                abort(&server, "bytes differ");
            }
            if dl.join("getfile").exists() {
                abort(&server, "stale file left behind");
            }
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn falls_back_to_single_stream_when_throttled() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("throttle");
        let settings = test_settings();
        settings.set_int("connections", 4).unwrap();

        // Big enough to split; the server 403s every range except the probe,
        // so the engine must downgrade to one stream and still finish intact.
        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture(
            "thr",
            "big.bin",
            20_000_000,
            "0",
            &["", "throttle-ranges"],
            19,
        );

        settings.set_boolean("show-notifications", false).unwrap();
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let path = dl.join("big.bin");
            let item = manager
                .enqueue(&url, Some(&dest), Some("big.bin"))
                .unwrap_or_else(|e| abort(&server, &e));
            let mut waited = 0;
            while item.status() != DownloadStatus::Done && waited < 600 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item.status()));
            }
            if std::fs::read(&path).unwrap() != payload {
                abort(&server, "bytes differ");
            }
            // A full (unranged) request proves the single-stream fallback ran:
            // pure multi would only ever log bounded ranges.
            let ranges = std::fs::read_to_string(dir.join("ranges.log")).unwrap_or_default();
            if !ranges.lines().any(|l| l == "full") {
                abort(
                    &server,
                    &format!("expected single-stream fallback, log: {ranges:?}"),
                );
            }
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn segmented_pause_resume_keeps_bytes() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("seg-pause");
        let settings = test_settings();
        settings.set_int("connections", 4).unwrap();

        // 20 MB clears the split threshold. Slightly throttled (~3 MB/s,
        // ~6 s total): the transfer must stay well above the 100 ms progress
        // granularity or no poll can land mid-transfer (unthrottled loopback
        // finishes in ~100 ms and polls only ever see 0% then Done).
        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture("segp", "big.bin", 20_000_000, "0.002", &[], 23);

        settings.set_boolean("show-notifications", false).unwrap();
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let path = dl.join("big.bin");
            let item = manager
                .enqueue(&url, Some(&dest), Some("big.bin"))
                .unwrap_or_else(|e| abort(&server, &e));
            let id = item.id();
            // Wait until at least one 1 MB piece (5%) landed, then pause.
            // The status check is part of the loop condition (same thread runs
            // to pause() with no await in between, so it cannot slip to Done).
            let mut waited = 0;
            while item.status() == DownloadStatus::Downloading
                && item.progress() < 0.05
                && waited < 2000
            {
                glib::timeout_future(std::time::Duration::from_millis(5)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Downloading {
                abort(&server, "finished before pause could land mid-transfer");
            }
            manager.pause(id);
            if item.status() != DownloadStatus::Paused {
                abort(
                    &server,
                    &format!("expected Paused, got {:?}", item.status()),
                );
            }
            // Resume must reuse the bitmap (partially done, not all).
            let mid: bool = manager
                .segment_state
                .borrow()
                .get(&id)
                .map(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b))
                .unwrap_or(false);
            if !mid {
                abort(&server, "resume bitmap is not mid-transfer");
            }
            manager.resume(id);
            let mut waited = 0;
            while item.status() != DownloadStatus::Done && waited < 600 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item.status()));
            }
            // The regression: resume used to truncate completed pieces to
            // zero while the bitmap still claimed them as done.
            if std::fs::read(&path).unwrap() != payload {
                abort(&server, "bytes differ after pause/resume");
            }
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn bitmap_persists_across_managers() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("segpersist");
        let settings = test_settings();
        let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let item = DownloadItem::new(1, "https://example.com/big.bin", "big.bin", "/tmp/dl");
        item.set_status(DownloadStatus::Paused);
        m1.store().append(&item);
        let mut st = SegmentState::new(4 * PIECE_SIZE);
        st.mark(0);
        st.mark(2);
        m1.segment_state.borrow_mut().insert(1, st);
        m1.persist_queue();

        // v1 readers ignore the unknown field; v2 keeps it.
        let text = std::fs::read_to_string(&qf).unwrap();
        assert!(text.contains("\"segments\""));
        let legacy = r#"{"version":1,"items":[{"url":"https://example.com/a.iso","dest_dir":"/tmp/dl","filename":"a.iso","status":"queued","progress":0.0}]}"#;
        let back: StoredQueue = serde_json::from_str(legacy).unwrap();
        assert!(back.items[0].segments.is_none());

        let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        m2.restore_queue();
        let restored = m2.segment_state.borrow();
        let st = restored.get(&1).expect("bitmap restored");
        assert_eq!(st.total, 4 * PIECE_SIZE);
        assert_eq!(
            st.missing(),
            vec![
                (1, PIECE_SIZE, 2 * PIECE_SIZE - 1),
                (3, 3 * PIECE_SIZE, 4 * PIECE_SIZE - 1),
            ]
        );
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn killed_segmented_resume_starts_over() {
        // Simulates SIGKILL mid-segmented-download: a sparse full-size file
        // with holes plus a stale Queued entry, no resume bitmap (RAM died
        // with the process). Restart must discard and re-fetch, never mark
        // the holey file Done via the 416 shortcut.
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("kill");
        let settings = test_settings();

        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture("kill", "big.bin", 20_000_000, "0", &[], 29);

        // Sparse corpse: full size, only the first megabyte real.
        let dest = dl.join("big.bin");
        {
            let f = std::fs::File::create(&dest).unwrap();
            f.set_len(20_000_000).unwrap();
            use std::io::Write;
            let mut f = f;
            f.write_all(&payload[..1_000_000]).unwrap();
        }
        assert!(has_holes(&dest));
        std::fs::write(
            &qf,
            serde_json::to_string(&StoredQueue {
                version: QUEUE_VERSION,
                items: vec![StoredItem {
                    url: format!("http://127.0.0.1:{port}/big.bin"),
                    dest_dir: dl.to_string_lossy().into_owned(),
                    filename: "big.bin".to_string(),
                    status: StoredStatus::Queued,
                    progress: 0.0,
                    segments: None,
                }],
            })
            .unwrap(),
        )
        .unwrap();

        settings.set_boolean("show-notifications", false).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            manager.restore_queue();
            if manager.store().n_items() != 1 {
                abort(&server, "queue did not restore");
            }
            let item = manager
                .store()
                .item(0)
                .and_downcast::<DownloadItem>()
                .unwrap();
            let mut waited = 0;
            while item.status() != DownloadStatus::Done && waited < 600 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item.status()));
            }
            if std::fs::read(&dest).unwrap() != payload {
                abort(&server, "holey file was marked Done instead of re-fetched");
            }
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&qf);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn shutdown_restart_resumes_segmented() {
        // Full kill-restart cycle in-process: manager1 downloads segmented,
        // shuts down mid-transfer (abort + truncate + persist WITH bitmap),
        // then a fresh manager2 on the same queue file resumes segmented.
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("segrestart");
        let settings = test_settings();
        settings.set_int("connections", 4).unwrap();

        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture("segr", "big.bin", 20_000_000, "0.002", &[], 31);

        settings.set_boolean("show-notifications", false).unwrap();
        let manager1 =
            DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let url = format!("http://127.0.0.1:{port}/big.bin");
            let dest = dl.to_string_lossy().into_owned();
            let item = manager1
                .enqueue(&url, Some(&dest), Some("big.bin"))
                .unwrap_or_else(|e| abort(&server, &e));
            let id = item.id();
            let mut waited = 0;
            while item.status() == DownloadStatus::Downloading
                && item.progress() < 0.05
                && waited < 2000
            {
                glib::timeout_future(std::time::Duration::from_millis(5)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Downloading {
                abort(&server, "finished before shutdown could land mid-transfer");
            }
            manager1.shutdown();
            // Bitmap survived shutdown, mid-transfer, and was persisted.
            let mid1 = manager1
                .segment_state
                .borrow()
                .get(&id)
                .map(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b))
                .unwrap_or(false);
            if !mid1 {
                abort(&server, "no mid-transfer bitmap at shutdown");
            }
            let text = std::fs::read_to_string(&qf).unwrap();
            if !text.contains("\"segments\"") {
                abort(&server, "bitmap was not persisted");
            }
            // Fresh manager, same queue file: must pick up the bitmap and
            // resume segmented, not restart.
            let manager2 =
                DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
            manager2.restore_queue();
            if manager2.store().n_items() != 1 {
                abort(&server, "queue did not restore");
            }
            let mid2 = manager2
                .segment_state
                .borrow()
                .values()
                .any(|st| st.done.iter().any(|b| *b) && st.done.iter().any(|b| !b));
            if !mid2 {
                abort(&server, "restored manager has no segmented resume state");
            }
            let item2 = manager2
                .store()
                .item(0)
                .and_downcast::<DownloadItem>()
                .unwrap();
            let mut waited = 0;
            while item2.status() != DownloadStatus::Done && waited < 600 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item2.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item2.status()));
            }
            if std::fs::read(dl.join("big.bin")).unwrap() != payload {
                abort(&server, "bytes differ after kill-restart-resume");
            }
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&qf);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn failed_download_reports_cause() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("http-error");
        let settings = test_settings();
        let port = 20000 + (std::process::id() % 5000) as u16 + 7;
        let server = std::process::Command::new("python3")
            .args(["-m", "http.server", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 http.server");
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let server = Rc::new(RefCell::new(server));
        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let store = gio::ListStore::new::<DownloadItem>();
            let manager = DownloadManager::new(store, settings);
            let item = manager
                .enqueue(
                    &format!("http://127.0.0.1:{port}/nope.bin"),
                    None,
                    Some("nope.bin"),
                )
                .unwrap_or_else(|e| abort(&server, &e));
            let mut waited = 0;
            while item.status() != DownloadStatus::Failed && waited < 200 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Failed {
                abort(
                    &server,
                    &format!("expected Failed, got {:?}", item.status()),
                );
            }
            if !item.detail().contains("404") {
                abort(
                    &server,
                    &format!("expected 404 hint, got {:?}", item.detail()),
                );
            }
            let _ = server.borrow_mut().kill();
            quit.quit();
        });
        run_loop(&main_loop, 60);
    }
}
