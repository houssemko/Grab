use futures_util::StreamExt as _;
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
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
    segments: Option<SegmentState>,
    /// Intake file selection for multi-file torrents (v2+). The live map
    /// is in-memory only, so the selection is persisted here and
    /// re-staged on restore — otherwise a restart drops the filter and
    /// the resume downloads every file.
    #[serde(default)]
    selected_files: Option<Vec<usize>>,
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

/// Cap a filename to filesystem limits (NAME_MAX is 255 bytes on
/// ext4/tmpfs), keeping the extension. Truncates the stem on a char
/// boundary; reserves room for the ` (n)` dedupe suffix.
pub(crate) fn shorten_filename(name: &str) -> String {
    const MAX_FILENAME_BYTES: usize = 240;
    if name.len() <= MAX_FILENAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i..])),
        _ => (name, None),
    };
    let ext_len = ext.map_or(0, str::len);
    let keep = stem.floor_char_boundary(MAX_FILENAME_BYTES.saturating_sub(ext_len));
    match ext {
        Some(e) => format!("{}{e}", &stem[..keep]),
        None => stem[..keep].to_string(),
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
    for _ in 1..=9999 {
        let cand = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{filename} ({n})"),
        };
        if !taken(&cand) {
            return cand;
        }
        n += 1;
    }
    // Absurd collision count: return the next candidate anyway (a later
    // write visibly fails) rather than stat-ing the disk forever.
    match ext {
        Some(e) => format!("{stem} ({n}).{e}"),
        None => format!("{filename} ({n})"),
    }
}

pub(crate) fn sane_filename(s: &str) -> bool {
    /// Explicit bidi controls (marks, embeddings/overrides, isolates).
    /// No std helper exists, so match the assigned ranges with escapes
    /// (never literal glyphs: they are invisible in source).
    fn is_bidi_control(c: char) -> bool {
        matches!(c, '\u{200E}' | '\u{200F}' | '\u{61C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    }
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\0')
        && s != "."
        && s != ".."
        // Control/bidi-override characters deceive in listings and
        // notification text (FIND-03/04); servers love to send them.
        && !s.chars().any(|c| c.is_control() || is_bidi_control(c))
}

/// Best-effort filename from a URL path, falling back to `index.html`.
/// Decode `%XX` escapes (RFC 5987 `filename*=`); leaves everything else
/// (including `+`) untouched. No new dependency for ten lines.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut decoded = None;
        if bytes[i] == b'%' {
            if let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2)) {
                if let (Some(h), Some(l)) = ((h as char).to_digit(16), (l as char).to_digit(16)) {
                    decoded = Some((h << 4 | l) as u8);
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

pub fn filename_from_url(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()).map(|s| s.to_string()))
        })
        // Browsers decode %XX escapes: %20 is a space, not six chars.
        .map(|s| percent_decode(&s))
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
    // Strip a pasted BOM: trim() leaves U+FEFF, which would defeat the
    // magnet classifier below and route magnets to the scheme branch.
    let trimmed = input.trim().trim_start_matches('\u{feff}');
    if crate::torrent::is_magnet(trimmed) {
        // Magnet links skip URL parsing and the HTTP length cap entirely:
        // parsed locally by the torrent engine, never sent as a request
        // line. Still capped against abuse (Ubuntu magnets run ~2-4 KB).
        const MAX_MAGNET_LEN: usize = 16384;
        if trimmed.len() > MAX_MAGNET_LEN {
            return Err(format!(
                "Magnet link is too long (max {MAX_MAGNET_LEN} characters)"
            ));
        }
        // Validated here so the row stores the trimmed link, re-parsed by
        // the torrent engine.
        return crate::torrent::parse_magnet(trimmed).map(|_| trimmed.to_string());
    }
    if crate::torrent::is_torrent_url(trimmed) {
        // Archived .torrent pseudo-URLs skip URL parsing and the HTTP
        // length cap like magnets: validated here so the row stores the
        // trimmed pseudo-URL, resolved by the torrent engine. The archive
        // must exist; a swept archive means the queue entry is stale.
        return crate::torrent::archive_path_for_url(trimmed)
            .map(|_| trimmed.to_string())
            .ok_or_else(|| "Torrent file is missing from the archive".to_string());
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(format!("URL is too long (max {MAX_URL_LEN} characters)"));
    }
    // An explicit scheme is authoritative: only http(s) passes, so the
    // separate validate pass is unnecessary. (Bare `host:port` inputs must
    // not take this branch: `localhost:8080/f` parses with scheme
    // "localhost" and still needs `https://` prepended below.)
    if trimmed.contains("://") {
        let u = url::Url::parse(trimmed).map_err(|_| format!("Invalid URL: {trimmed}"))?;
        if !u.username().is_empty() || u.password().is_some() {
            return Err("URLs with a username/password are not supported".to_string());
        }
        return match u.scheme() {
            "http" | "https" => Ok(u.to_string()),
            "ftp" => Err("FTP is not supported (use http/https)".to_string()),
            s => Err(format!("Unsupported scheme: {s} (use http/https)")),
        };
    }
    let bare =
        !trimmed.contains(' ') && (trimmed.contains('.') || trimmed.starts_with("localhost"));
    if bare {
        let with_scheme = format!("https://{trimmed}");
        if let Ok(u) = url::Url::parse(&with_scheme) {
            if u.username().is_empty() && u.password().is_none() {
                return Ok(u.to_string());
            }
        }
    }
    Err(format!("Invalid URL: {trimmed}"))
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

pub(crate) fn tokio_rt() -> &'static tokio::runtime::Runtime {
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
    CLIENT.get_or_init(|| {
        // Bounded hops: a malicious server must not bounce the client
        // around (or downgrade https→http) without limit. No cookie or
        // auth store is enabled, so only the URL + UA cross origins.
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("http client")
    })
}

pub(crate) enum EngineMsg {
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
        ack: tokio::sync::mpsc::Sender<()>,
    },
    /// Server-advertised filename (Content-Disposition). Stored for
    /// adoption at Finished, when the current name qualifies.
    SuggestName(String),
    /// The server object changed mid-download (version check failed). The
    /// UI thread drops the resume bitmap so a later retry starts fresh
    /// instead of failing on the dead file version forever.
    FailedVersion(String),
}

/// Shared inputs for one download's engine task. Groups the params every
/// attempt function needs instead of threading eight loose arguments.
struct FetchCtx {
    client: &'static reqwest::Client,
    url: String,
    dest: std::path::PathBuf,
    opts: DownloadOptions,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
}

async fn run_download(ctx: FetchCtx, connections: usize, mode: StartMode) {
    let timeout = Duration::from_secs(ctx.opts.timeout.max(1) as u64);
    let mut tries = ctx.opts.tries.max(1);
    match mode {
        StartMode::Single => {
            single_loop(&ctx, &mut tries, None, false).await;
        }
        StartMode::Fresh => match probe_ranges(ctx.client, &ctx.url, &ctx.opts, timeout).await {
            Ok(total) => {
                if plan_pieces(total, connections).is_empty() {
                    single_loop(&ctx, &mut tries, Some(total), true).await;
                } else {
                    ctx.tx.send(EngineMsg::SegmentsInit { total }).ok();
                    if multi_loop(&ctx, total, None, connections, &mut tries).await {
                        let mut single_tries = ctx.opts.tries.max(1);
                        single_loop(&ctx, &mut single_tries, Some(total), false).await;
                    }
                }
            }
            Err(_) => {
                single_loop(&ctx, &mut tries, None, true).await;
            }
        },
        StartMode::Resume(st) => {
            let total = st.total;
            if multi_loop(&ctx, total, Some(st), connections, &mut tries).await {
                let mut single_tries = ctx.opts.tries.max(1);
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
    let n_workers = queue
        .lock()
        .expect("Grab: piece queue poisoned (bug)")
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
                let piece = queue
                    .lock()
                    .expect("Grab: piece queue poisoned (bug)")
                    .pop_front();
                let Some((idx, s, e)) = piece else { break };
                match fetch_piece(ctx, s, e, total).await {
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
                    Err(AttemptFail::Changed(msg)) => {
                        first_err
                            .lock()
                            .expect("Grab: error slot poisoned (bug)")
                            .get_or_insert(msg);
                        changed.store(true, Ordering::SeqCst);
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
            ctx.tx
                .send(EngineMsg::Progress {
                    downloaded,
                    total: Some(total),
                })
                .ok();
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
                if let Some(r) = rate {
                    paced += bytes.len() as u64;
                    let wait = paced as f64 / r as f64 - pace_start.elapsed().as_secs_f64();
                    if wait > 0.0 {
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    }
                }
                if last_sent.elapsed() >= Duration::from_millis(100) {
                    rate = live_rate_limit();
                    ctx.tx
                        .send(EngineMsg::Progress {
                            downloaded,
                            total: Some(total),
                        })
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
                .ok();
            Ok::<u64, String>(written)
        }
    };
    let (wres, _) = tokio::join!(writer, futures_util::future::join_all(workers));
    let written = wres.map_err(AttemptFail::Retryable)?;
    if changed.load(Ordering::SeqCst) {
        let msg = first_err
            .lock()
            .expect("Grab: error slot poisoned (bug)")
            .take()
            .unwrap_or_else(|| "Download interrupted".to_string());
        return Err(AttemptFail::Changed(msg));
    }
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

/// A fresh run found someone else's file at our path (it appeared after
/// dedupe): the pump requeues under a fresh name instead of failing.
const DEST_EXISTS: &str = "Destination already exists";

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
        let mut req = ctx.client.get(&ctx.url);
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
        }
        if !ctx.opts.user_agent.trim().is_empty() {
            req = req.header("User-Agent", ctx.opts.user_agent.trim());
        }
        let resp = match tokio::time::timeout(
            ctx.timeout,
            ctx.client.execute(req.build().map_err(|e| e.to_string())?),
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
                let mut hreq = ctx.client.head(&ctx.url);
                if !ctx.opts.user_agent.trim().is_empty() {
                    hreq = hreq.header("User-Agent", ctx.opts.user_agent.trim());
                }
                if let Ok(built) = hreq.build() {
                    if let Ok(Ok(hresp)) =
                        tokio::time::timeout(ctx.timeout, ctx.client.execute(built)).await
                    {
                        if hresp.status().is_success()
                            && hresp.content_length() == Some(start)
                            && !has_holes(&ctx.dest)
                        {
                            return Ok(());
                        }
                    }
                }
            }
            if restarted {
                // Even a plain GET gets 416: pathological server, stop looping.
                return Err("Server rejects range requests".to_string());
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
            return Err("Server returned an unexpected file size".to_string());
        }
        // Unprobed resume the server answers from zero with a SMALLER object
        // than what we hold: a different file (login wall, throttle page),
        // not our download. Fail loudly and keep the partial bytes instead
        // of truncating them away for it.
        if !partial && start > 0 {
            if let Some(l) = resp.content_length() {
                if l < start {
                    return Err("Server restarted the download with a smaller file".to_string());
                }
            }
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
        if !partial {
            if let Some(name) = resp
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(filename_from_content_disposition)
            {
                ctx.tx.send(EngineMsg::SuggestName(name)).ok();
            }
        }
        let mut downloaded = if partial { start } else { 0 };
        ctx.tx.send(EngineMsg::Progress { downloaded, total }).ok();
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
                Err(_) => return Err("Stalled connection timed out".to_string()),
            };
            if chunk.is_empty() {
                empty_streak += 1;
                if empty_streak > 32 {
                    return Err("Stalled connection timed out".to_string());
                }
                continue;
            }
            empty_streak = 0;
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Cannot write file: {e}"))?;
            downloaded += chunk.len() as u64;
            if let Some(r) = rate {
                paced += chunk.len() as u64;
                let wait = paced as f64 / r as f64 - pace_start.elapsed().as_secs_f64();
                if wait > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
            }
            if last_sent.elapsed() >= Duration::from_millis(100) {
                rate = live_rate_limit();
                ctx.tx.send(EngineMsg::Progress { downloaded, total }).ok();
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

/// Parse a speed limit like `500K`, `2M`, `1.5G` (or plain bytes) into
/// bytes/sec. `None` means unlimited (empty, `0`) or invalid.
pub(crate) fn parse_rate(s: &str) -> Option<u64> {
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

/// Live speed cap in bytes/sec (0 = unlimited), applied per download: every
/// engine paces to the full value. One atomic serves all engines because the
/// preference is single: the settings watch publishes, pacing loops read
/// each tick. `gio::Settings` is main-thread-only (`!Send`), hence the hop.
static LIVE_RATE_LIMIT: AtomicU64 = AtomicU64::new(0);

fn publish_rate_limit(settings: &gio::Settings) {
    LIVE_RATE_LIMIT.store(
        parse_rate(settings.string("speed-limit").as_str()).unwrap_or(0),
        Ordering::Relaxed,
    );
}

fn live_rate_limit() -> Option<u64> {
    match LIVE_RATE_LIMIT.load(Ordering::Relaxed) {
        0 => None,
        r => Some(r),
    }
}

pub(crate) fn fmt_bytes(n: u64) -> String {
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

/// "900 MB of 2.0 GB" for progress rows (Files copy-dialog convention).
fn format_amounts(downloaded: u64, total: u64) -> String {
    format!("{} of {}", fmt_bytes(downloaded), fmt_bytes(total))
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

/// Smallest piece size: everything at or under ~4 GB splits into 1 MB
/// pieces, so one slow connection only ever delays the tail by ~1 MB.
const PIECE_MIN: u64 = 1024 * 1024;
/// Largest piece size: bounds per-request overhead on huge files without
/// starving the work-stealing queue (still thousands of pieces).
const PIECE_MAX: u64 = 16 * 1024 * 1024;
/// Pieces per download to aim for; beyond this the piece size grows.
const PIECE_TARGET_COUNT: u64 = 4096;

/// Byte range each segmented piece covers. Pure function of the total, so
/// persisted bitmaps stay valid across restarts: DO NOT change the formula
/// without a queue migration (restore drops mismatched bitmaps to a safe
/// single-stream resume instead of corrupting).
fn piece_len(total: u64) -> u64 {
    total
        .div_ceil(PIECE_TARGET_COUNT)
        .clamp(PIECE_MIN, PIECE_MAX)
}
/// A file is split only when it holds at least this much per connection
/// (aria2-style: connections x MIN_SEGMENT), keeping small downloads on the
/// cheaper single-stream path.
const MIN_SEGMENT: u64 = 4 * 1024 * 1024;
/// Largest server-claimed size eligible for splitting. A lying Content-Range
/// would otherwise size a bitmap and sparse file to absurdity; above this,
/// downloads stay single-stream (which preallocates nothing).
const MAX_SEGMENTED_TOTAL: u64 = 1 << 40;
/// Per-piece fetch attempts before a worker gives up on it.
const PIECE_TRIES: u32 = 3;

/// How many connections a download may use: at least 2 to bother splitting,
/// at most 16, and never more than one per MIN_SEGMENT of file.
fn split_count(total: u64, connections: usize) -> usize {
    (connections.max(1) as u64).min(total / MIN_SEGMENT).min(16) as usize
}

/// Split `total` bytes into `piece_len` `(start, end)` pieces (inclusive
/// ends). Empty when the file is too small — or too big to trust — to
/// split: caller uses single-stream.
fn plan_pieces(total: u64, connections: usize) -> Vec<(u64, u64)> {
    if total > MAX_SEGMENTED_TOTAL || split_count(total, connections) < 2 || total == 0 {
        return Vec::new();
    }
    let piece = piece_len(total);
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + piece).min(total) - 1;
        pieces.push((start, end));
        start = end + 1;
    }
    pieces
}

/// Resume bitmap for one segmented download, persisted in the queue file
/// (same JSON shape) so restarts resume segmented instead of starting over.
/// Piece bitmap: `done[i]` covers
/// `[i * piece_len(total), min((i+1) * piece_len(total), total))`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SegmentState {
    total: u64,
    done: Vec<bool>,
}

impl SegmentState {
    fn new(total: u64) -> Self {
        Self {
            total,
            done: vec![false; total.div_ceil(piece_len(total)) as usize],
        }
    }

    fn mark(&mut self, idx: u64) {
        if let Some(slot) = self.done.get_mut(idx as usize) {
            *slot = true;
        }
    }

    /// Missing `(piece index, start, end)` ranges, in order.
    fn missing(&self) -> Vec<(u64, u64, u64)> {
        let piece = piece_len(self.total);
        let mut out = Vec::new();
        for (i, done) in self.done.iter().enumerate() {
            if !done {
                let start = i as u64 * piece;
                out.push((i as u64, start, (start + piece).min(self.total) - 1));
            }
        }
        out
    }

    /// Contiguous completed prefix, in bytes (never past `total`: the tail
    /// piece is usually short, so an uncapped count would overshoot).
    fn prefix_len(&self) -> u64 {
        (self.done.iter().take_while(|b| **b).count() as u64 * piece_len(self.total))
            .min(self.total)
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
        let piece = piece_len(self.total);
        self.done
            .iter()
            .enumerate()
            .map(|(i, d)| {
                if *d {
                    piece.min(self.total.saturating_sub(i as u64 * piece))
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
async fn ensure_sized(dest: &std::path::Path, total: u64) -> Result<(), AttemptFail> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dest)
        .await
        .map_err(|e| AttemptFail::Retryable(format!("Cannot write file: {e}")))?;
    if file.metadata().await.map(|m| m.len()).unwrap_or(u64::MAX) != total {
        if let Err(e) = file.set_len(total).await {
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
    }
    Ok(())
}

/// Rename without clobbering: `std::fs::rename` silently replaces the
/// destination. Prefers `renameat2(RENAME_NOREPLACE)` (atomic on any
/// filesystem, FAT included); falls back to claiming `new` with a hard
/// link, and to a checked plain rename only where neither exists.
fn rename_noreplace(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    match rename_noreplace_sys(old, new) {
        // Ancient kernels (< 3.15) lack renameat2: use the portable path.
        // ENOSYS is 38 in the Linux UAPI (asm-generic and x86 alike).
        Err(e) if e.raw_os_error() == Some(38) => {}
        r => return r,
    }
    match std::fs::hard_link(old, new) {
        Ok(()) => std::fs::remove_file(old),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) if !new.exists() => std::fs::rename(old, new),
        Err(e) => Err(e),
    }
}

/// `renameat2(olddirfd, old, newdirfd, new, RENAME_NOREPLACE)` without a
/// libc dependency: one syscall, three stable constants.
#[cfg(target_os = "linux")]
fn rename_noreplace_sys(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    extern "C" {
        fn renameat2(
            olddirfd: std::os::raw::c_int,
            oldpath: *const std::os::raw::c_char,
            newdirfd: std::os::raw::c_int,
            newpath: *const std::os::raw::c_char,
            flags: std::os::raw::c_uint,
        ) -> std::os::raw::c_int;
    }
    const AT_FDCWD: std::os::raw::c_int = -100;
    const RENAME_NOREPLACE: std::os::raw::c_uint = 1; // renameat2(2)
                                                      // Queue/dedupe names never contain NUL (sane_filename), but fail
                                                      // visibly instead of truncating if one ever slips through.
    let cvt = |p: &std::path::Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let (old, new) = (cvt(old)?, cvt(new)?);
    // SAFETY: NUL-terminated buffers outlive the call; the rest are integers.
    let r = unsafe {
        renameat2(
            AT_FDCWD,
            old.as_ptr(),
            AT_FDCWD,
            new.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
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
    parse_content_range(&value)
        .filter(|(s, _, _)| *s == 0)
        .map(|(_, _, t)| t)
        .ok_or_else(|| "Bad Content-Range".to_string())
}

/// Parse `Content-Range: bytes <start>-<end>/<total>`. Callers pin the
/// fields they require: a wrong start or end means the server answered a
/// different range than asked (pins all chunks to one file version).
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
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
fn response_total(
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
fn rejects_unexpected_restart(
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
enum AttemptFail {
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
async fn fetch_piece(
    ctx: &FetchCtx,
    start: u64,
    end: u64,
    total: u64,
) -> Result<Vec<u8>, AttemptFail> {
    let timeout = ctx.timeout;
    use AttemptFail::{Changed, Retryable, Throttled};
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
                return Err(Changed("File changed on server".to_string()));
            }
            // Unparseable or wrong range: the host ignores ranges, so
            // downgrade to single-stream instead of retrying to Failed.
            _ => {
                return Err(Throttled("Server ignored range request".to_string()));
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
                break Some(Retryable("Piece stalled".to_string()));
            }
            let idle = tokio::time::sleep(quiet_limit.saturating_sub(last_progress.elapsed()));
            tokio::pin!(idle);
            tokio::select! {
                _ = &mut idle => {
                    break Some(Retryable("Piece stalled".to_string()))
                }
                next = tokio::time::timeout(timeout, stream.next()) => match next {
                    Ok(Some(Ok(c))) => {
                        if body.len() + c.len() > (end - start + 1) as usize {
                            break Some(Retryable("Server sent too much data".to_string()));
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
                        break Some(Retryable("Stalled connection timed out".to_string()))
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
            last_err = Retryable("Incomplete piece".to_string());
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
    /// Server-advertised names waiting for their download to finish. The
    /// move happens at Finished so the engine never writes through a
    /// renamed path mid-transfer (stale size reads, split files).
    pending_names: RefCell<HashMap<u64, String>>,
    /// Cached count of queued rows; backs the per-row Queue button without
    /// scanning the store on every progress tick. Refreshed in changed(),
    /// which follows every status transition (progress-only updates change
    /// no statuses, so they need no recount).
    queued: Cell<usize>,
    /// Spawn generation per row, bumped on every engine start. A pump
    /// future whose generation is stale (defer or a quick pause-resume
    /// started a newer engine first) must not touch progress, status,
    /// notifications, or the new engine's handle.
    epoch: RefCell<HashMap<u64, u64>>,
    /// Resume bitmaps for segmented downloads (session-only, main thread).
    segment_state: RefCell<HashMap<u64, SegmentState>>,
    /// Set by shutdown(): stale engine futures must not re-persist or
    /// re-mark rows once the authoritative shutdown persist has run.
    draining: Cell<bool>,
}

/// Queue + engine owner: persists the queue, spawns downloads, notifies the UI.
impl DownloadManager {
    /// Create a manager over `store`; call [`DownloadManager::restore_queue`] once.
    pub fn new(store: gio::ListStore, settings: gio::Settings) -> Rc<Self> {
        let this = Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
            batch: Cell::new(false),
            pending_names: RefCell::new(HashMap::new()),
            queued: Cell::new(0),
            epoch: RefCell::new(HashMap::new()),
            segment_state: RefCell::new(HashMap::new()),
            draining: Cell::new(false),
        });
        // Live preferences: raising the download limit must wake queued
        // rows now (nothing else re-runs start_next until the next
        // insert/finish event); lowering it parks the newest running rows
        // back to queued. Speed edits republish the shared engine cap.
        // Weak ref: the settings object would otherwise keep the
        // manager alive forever.
        //
        // Owner-thread guard: engine futures are bound to the thread that
        // spawned them, so queue actions must only run where the manager
        // was created. Invariant: in production every settings write
        // originates on the main thread (preferences UI, dconf dispatch),
        // so this never skips there; foreign-thread writes only happen
        // through the test suite's shared memory backend, where reacting
        // would spawn engines on the wrong thread.
        let owner = std::thread::current().id();
        let weak = Rc::downgrade(&this);
        this.settings
            .connect_changed(Some("max-concurrent"), move |_, _| {
                if std::thread::current().id() != owner {
                    return;
                }
                if let Some(m) = weak.upgrade() {
                    m.start_next();
                    m.preempt_excess();
                    m.changed();
                }
            });
        let settings_weak = this.settings.downgrade();
        this.settings
            .connect_changed(Some("speed-limit"), move |_, _| {
                if let Some(s) = settings_weak.upgrade() {
                    publish_rate_limit(&s);
                    crate::torrent::apply_live_limits(parse_rate(s.string("speed-limit").trim()));
                }
            });
        this
    }

    /// UI refresh callback, invoked after every state change.
    pub fn set_on_change(&self, cb: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(cb));
    }

    /// Delay queue persists across bulk inserts (URL-list import): each
    /// `enqueue` otherwise rewrites + fsyncs the whole queue file, turning
    /// a 1000-line import into 1000 full rewrites. Pair with `end_batch`.
    pub fn begin_batch(&self) {
        self.batch.set(true);
    }

    /// Persist once after a `begin_batch` block and refresh the UI.
    pub fn end_batch(self: &Rc<Self>) {
        self.batch.set(false);
        self.persist_queue();
        self.changed();
    }

    fn changed(&self) {
        self.queued.set(
            (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .filter(|it| it.status() == DownloadStatus::Queued)
                .count(),
        );
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
        // An explicit destination must be absolute: a relative dir would
        // resolve against the launcher CWD (and fail the sandbox). Restore
        // already rejects these; live input gets the same gate.
        let dir = dest_dir
            .filter(|s| !s.is_empty() && std::path::Path::new(s).is_absolute())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.effective_download_dir());
        let name = filename
            .filter(|s| sane_filename(s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if crate::torrent::is_magnet(&url) {
                    // The real name arrives with metadata; the info-hash stub
                    // labels the row until SuggestName renames it.
                    crate::torrent::stub_name(&url).unwrap_or_else(|| filename_from_url(&url))
                } else {
                    filename_from_url(&url)
                }
            });
        let name = shorten_filename(&name);
        let name = dedupe_filename(&name, |n| {
            std::path::Path::new(&dir).join(n).exists()
                || (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .any(|it| it.dest_dir() == dir && it.filename() == n)
        });
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        Ok(self.insert(item))
    }

    /// Intake for .torrent files: archive the bytes, then enqueue the
    /// pseudo-URL like any other download (stub from the file stem, real
    /// name arrives with metadata via SuggestName).
    ///
    /// # Errors
    /// Returns a display-ready message when the bytes are not a valid torrent.
    pub fn enqueue_torrent_file(
        self: &Rc<Self>,
        bytes: Vec<u8>,
        file_name: &str,
        dest_dir: Option<&str>,
        only_files: Option<Vec<usize>>,
    ) -> Result<DownloadItem, String> {
        let pseudo = crate::torrent::archive_torrent_file(file_name, &bytes)?;
        if let Some(sel) = only_files {
            crate::torrent::stage_selection(&pseudo, sel);
        }
        let stub = crate::torrent::stub_name_for_file(file_name);
        self.enqueue(&pseudo, dest_dir, Some(&stub))
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
        if !sane_filename(filename) {
            return Err(format!("Invalid filename in queue: {filename}"));
        }
        if !std::path::Path::new(dest_dir).is_absolute() {
            return Err(format!("Invalid destination in queue: {dest_dir}"));
        }
        let filename = shorten_filename(filename);
        let item = DownloadItem::new(self.alloc_id(), &url, &filename, dest_dir);
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
            tracing::warn!("skipping history entry with bad URL");
            return;
        };
        if !sane_filename(&name) {
            tracing::warn!("skipping invalid history entry for {name}");
            return;
        }
        if !std::path::Path::new(&dir).is_absolute() {
            tracing::warn!("skipping history entry with relative destination");
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

    /// Whether closing over active downloads notifies.
    pub fn background_notifications_enabled(&self) -> bool {
        self.settings.boolean("notify-background")
    }

    /// Configured folder, or the system Downloads folder when empty or
    /// relative (a relative dir would resolve against the launcher CWD).
    pub fn effective_download_dir(&self) -> String {
        let configured = self.settings.string("download-dir").to_string();
        if !configured.is_empty() && std::path::Path::new(&configured).is_absolute() {
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
        // Fail fast on junk; the running engine follows the live value,
        // so later edits apply without re-queueing.
        let limit = opts.limit_rate.trim();
        if !limit.is_empty() && limit != "0" && parse_rate(limit).is_none() {
            item.set_status(DownloadStatus::Failed);
            item.set_detail(format!("Invalid speed limit: {limit}"));
            self.changed();
            return;
        }
        publish_rate_limit(&self.settings);
        let url = item.url().to_string();
        if crate::torrent::is_torrent(&url) {
            return self.spawn_torrent(item, url);
        }
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
        let gen = self.epoch.borrow().get(&item.id()).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(item.id(), gen);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = FetchCtx {
            client: http_client(),
            url,
            dest,
            opts,
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

        let item_id = item.id();
        self.pump(item, item_id, gen, rx);
    }

    /// Drain one engine's message channel into its row. Shared by the HTTP
    /// and torrent engines: every arm below is engine-generic, so arms the
    /// other engine never sends simply never fire.
    fn pump(
        self: &Rc<Self>,
        item: DownloadItem,
        id: u64,
        gen: u64,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
    ) {
        let this = Rc::clone(self);
        // Speed baseline: deltas from here, not totals from zero. Resumes
        // seed `downloaded` with pre-existing bytes, which lifetime-average
        // math would otherwise report as fantasy GB/s on the first updates.
        let mut base: Option<(u64, Instant)> = None;
        // Set on Finished/Failed. If the channel closes first, the engine
        // task died without reporting (panic): fail the row instead of
        // stranding it as "Downloading" forever.
        let mut done = false;
        glib::spawn_future_local(async move {
            while let Some(msg) = rx.recv().await {
                // Superseded by a newer spawn for this row: its progress
                // reports would drag the bar backwards, and its tail would
                // fail the row or steal the new engine's handle.
                if !this.is_current(id, gen) {
                    break;
                }
                match msg {
                    EngineMsg::Progress { downloaded, total } => {
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let (d0, tb) = match base {
                            Some((d0, tb)) if downloaded >= d0 => (d0, tb),
                            _ => {
                                let b = (downloaded, Instant::now());
                                base = Some(b);
                                b
                            }
                        };
                        let bps = downloaded.saturating_sub(d0) as f64
                            / Instant::now().duration_since(tb).as_secs_f64().max(0.001);
                        let speed = format!("{}/s", fmt_bytes(bps as u64));
                        match total {
                            Some(t) if t > 0 => {
                                let frac = (downloaded as f64 / t as f64).clamp(0.0, 1.0);
                                item.set_progress(frac);
                                let eta = if bps > 0.0 {
                                    fmt_eta((t.saturating_sub(downloaded) as f64 / bps) as u64)
                                } else {
                                    "—".to_string()
                                };
                                item.set_detail(format!(
                                    "{}% ({}) • {} • ETA {}",
                                    (frac * 100.0) as u64,
                                    format_amounts(downloaded, t),
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
                            // A pause keeps its pending name: resumed
                            // single-stream attempts never re-suggest.
                            let pending = this.pending_names.borrow_mut().remove(&id);
                            if let Some(name) = pending {
                                let current = item.filename().to_string();
                                let dir = item.dest_dir().to_string();
                                let taken = |n: &str| {
                                    std::path::Path::new(&dir).join(n).exists()
                                        || (0..this.store.n_items())
                                            .filter_map(|i| {
                                                this.store.item(i).and_downcast::<DownloadItem>()
                                            })
                                            .any(|it| it.dest_dir() == dir && it.filename() == n)
                                };
                                // Claim-then-move so a file appearing between the
                                // dedupe check and the rename is never clobbered:
                                // retry with a fresh deduped name instead.
                                let old_path = item.file_path();
                                let mut final_name = dedupe_filename(&name, taken);
                                let mut moved = !old_path.exists();
                                for _ in 0..8 {
                                    if moved
                                        || old_path == std::path::Path::new(&dir).join(&final_name)
                                    {
                                        break;
                                    }
                                    match rename_noreplace(
                                        &old_path,
                                        &std::path::Path::new(&dir).join(&final_name),
                                    ) {
                                        Ok(()) => {
                                            moved = true;
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                            final_name = dedupe_filename(&name, taken);
                                        }
                                        Err(_) => break,
                                    }
                                }
                                if moved && final_name != current {
                                    item.set_filename(final_name);
                                }
                            }
                            // Size off the final path: the engine measured the
                            // pre-rename one.
                            let size = std::fs::metadata(item.file_path())
                                .map(|m| m.len())
                                .unwrap_or(size);
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
                        done = true;
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        // A failed download keeps its URL-derived name.
                        this.pending_names.borrow_mut().remove(&id);
                        if e == DEST_EXISTS
                            && item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // A foreign file appeared at our path after
                            // dedupe: pick a fresh free name and requeue
                            // instead of failing. The new name is free by
                            // construction, so this terminates.
                            let dir = item.dest_dir().to_string();
                            let current = item.filename().to_string();
                            let new_name = dedupe_filename(&current, |n| {
                                std::path::Path::new(&dir).join(n).exists()
                                    || (0..this.store.n_items())
                                        .filter_map(|i| {
                                            this.store.item(i).and_downcast::<DownloadItem>()
                                        })
                                        .any(|it| it.dest_dir() == dir && it.filename() == n)
                            });
                            item.set_filename(new_name);
                            item.set_status(DownloadStatus::Queued);
                            this.persist_queue();
                            this.changed();
                            this.start_next();
                            done = true;
                            break;
                        }
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Failed);
                            item.set_detail(e.clone());
                            this.notify_finished(&item, false, Some(e));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::FailedVersion(e) => {
                        // The bytes on the server changed mid-download: any
                        // resume bitmap describes a dead file version, so
                        // drop it. The next attempt (or Retry) starts fresh
                        // and re-probes instead of failing forever.
                        this.segment_state.borrow_mut().remove(&id);
                        this.pending_names.borrow_mut().remove(&id);
                        if item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            item.set_status(DownloadStatus::Failed);
                            item.set_detail(e.clone());
                            this.notify_finished(&item, false, Some(e));
                        }
                        done = true;
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
                        // Stash the server-advertised name for adoption at
                        // Finished. Moving mid-transfer would desync the
                        // engine (stale size reads, split files), so only
                        // fresh single-stream attempts suggest at all.
                        if item.status() != DownloadStatus::Downloading {
                            continue;
                        }
                        let current = item.filename().to_string();
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        // Chromium parity: the server-advertised name wins
                        // over the URL-derived one. Adopt when the current
                        // name is a placeholder/extensionless, or when the
                        // server name has an extension and is shorter (long
                        // tracked/tokenized URL names lose to the real file).
                        // A generic extensionless server name never clobbers
                        // a good URL-derived name.
                        let placeholder = current == "index.html" || !current.contains('.');
                        if !placeholder && !(name.contains('.') && name.len() < current.len()) {
                            continue;
                        }
                        let name = shorten_filename(&name);
                        if name == current {
                            continue;
                        }
                        this.pending_names.borrow_mut().insert(id, name);
                    }
                }
            }
            // Superseded pump future: touch nothing, especially not the
            // new engine's handle in `running`.
            if !this.is_current(id, gen) {
                return;
            }
            this.running.borrow_mut().remove(&id);
            if this.draining.get() {
                return;
            }
            if !done && item.status() == DownloadStatus::Downloading {
                item.set_status(DownloadStatus::Failed);
                item.set_detail("Download interrupted".to_string());
                this.notify_finished(&item, false, Some("Download interrupted".to_string()));
            }
            this.persist_queue();
            this.changed();
            this.start_next();
        });
    }

    /// Spawn the torrent engine for a magnet row. Mirrors `spawn`'s contract
    /// (epoch bump, running slot, Downloading status, shared pump) so pause,
    /// cancel, retry, persist and the stale-pump guard keep working unchanged.
    fn spawn_torrent(self: &Rc<Self>, item: DownloadItem, magnet: String) {
        let dir = std::path::PathBuf::from(item.dest_dir().to_string());
        let _ = std::fs::create_dir_all(&dir);
        let settings = &self.settings;
        let seed_finished = settings.boolean("torrent-seed-finished");
        let dht = settings.boolean("torrent-dht");
        let peer_limit = crate::torrent::peer_limit_of(settings);
        let download_bps = parse_rate(settings.string("speed-limit").trim());
        let id = item.id();
        let gen = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, gen);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let source = if crate::torrent::is_torrent_url(&magnet) {
            match crate::torrent::archive_path_for_url(&magnet) {
                Some(path) => crate::torrent::TorrentSource::File(path),
                None => {
                    item.set_status(DownloadStatus::Failed);
                    item.set_detail("Torrent file is missing from the archive".to_string());
                    self.changed();
                    return;
                }
            }
        } else {
            crate::torrent::TorrentSource::Magnet(magnet)
        };
        let only_files = crate::torrent::get_selection(&item.url().to_string());
        let handle = tokio_rt().spawn(crate::torrent::run_torrent(crate::torrent::TorrentJob {
            id,
            source,
            dest: dir,
            seed_finished,
            dht,
            peer_limit,
            download_bps,
            only_files,
            tx,
        }));
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        item.set_detail("Starting torrent…".to_string());
        self.changed();
        self.pump(item, id, gen, rx);
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
            let mut body = if ok {
                item.filename().to_string()
            } else {
                format!(
                    "{} → {}",
                    item.filename(),
                    item.file_path().to_string_lossy()
                )
            };
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
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
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

    /// Send a downloading/paused row to the back of the queue, keeping its
    /// progress and partial file. The freed slot goes to the longest-waiting
    /// queued row.
    pub fn defer(self: &Rc<Self>, id: u64) {
        let Some(item) = self.find(id) else {
            return;
        };
        if !matches!(
            item.status(),
            DownloadStatus::Downloading | DownloadStatus::Paused
        ) {
            return;
        }
        self.park(id);
        self.move_to_back(id);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Stop the engine for `id`, keeping file, bitmap and progress, and
    /// mark it queued. Unlike pause the row yields its slot; unlike cancel
    /// nothing is deleted and progress is kept.
    fn park(&self, id: u64) {
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            if crate::torrent::is_torrent(&item.url()) {
                crate::torrent::pause_download(id);
            }
            if matches!(
                item.status(),
                DownloadStatus::Downloading | DownloadStatus::Paused
            ) {
                item.set_status(DownloadStatus::Queued);
                item.set_detail("Queued".to_string());
            }
        }
    }

    fn move_to_back(&self, id: u64) {
        let pos = (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<DownloadItem>()
                .map(|it| it.id() == id)
                .unwrap_or(false)
        });
        if let Some(pos) = pos {
            if let Some(obj) = self.store.item(pos) {
                self.store.remove(pos);
                self.store.append(&obj);
            }
        }
    }

    /// Park running rows past the shrunk limit, lowest ids (earliest
    /// enqueued) keep their slots. Id order approximates start order;
    /// parked rows keep progress and resume later either way.
    fn preempt_excess(&self) {
        let max = self.max_concurrent();
        let mut ids: Vec<u64> = self.running.borrow().keys().cloned().collect();
        if ids.len() <= max {
            return;
        }
        ids.sort_unstable();
        for id in ids.into_iter().skip(max) {
            self.park(id);
        }
        self.persist_queue();
        self.changed();
    }

    /// Cancel a download; retry with [`DownloadManager::retry`].
    pub fn cancel(self: &Rc<Self>, id: u64) {
        self.cancel_inner(id);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    fn cancel_inner(&self, id: u64) {
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        self.pending_names.borrow_mut().remove(&id);
        let had_segments = self.segment_state.borrow_mut().remove(&id).is_some();
        if let Some(item) = self.find(id) {
            if had_segments {
                let _ = std::fs::remove_file(item.file_path());
            }
            // Unfinished torrent rows drop their session entry but keep
            // partial files, so cancel/retry and remove/Undo resume instead
            // of restarting (explicit delete discards the files instead).
            if crate::torrent::is_torrent(&item.url()) && item.status() != DownloadStatus::Done {
                crate::torrent::forget_download(id, false);
            }
            item.set_status(DownloadStatus::Cancelled);
            item.set_detail("Cancelled".to_string());
        }
    }

    /// Re-queue a failed or cancelled download.
    pub fn retry(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id) {
            match item.status() {
                DownloadStatus::Failed | DownloadStatus::Cancelled => {
                    // A kept segment bitmap resumes where it left off, so
                    // leave the progress bar there instead of flashing 0%.
                    if !self.segment_state.borrow().contains_key(&id) {
                        item.set_progress(0.0);
                    }
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
        self.cancel_inner(id);
        self.epoch.borrow_mut().remove(&id);
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
        self.start_next();
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
        // Torrent rows (any status): drop the session entry and the
        // archive, drop the row, then Trash the real files (single file or
        // torrent subfolder — the stub path was never written, gio trash
        // handles both; a missing path is fine when metadata never
        // resolved). Matches the HTTP delete contract (Trash, recoverable).
        if crate::torrent::is_torrent(&item.url()) {
            let path = item.file_path();
            crate::torrent::forget_download(id, false);
            crate::torrent::delete_archive_for_url(&item.url());
            self.remove(id);
            return match gio::File::for_path(path).trash(gio::Cancellable::NONE) {
                Ok(()) => Ok(()),
                Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {
                    Ok(())
                }
                Err(e) => Err(format!("Could not move {} to Trash: {e}", item.filename())),
            };
        }
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
        // One persist/sync at the end, and no per-row start_next: cancel()
        // would briefly spawn the next queued row only to cancel it right
        // after, leaving stray engine tasks and UI futures behind.
        self.for_matching(
            |s| {
                matches!(
                    s,
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            },
            |m, id| m.cancel_inner(id),
        );
        self.persist_queue();
        self.changed();
        self.start_next();
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

    /// Whether `gen` is still the row's latest engine spawn.
    fn is_current(&self, id: u64, gen: u64) -> bool {
        self.epoch.borrow().get(&id).cloned().unwrap_or(0) == gen
    }

    /// Rows waiting queued (cached; see `queued`).
    pub fn queued_count(&self) -> usize {
        self.queued.get()
    }

    /// Whether any item is queued, downloading or paused.
    pub fn has_active(&self) -> bool {
        self.active_count() > 0
    }

    /// Rows `cancel_all` would touch (queued, downloading, paused).
    pub fn active_count(&self) -> usize {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            })
            .count()
    }

    /// Whether anything is actually transferring (queued or downloading).
    /// Paused items don't count: closing over only-paused downloads quits
    /// instead of hiding to a "background" notification.
    pub fn has_transferring(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Queued | DownloadStatus::Downloading))
    }

    /// Whether any item failed or was cancelled.
    pub fn has_failed(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed | DownloadStatus::Cancelled))
    }

    /// Whether any item genuinely failed. User-cancelled rows need no
    /// error banner: cancelling was deliberate, and Retry Failed in the
    /// menu still resurrects them via `has_failed`.
    pub fn has_errored(&self) -> bool {
        self.any_status(|s| matches!(s, DownloadStatus::Failed))
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

    /// Move a broken queue file aside (`queue.json.bak`) so its bytes
    /// survive for inspection and the next persist starts fresh.
    fn quarantine_queue() {
        let bak = Self::queue_file().with_extension("json.bak");
        if let Err(e) = std::fs::rename(Self::queue_file(), &bak) {
            tracing::error!("could not quarantine download queue: {e}");
        }
    }

    fn persist_queue(&self) {
        if self.batch.get() {
            return;
        }
        let mut items = Vec::new();
        for i in 0..self.store.n_items() {
            if let Some(it) = self.store.item(i).and_downcast::<DownloadItem>() {
                if let Some(status) = StoredStatus::from_item(it.status()) {
                    let segments = self.segment_state.borrow().get(&it.id()).cloned();
                    let selected_files = crate::torrent::get_selection(&it.url().to_string());
                    items.push(StoredItem {
                        url: it.url().to_string(),
                        dest_dir: it.dest_dir().to_string(),
                        filename: it.filename().to_string(),
                        status,
                        progress: it.progress(),
                        segments,
                        selected_files,
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
                tracing::error!("could not serialize download queue: {e}");
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
                    tracing::error!("could not replace download queue: {e}");
                    return;
                }
                if let Some(parent) = Self::queue_file().parent() {
                    if let Ok(dir) = std::fs::File::open(parent) {
                        let _ = dir.sync_all();
                    }
                }
            }
            Err(e) => tracing::error!("could not persist download queue: {e}"),
        }
    }

    /// Load the persisted queue (cap: 1000 items / 10 MB), then resume.
    /// Unusable files are moved to `queue.json.bak` (not deleted), so a
    /// single bad write can never silently wipe the whole queue.
    pub fn restore_queue(self: &Rc<Self>) {
        if Self::queue_file().exists() {
            const MAX_QUEUE_BYTES: u64 = 10_000_000;
            const MAX_QUEUE_ITEMS: usize = 1000;
            if std::fs::metadata(Self::queue_file())
                .map(|m| m.len() > MAX_QUEUE_BYTES)
                .unwrap_or(true)
            {
                tracing::warn!("quarantining oversized download queue");
                Self::quarantine_queue();
                return;
            };
            let Ok(text) = std::fs::read_to_string(Self::queue_file()) else {
                return;
            };
            let Ok(queue) = serde_json::from_str::<StoredQueue>(&text) else {
                tracing::warn!("quarantining unreadable download queue");
                Self::quarantine_queue();
                return;
            };
            if queue.version == 0 || queue.version > QUEUE_VERSION {
                tracing::warn!("quarantining download queue version {}", queue.version);
                Self::quarantine_queue();
                return;
            }
            // Over-cap queues keep every resumable item first, then the
            // newest history: active rows are user intent, Done rows are not.
            let mut items = queue.items;
            if items.len() > MAX_QUEUE_ITEMS {
                tracing::warn!(
                    "truncating download queue ({} items, keeping active first)",
                    items.len()
                );
                let (mut active, done): (Vec<StoredItem>, Vec<StoredItem>) = items
                    .into_iter()
                    .partition(|it| !matches!(it.status, StoredStatus::Done));
                active.truncate(MAX_QUEUE_ITEMS);
                let skip = done.len().saturating_sub(MAX_QUEUE_ITEMS - active.len());
                items = active
                    .into_iter()
                    .chain(done.into_iter().skip(skip))
                    .collect();
            }
            self.batch.set(true);
            for item in items {
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
                                    && s.total <= MAX_SEGMENTED_TOTAL
                                    && s.done.len()
                                        == s.total.div_ceil(piece_len(s.total)) as usize =>
                            {
                                Some(s)
                            }
                            _ => None,
                        };
                        match self.restore_existing(
                            &item.url,
                            &item.dest_dir,
                            &item.filename,
                            status,
                            segments,
                        ) {
                            Ok(restored) => {
                                // Re-stage the intake file selection: the
                                // live map is in-memory only, so without
                                // this a restart drops the filter and the
                                // resume downloads every file.
                                if let Some(sel) = item.selected_files {
                                    crate::torrent::stage_selection(&restored.url(), sel);
                                }
                            }
                            Err(e) => tracing::warn!("skipping queue entry: {e}"),
                        }
                    }
                }
            }
            self.batch.set(false);
            // Drop archived .torrent files no row references anymore
            // (removed rows keep theirs until now; explicit deletes drop
            // theirs at once, Finished engines drop theirs on completion).
            let referenced: std::collections::HashSet<String> = (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent_url(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            // Same for staged file selections: rows that are gone need no
            // filter on a future re-add (which stages fresh at intake).
            crate::torrent::prune_selections(&referenced);
            self.persist_queue();
            self.changed();
        }
    }

    /// Abort running tasks and persist the queue for the next launch.
    pub fn shutdown(&self) {
        self.draining.set(true);
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
        // resume segmented instead of restarting. Pump tails exit silently
        // once draining is set, so this is the only persist that matters.
        self.persist_queue();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static QUEUE_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    /// Serializes every test that iterates the shared glib default
    /// MainContext (MainLoops and drain pumps). glib futures are bound to
    /// their spawning thread: a foreign iteration polling another test's
    /// pending source aborts on the thread-affinity guard. Take AFTER
    /// QUEUE_FILE_LOCK, always that order.
    static MAIN_LOOP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Both test locks in the one safe order (queue, then loop). Take these
    /// together and never the loop lock before the queue lock: a reversed
    /// order across tests would deadlock the suite into a CI-timeout hang.
    fn test_locks() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let q = QUEUE_FILE_LOCK.lock().unwrap();
        let l = MAIN_LOOP_LOCK.lock().unwrap();
        (q, l)
    }

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

    /// Fresh loopback port per call. Parallel tests share one process (and
    /// pid), so pid-derived ports collide; a counter never repeats.
    /// Salted per process: crashed runs leak python servers that keep
    /// listening, and without the salt the next run reuses their ports and
    /// talks to stale fixtures.
    fn test_port(offset: u16) -> u16 {
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
        static SALT: OnceLock<u64> = OnceLock::new();
        let salt = *SALT.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
            std::process::id() as u64 ^ nanos ^ ((nanos >> 17) | 1)
        });
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        21000
            + ((n as u64)
                .wrapping_mul(173)
                .wrapping_add(offset as u64)
                .wrapping_add(salt)
                % 40000) as u16
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
        let port = test_port(port_offset);
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
        let _ = server.borrow_mut().kill();
        panic!("TEST FAILURE: {msg}");
    }

    /// Success-path teardown shared by the live-server tests.
    fn cleanup(server: &Rc<RefCell<std::process::Child>>, dir: &std::path::Path) {
        let _ = server.borrow_mut().kill();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Watchdog + run + quiescence drain shared by every main-loop test: the
    /// drain pumps until the context goes quiet so no pending future is left
    /// for another test's loop to trip over. Callers must hold
    /// MAIN_LOOP_LOCK: only one test may iterate at a time.
    fn run_loop(main_loop: &glib::MainLoop, watchdog_secs: u64) {
        let watchdog = main_loop.clone();
        let timed_out = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (flag, done) = (Arc::clone(&timed_out), Arc::clone(&finished));
        std::thread::spawn(move || {
            // Poll so a finished test doesn't leave us sleeping for minutes.
            for _ in 0..watchdog_secs.max(1) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if done.load(Ordering::SeqCst) || !watchdog.is_running() {
                    return;
                }
            }
            if !done.load(Ordering::SeqCst) && watchdog.is_running() {
                flag.store(true, Ordering::SeqCst);
                watchdog.quit();
            }
        });
        main_loop.run();
        finished.store(true, Ordering::SeqCst);
        assert!(!timed_out.load(Ordering::SeqCst), "TEST TIMEOUT");
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
        // Absurd server-claimed sizes never split (FIND-01): no giant
        // bitmap, no terabyte sparse file, just single-stream.
        assert!(plan_pieces(u64::MAX, 16).is_empty());
        assert!(plan_pieces(2 * (1 << 40), 16).is_empty());
        // Worker split count follows connections x 4 MB.
        assert_eq!(split_count(100 * 1024 * 1024, 4), 4);
        assert_eq!(split_count(100 * 1024 * 1024, 99), 16);
        assert_eq!(split_count(1024, 4), 0);
    }

    #[test]
    fn piece_size_scales_with_total() {
        // Floor holds through ~4 GB: everything ordinary splits as before.
        assert_eq!(piece_len(1024), PIECE_MIN);
        assert_eq!(piece_len(4 * 1024 * 1024 * 1024), PIECE_MIN);
        // Beyond that pieces grow toward ~4k per download, capped at 16 MB.
        assert_eq!(piece_len(20 * 1024 * 1024 * 1024), 5 * 1024 * 1024);
        assert_eq!(piece_len(1 << 40), PIECE_MAX);
        // Bitmap stays aligned: the piece count covers the total exactly.
        let total = 20 * 1024 * 1024 * 1024;
        let st = SegmentState::new(total);
        assert_eq!(st.done.len() as u64, total.div_ceil(piece_len(total)));
        let pieces = plan_pieces(total, 4);
        assert_eq!(pieces.len() as u64, total.div_ceil(piece_len(total)));
        assert_eq!(pieces.last().unwrap().1, total - 1);
    }

    #[test]
    fn parses_range_totals() {
        assert_eq!(parse_content_range("bytes 0-0/12345"), Some((0, 0, 12345)));
        assert_eq!(
            parse_content_range("bytes 1048576-2097151/8388608"),
            Some((1048576, 2097151, 8388608))
        );
        // Inverted range or garbage rejected.
        assert_eq!(parse_content_range("bytes 99-0/12345"), None);
        assert_eq!(parse_content_range("bytes */12345"), None);
        assert_eq!(parse_content_range("nonsense"), None);
        assert_eq!(parse_content_range("bytes 0-0/0"), None);
    }

    #[test]
    fn resolves_response_totals() {
        assert_eq!(response_total(Some(100), None, false, 0), Some(100));
        assert_eq!(response_total(Some(60), None, true, 40), Some(100));
        // Chunked 206 without Content-Length: trust Content-Range.
        assert_eq!(
            response_total(None, Some("bytes 40-99/100"), true, 40),
            Some(100)
        );
        // Range starts elsewhere, or no headers at all: unknown.
        assert_eq!(response_total(None, Some("bytes 0-0/100"), true, 40), None);
        assert_eq!(response_total(None, None, true, 40), None);
        assert_eq!(response_total(None, None, false, 0), None);
    }

    #[test]
    fn rejects_size_mismatched_restarts() {
        // Resumed range answered 200 with a different length: reject.
        assert!(rejects_unexpected_restart(false, 40, Some(100), Some(12)));
        // Same length, fresh start, or unknown lengths: proceed.
        assert!(!rejects_unexpected_restart(false, 40, Some(100), Some(100)));
        assert!(!rejects_unexpected_restart(false, 0, Some(100), Some(12)));
        assert!(!rejects_unexpected_restart(true, 40, Some(100), Some(60)));
        assert!(!rejects_unexpected_restart(false, 40, None, Some(12)));
        assert!(!rejects_unexpected_restart(false, 40, Some(100), None));
    }

    #[test]
    fn prefix_len_caps_at_total() {
        // Tail piece is short: two done pieces of a 1.5 MB file cover
        // 1.5 MB, not 2 MB (an uncapped prefix could extend the file).
        let mut st = SegmentState::new(1_500_000);
        st.mark(0);
        st.mark(1);
        assert_eq!(st.prefix_len(), 1_500_000);
    }

    #[test]
    fn content_disposition_star_case_insensitive() {
        // Servers emit FILENAME*= too; parameter names are case-insensitive.
        assert_eq!(
            filename_from_content_disposition("attachment; FILENAME*=UTF-8''%E2%82%ACrates.mp4"),
            Some("€rates.mp4".to_string())
        );
    }

    #[test]
    fn dedupe_caps_iterations() {
        // Everything taken: must still return (not stat the disk forever).
        let name = dedupe_filename("f.iso", |_| true);
        assert!(name.starts_with("f ("));
    }

    #[test]
    fn corrupt_queue_is_quarantined() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("quarantine");
        std::fs::write(&qf, b"{not json").unwrap();
        let settings = test_settings();
        let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        m.restore_queue();
        assert_eq!(m.store().n_items(), 0);
        let bak = qf.with_extension("json.bak");
        assert!(bak.exists());
        let _ = std::fs::remove_file(&bak);
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn rename_noreplace_never_clobbers() {
        let dir = std::env::temp_dir().join(format!("grab-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.bin"), dir.join("b.bin"), dir.join("c.bin"));
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(&b, b"bbb").unwrap();
        // Occupied destination: error, victim untouched.
        assert!(rename_noreplace(&a, &b).is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"bbb");
        assert_eq!(std::fs::read(&a).unwrap(), b"aaa");
        // Free destination: moved, source gone.
        assert!(rename_noreplace(&a, &c).is_ok());
        assert!(!a.exists());
        assert_eq!(std::fs::read(&c).unwrap(), b"aaa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overcap_queue_keeps_active_first() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("overcap");
        let settings = test_settings();
        let mut items = vec![
            StoredItem {
                url: "https://example.com/active.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "active.iso".to_string(),
                status: StoredStatus::Queued,
                progress: 0.0,
                segments: None,
                selected_files: None,
            },
            StoredItem {
                url: "https://example.com/paused.iso".to_string(),
                dest_dir: "/tmp/dl".to_string(),
                filename: "paused.iso".to_string(),
                status: StoredStatus::Paused,
                progress: 0.5,
                segments: None,
                selected_files: None,
            },
        ];
        for i in 0..1000 {
            items.push(StoredItem {
                url: format!("https://example.com/f{i}.iso"),
                dest_dir: "/tmp/dl".to_string(),
                filename: format!("f{i}.iso"),
                status: StoredStatus::Done,
                progress: 1.0,
                segments: None,
                selected_files: None,
            });
        }
        let queue = StoredQueue {
            version: QUEUE_VERSION,
            items,
        };
        std::fs::write(&qf, serde_json::to_string(&queue).unwrap()).unwrap();
        settings.set_int("max-concurrent", 1).unwrap();
        let m = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        // Occupy the only slot so Queued restores can't spawn downloads.
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        m.running.borrow_mut().insert(99, holder);
        m.restore_queue();
        assert_eq!(m.store().n_items(), 1000);
        let names: Vec<String> = (0..m.store().n_items())
            .filter_map(|i| m.store().item(i).and_downcast::<DownloadItem>())
            .map(|it| it.filename().to_string())
            .collect();
        // Resumable intent survives even though it was oldest in the file.
        assert!(names.contains(&"active.iso".to_string()));
        assert!(names.contains(&"paused.iso".to_string()));
        // Newest history kept, oldest history dropped.
        assert!(names.contains(&"f999.iso".to_string()));
        assert!(!names.contains(&"f0.iso".to_string()));
        assert!(!names.contains(&"f1.iso".to_string()));
        let _ = std::fs::remove_file(&qf);
    }

    #[test]
    fn forgets_bitmap_beyond_prefix() {
        let mut st = SegmentState::new(4 * PIECE_MIN);
        st.mark(0);
        st.mark(1);
        st.mark(3);
        st.forget_beyond_prefix();
        assert_eq!(
            st.missing(),
            vec![
                (2, 2 * PIECE_MIN, 3 * PIECE_MIN - 1),
                (3, 3 * PIECE_MIN, 4 * PIECE_MIN - 1),
            ]
        );
        assert_eq!(st.prefix_len(), 2 * PIECE_MIN);
    }

    #[test]
    fn truncates_to_prefix() {
        let dir = std::env::temp_dir().join(format!("grab-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.bin");
        std::fs::write(&file, vec![7u8; 3 * PIECE_MIN as usize]).unwrap();
        // Pieces 0,1 done, 2 missing: shrink to 2 MB.
        let mut st = SegmentState::new(3 * PIECE_MIN);
        st.mark(0);
        st.mark(1);
        truncate_to_prefix(&file, &st);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_MIN);
        // Already short: untouched.
        truncate_to_prefix(&file, &st);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 2 * PIECE_MIN);
        // Nothing done: emptied.
        let st = SegmentState::new(3 * PIECE_MIN);
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
    fn rate_limit_follows_settings_live() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("rate-live");
        let settings = test_settings();
        // The manager's watch publishes; the engine cap follows with no
        // re-queue. Junk reads as unlimited.
        let _manager =
            DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        settings.set_string("speed-limit", "1M").unwrap();
        assert_eq!(live_rate_limit(), Some(1024 * 1024));
        settings.set_string("speed-limit", "").unwrap();
        assert_eq!(live_rate_limit(), None);
        settings.set_string("speed-limit", "junk").unwrap();
        assert_eq!(live_rate_limit(), None);
        // Restore: the memory backend is shared across tests; a leaked
        // value would throttle or fail other tests' spawns.
        settings.set_string("speed-limit", "").unwrap();
    }

    #[test]
    fn notification_toggles() {
        // NOTE: no pristine-defaults assert here: the memory GSettings
        // backend is process-shared, so other tests' set_boolean(false)
        // calls are visible. This checks live key -> method wiring instead.
        let settings = test_settings();
        settings.set_boolean("show-notifications", true).unwrap();
        settings.set_boolean("notify-background", true).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        assert!(manager.notifications_enabled());
        assert!(manager.background_notifications_enabled());
        settings.set_boolean("notify-background", false).unwrap();
        assert!(!manager.background_notifications_enabled());
        assert!(manager.notifications_enabled());
    }

    #[test]
    fn transferring_ignores_paused() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("transferring");
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        assert!(!manager.has_transferring());
        let paused = DownloadItem::new(1, "https://example.com/a.bin", "a.bin", "/tmp/dl");
        paused.set_status(DownloadStatus::Paused);
        manager.store().append(&paused);
        assert!(manager.has_active());
        assert!(!manager.has_transferring());
        let queued = DownloadItem::new(2, "https://example.com/b.bin", "b.bin", "/tmp/dl");
        queued.set_status(DownloadStatus::Queued);
        manager.store().append(&queued);
        assert!(manager.has_transferring());
    }

    #[test]
    fn active_count_covers_cancel_all_scope() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("active-count");
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        assert_eq!(manager.active_count(), 0);
        for (id, status) in [
            (1, DownloadStatus::Queued),
            (2, DownloadStatus::Downloading),
            (3, DownloadStatus::Paused),
            (4, DownloadStatus::Done),
            (5, DownloadStatus::Failed),
            (6, DownloadStatus::Cancelled),
        ] {
            let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
            it.set_status(status);
            manager.store().append(&it);
        }
        assert_eq!(manager.active_count(), 3);
        assert!(manager.has_active());
    }

    #[test]
    fn error_banner_ignores_cancelled() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("error-banner");
        let settings = test_settings();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
        assert!(!manager.has_failed());
        assert!(!manager.has_errored());
        for id in 1..=2 {
            let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
            manager.store().append(&it);
        }
        manager.cancel_all();
        assert!(manager.has_failed());
        assert!(!manager.has_errored());
        let failed = DownloadItem::new(3, "https://example.com/g.bin", "g.bin", "/tmp/dl");
        failed.set_status(DownloadStatus::Failed);
        manager.store().append(&failed);
        assert!(manager.has_errored());
    }

    #[test]
    fn formats_amounts() {
        assert_eq!(format_amounts(0, 1024), "0 B of 1.0 KB");
        assert_eq!(
            format_amounts(5 * 1024 * 1024, 2 * 1024 * 1024 * 1024),
            "5.0 MB of 2.0 GB"
        );
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
        assert_eq!(
            filename_from_url("https://example.com/a/my%20game.zip"),
            "my game.zip"
        );
    }

    #[test]
    fn urls() {
        assert!(normalize_url("https://example.com/f.iso").is_ok());
        assert!(normalize_url("ftp://example.com/f").is_err());
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("--post-file=x").is_err());
        assert!(normalize_url("localhost:8080/f.iso").is_ok());
        // Userinfo would persist plaintext creds in queue.json: reject.
        assert!(normalize_url("https://user:pass@example.com/f.iso").is_err());
        assert!(normalize_url("https://user@example.com/f.iso").is_err());
        assert!(normalize_url("user:pass@example.com/f.iso").is_err());
    }

    #[test]
    fn magnet_links() {
        let good = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=test";
        assert_eq!(normalize_url(good).as_deref(), Ok(good));
        assert_eq!(
            normalize_url("  MAGNET:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3  ")
                .as_deref(),
            Ok("MAGNET:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3")
        );
        // Tracker-heavy magnets (Ubuntu's run ~2-4 KB) bypass the 2048
        // HTTP cap: parsed locally, never sent as a request line.
        let big = format!(
            "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3{}",
            "&tr=udp://tracker.example.com:1337/announce".repeat(100)
        );
        assert!(big.len() > MAX_URL_LEN);
        assert_eq!(normalize_url(&big).as_deref(), Ok(big.as_str()));
        // Absurd magnets still rejected.
        let huge = format!(
            "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&x={}",
            "a".repeat(16384)
        );
        assert!(normalize_url(&huge).is_err());
        // Tracker params contain `://`: must never reach the http scheme
        // branch (regression: "Unsupported scheme: magnet").
        let tracked = "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&tr=http://tracker.example.com:80/announce&tr=udp://tracker.example.com:1337/announce";
        assert_eq!(normalize_url(tracked).as_deref(), Ok(tracked));
        // Pasted BOM must not defeat the magnet classifier.
        let bom = format!("\u{feff}{good}");
        assert_eq!(normalize_url(&bom).as_deref(), Ok(good));
        // Magnet-shaped but unparseable: rejected by the parser, never by
        // the scheme branch.
        let bad_scheme =
            normalize_url("magnet://xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3");
        assert!(bad_scheme.is_err());
        assert!(!bad_scheme.unwrap_err().contains("Unsupported scheme"));
        // No BTv1 info-hash: unresolvable, reject at intake.
        assert!(normalize_url("magnet:?dn=nameless").is_err());
        assert!(normalize_url("magnet:?xt=urn:btih:xyz").is_err());
        assert!(normalize_url("magnet:").is_err());
    }

    /// Minimal single-file .torrent: info{length: 1, name: "foo"}.
    fn single_torrent_bytes() -> Vec<u8> {
        format!(
            "d8:announce12:http://t.co/4:infod6:lengthi1e4:name3:foo12:piece lengthi16384e6:pieces20:{}ee",
            "A".repeat(20)
        )
        .into_bytes()
    }

    /// Minimal multi-file .torrent: bar/{a.txt: 2, sub/b.txt: 3}.
    /// Info-dict keys must be sorted (files < name < piece length <
    /// pieces): the parser enforces canonical order.
    fn multi_torrent_bytes() -> Vec<u8> {
        format!(
            "d8:announce12:http://t.co/4:infod5:filesld6:lengthi2e4:pathl5:a.txteed6:lengthi3e4:pathl3:sub5:b.txteee4:name3:bar12:piece lengthi16384e6:pieces20:{}ee",
            "A".repeat(20)
        )
        .into_bytes()
    }

    #[test]
    fn torrent_urls() {
        // Nothing archived under that path: rejected at intake.
        assert!(normalize_url("torrent:/nope/missing.torrent").is_err());
        assert!(!crate::torrent::is_torrent_url(
            "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"
        ));
        // A real archived file round-trips through normalize.
        let stem = format!("grab-test-{}", std::process::id());
        let pseudo = crate::torrent::archive_torrent_file(
            &format!("{stem}.torrent"),
            &single_torrent_bytes(),
        )
        .unwrap();
        assert!(crate::torrent::is_torrent_url(&pseudo));
        assert!(crate::torrent::is_torrent(&pseudo));
        assert_eq!(normalize_url(&pseudo).as_deref(), Ok(pseudo.as_str()));
        crate::torrent::delete_archive_for_url(&pseudo);
        assert!(crate::torrent::archive_path_for_url(&pseudo).is_none());
    }

    #[test]
    fn torrent_file_list_parses() {
        let (name, entries) = crate::torrent::torrent_file_list(&single_torrent_bytes()).unwrap();
        assert_eq!(name, "foo");
        assert!(entries.is_empty());
        let (name, entries) = crate::torrent::torrent_file_list(&multi_torrent_bytes()).unwrap();
        assert_eq!(name, "bar");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "a.txt");
        assert_eq!(entries[0].length, 2);
        assert_eq!(entries[1].path, "sub/b.txt");
        assert_eq!(entries[1].length, 3);
        assert!(crate::torrent::torrent_file_list(b"not a torrent").is_err());
    }

    #[test]
    fn sweep_keeps_only_referenced_archives() {
        let dir = std::env::temp_dir().join(format!("grab-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keep = dir.join("keep.torrent");
        let drop = dir.join("drop.torrent");
        let skip = dir.join("notes.txt");
        std::fs::write(&keep, b"x").unwrap();
        std::fs::write(&drop, b"x").unwrap();
        std::fs::write(&skip, b"x").unwrap();
        let mut referenced = std::collections::HashSet::new();
        referenced.insert(format!("torrent:{}", keep.to_string_lossy()));
        crate::torrent::sweep_archives_in(&dir, &referenced);
        assert!(keep.exists());
        assert!(!drop.exists());
        assert!(skip.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_selection_survives_respawn() {
        // Regression: take-once lost the file filter on every re-spawn
        // (retry after cancel/fail), silently downloading everything.
        // Selections now peek until pruned with unreferenced archives.
        let url = "torrent:/tmp/grab-test-sel.torrent";
        crate::torrent::stage_selection(url, vec![2]);
        assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
        assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
        let mut referenced = std::collections::HashSet::new();
        referenced.insert(url.to_string());
        crate::torrent::prune_selections(&referenced);
        assert_eq!(crate::torrent::get_selection(url), Some(vec![2]));
        crate::torrent::prune_selections(&std::collections::HashSet::new());
        assert_eq!(crate::torrent::get_selection(url), None);
    }

    #[test]
    fn torrent_file_enqueue_uses_stem_stub() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("torrent-enqueue");
        let settings = test_settings();
        // Occupy the only slot so nothing spawns a real engine below.
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(99, holder);
        let stem = format!("grab-enqueue-{}", std::process::id());
        let item = manager
            .enqueue_torrent_file(
                single_torrent_bytes(),
                &format!("{stem}.torrent"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(item.status(), DownloadStatus::Queued);
        assert!(crate::torrent::is_torrent_url(&item.url()));
        assert!(sane_filename(&item.filename()));
        assert!(!item.filename().is_empty());
        // Leave no Queued row behind (a later restore could spawn it) and
        // no archive behind; restore the shared memory-backend key.
        manager.cancel_all();
        crate::torrent::delete_archive_for_url(&item.url());
        assert!(crate::torrent::archive_path_for_url(&item.url()).is_none());
        settings.set_int("max-concurrent", 3).unwrap();
    }

    #[test]
    fn staged_selection_reaches_spawn_take() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("torrent-take");
        let settings = test_settings();
        // Occupy the only slot so nothing spawns a real engine below.
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(99, holder);
        let stem = format!("grab-take-{}", std::process::id());
        let item = manager
            .enqueue_torrent_file(
                single_torrent_bytes(),
                &format!("{stem}.torrent"),
                None,
                Some(vec![0]),
            )
            .unwrap();
        // The exact key spawn_torrent takes with: a staged selection must
        // survive the archive → normalize → store round-trip identically.
        assert_eq!(
            crate::torrent::get_selection(&item.url().to_string()),
            Some(vec![0])
        );
        // Leave no Queued row behind (a later restore could spawn it) and
        // no archive behind; restore the shared memory-backend key.
        manager.cancel_all();
        crate::torrent::delete_archive_for_url(&item.url());
        assert!(crate::torrent::archive_path_for_url(&item.url()).is_none());
        settings.set_int("max-concurrent", 3).unwrap();
    }

    #[test]
    fn delete_download_trashes_torrent_files() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("torrent-del");
        let settings = test_settings();
        // Home-backed dir: GIO refuses to trash across filesystems like /tmp.
        let dir = glib::user_data_dir().join(format!("grab-torrent-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gone.bin");
        std::fs::write(&file, b"bye").unwrap();

        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store.clone(), settings);
        let item = DownloadItem::new(
            7,
            "magnet:?xt=urn:btih:a94a8fe5ccb19ba61c4c0873d391e987982fbbd3&dn=gone",
            "gone.bin",
            &dir.to_string_lossy(),
        );
        item.set_status(DownloadStatus::Done);
        store.append(&item);

        // Explicit delete trashes the real files (not kept silently) and
        // drops the row; the session entry was never created offline.
        assert!(manager.delete_download(7).is_ok());
        assert!(!file.exists());
        assert_eq!(store.n_items(), 0);
        // Undo the test's own Trash litter.
        let trash = glib::user_data_dir().join("Trash");
        let _ = std::fs::remove_file(trash.join("files/gone.bin"));
        let _ = std::fs::remove_file(trash.join("info/gone.bin.trashinfo"));

        // Multi-file torrents trash the whole subfolder.
        let sub = dir.join("Some Torrent");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.mp4"), b"data").unwrap();
        let item2 = DownloadItem::new(
            8,
            "magnet:?xt=urn:btih:b94a8fe5ccb19ba61c4c0873d391e987982fbbd4&dn=Some+Torrent",
            "Some Torrent",
            &dir.to_string_lossy(),
        );
        item2.set_status(DownloadStatus::Done);
        store.append(&item2);
        assert!(manager.delete_download(8).is_ok());
        assert!(!sub.exists());
        assert_eq!(store.n_items(), 0);
        let _ = std::fs::remove_dir_all(trash.join("files/Some Torrent"));
        let _ = std::fs::remove_file(trash.join("info/Some Torrent.trashinfo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_keeps_torrent_archive_for_undo() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("torrent-remove");
        let settings = test_settings();
        // Occupy the only slot so nothing spawns a real engine below.
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(99, holder);
        let stem = format!("grab-undo-{}", std::process::id());
        let item = manager
            .enqueue_torrent_file(
                single_torrent_bytes(),
                &format!("{stem}.torrent"),
                None,
                None,
            )
            .unwrap();
        let id = item.id();
        let url = item.url().to_string();
        // Remove drops the row but keeps the archive, so Undo can re-add
        // the same pseudo-URL and the engine resumes from kept partials.
        manager.remove(id);
        assert_eq!(manager.store().n_items(), 0);
        assert!(
            crate::torrent::archive_path_for_url(&url).is_some_and(|p| p.exists()),
            "remove must keep the archive for Undo"
        );
        // Teardown BEFORE restoring keys (see rejects_relative_download_dir).
        manager.cancel_all();
        crate::torrent::delete_archive_for_url(&url);
        assert!(crate::torrent::archive_path_for_url(&url).is_none());
        settings.set_int("max-concurrent", 3).unwrap();
    }

    #[test]
    fn rejects_relative_download_dir() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("relative-dir");
        let settings = test_settings();
        // Occupy the only slot so nothing spawns a real engine below.
        settings.set_int("max-concurrent", 1).unwrap();
        settings.set_string("download-dir", "relative/dir").unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(99, holder);
        // Configured relative dir falls back to an absolute folder.
        let dir = manager.effective_download_dir();
        assert!(std::path::Path::new(&dir).is_absolute());
        // Explicit relative destinations fall back the same way.
        let item = manager
            .enqueue("https://example.com/f.iso", Some("relative/dir"), None)
            .unwrap();
        assert!(item.dest_dir() == dir);
        // Absolute destinations still pass through untouched.
        let item2 = manager
            .enqueue("https://example.com/g.iso", Some("/tmp"), None)
            .unwrap();
        assert!(item2.dest_dir() == "/tmp");
        // Teardown BEFORE restoring keys: the max-concurrent watch fires
        // start_next, and with no Queued row left it is a no-op. Restoring
        // first would free slots while rows are still queued and spawn real
        // engines whose pump futures outlive this test and abort later tests
        // on glib thread-affinity.
        manager.cancel_all();
        // Restore: the memory backend is shared across tests.
        settings.set_string("download-dir", "").unwrap();
        settings.set_int("max-concurrent", 3).unwrap();
        let _ = std::fs::remove_file(&_qf);
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
    fn shortens_long_filenames() {
        assert_eq!(shorten_filename("short.mp4"), "short.mp4");
        let long = format!("{}.mp4", "a".repeat(300));
        let short = shorten_filename(&long);
        assert!(short.len() <= 240);
        assert!(short.ends_with(".mp4"));
        let wide = format!("{}.mp4", "é".repeat(200));
        let short = shorten_filename(&wide);
        assert!(short.len() <= 240);
        assert!(short.ends_with(".mp4"));
        let no_ext = "b".repeat(300);
        assert!(shorten_filename(&no_ext).len() <= 240);
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
                    selected_files: None,
                },
                StoredItem {
                    url: "https://example.com/b.iso".to_string(),
                    dest_dir: "/tmp/dl".to_string(),
                    filename: "b.iso".to_string(),
                    status: StoredStatus::Done,
                    progress: 1.0,
                    segments: None,
                    selected_files: None,
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
    fn selection_survives_persist_restore() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let qf = test_queue_file("selection-roundtrip");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let m1 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        // Occupy the only slot so nothing spawns a real engine below.
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        m1.running.borrow_mut().insert(99, holder);
        // Archive + stage a selection like the intake dialog does.
        let pseudo =
            crate::torrent::archive_torrent_file("keep.torrent", &single_torrent_bytes()).unwrap();
        crate::torrent::stage_selection(&pseudo, vec![0]);
        let item = m1.enqueue(&pseudo, Some("/tmp/dl"), Some("keep")).unwrap();
        assert_eq!(item.status(), DownloadStatus::Queued);
        m1.persist_queue();
        // The queue file carries the selection...
        let text = std::fs::read_to_string(&qf).unwrap();
        let queue: StoredQueue = serde_json::from_str(&text).unwrap();
        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.items[0].selected_files, Some(vec![0]));
        // ...and restore re-stages it. Prune first to simulate the
        // restart that wipes the in-memory map: without the re-stage,
        // the spawn would take None and download every file.
        crate::torrent::prune_selections(&std::collections::HashSet::new());
        let m2 = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let holder2 = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        m2.running.borrow_mut().insert(99, holder2);
        m2.restore_queue();
        assert_eq!(m2.store().n_items(), 1);
        assert_eq!(crate::torrent::get_selection(&pseudo), Some(vec![0]));
        // Teardown: leave no Queued row behind (a later backend restore
        // would spawn a real engine for it) and restore shared keys.
        m1.cancel_all();
        m2.cancel_all();
        crate::torrent::delete_archive_for_url(&pseudo);
        settings.set_int("max-concurrent", 3).unwrap();
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
        // Control and bidi-override characters deceive in listings and
        // notification text, and arrive via Content-Disposition decoding.
        assert!(!sane_filename("a\nb.mp4"));
        assert!(!sane_filename("a\tb.mp4"));
        assert!(!sane_filename("evil\u{202e}mp4.txt"));
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
                selected_files: None,
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
        let (_lock, _loop) = test_locks();
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
        let (_lock, _loop) = test_locks();
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
    fn raising_max_concurrent_starts_queued() {
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("concurrent-bump");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        let a = DownloadItem::new(63, "https://example.com/a.bin", "a.bin", "/tmp/dl");
        a.set_status(DownloadStatus::Downloading);
        manager.store().append(&a);
        let holder = tokio_rt().spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        manager.running.borrow_mut().insert(63, holder);
        let b = DownloadItem::new(64, "http://127.0.0.1:9/b.bin", "b.bin", "/tmp/dl");
        manager.store().append(&b);
        assert_eq!(b.status(), DownloadStatus::Queued);
        // The preferences SpinRow writes this key; the queued row must start
        // without any other queue event. Port 9 is closed so the spawned
        // engine fails fast during the drain below.
        settings.set_int("max-concurrent", 2).unwrap();
        assert_eq!(b.status(), DownloadStatus::Downloading);
        assert!(manager.running.borrow().contains_key(&64));
        let ctx = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while manager.running.borrow().contains_key(&64) && std::time::Instant::now() < deadline {
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!manager.running.borrow().contains_key(&64));
        // Restore: the memory backend is shared across tests.
        settings.set_int("max-concurrent", 3).unwrap();
    }

    #[test]
    fn defer_yields_slot_and_goes_last() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("defer");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        // Both slots busy with stand-in holders: deferring must not spawn
        // any real engine, keeping this test fully synchronous (no main
        // loop pumping, which would race other tests' glib sources).
        for (id, progress) in [(71, 0.5), (72, 0.0)] {
            let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
            it.set_status(DownloadStatus::Downloading);
            it.set_progress(progress);
            manager.store().append(&it);
            let holder = tokio_rt().spawn(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            });
            manager.running.borrow_mut().insert(id, holder);
        }
        manager.defer(71);
        // Progress kept, row parked behind the other one, slot count kept.
        let a = manager.find(71).unwrap();
        assert_eq!(a.status(), DownloadStatus::Queued);
        assert_eq!(a.progress(), 0.5);
        let order: Vec<u64> = (0..manager.store.n_items())
            .filter_map(|i| manager.store().item(i).and_downcast::<DownloadItem>())
            .map(|it| it.id())
            .collect();
        assert_eq!(order, vec![72, 71]);
        assert!(manager.running.borrow().contains_key(&72));
        assert!(!manager.running.borrow().contains_key(&71));
        // Teardown through the real API: cancelling everything first means
        // no row is queued, so the backend restore below can't start_next a
        // real engine whose UI future would outlive this test. The fake
        // holders are aborted by the cancel itself.
        manager.cancel_all();
        assert!(!manager.running.borrow().contains_key(&72));
        settings.set_int("max-concurrent", 3).unwrap();
    }

    #[test]
    fn lowering_max_concurrent_parks_newest() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("preempt");
        let settings = test_settings();
        settings.set_int("max-concurrent", 2).unwrap();
        let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings.clone());
        for id in [81, 82] {
            let it = DownloadItem::new(id, "https://example.com/f.bin", "f.bin", "/tmp/dl");
            it.set_status(DownloadStatus::Downloading);
            manager.store().append(&it);
            let holder = tokio_rt().spawn(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            });
            manager.running.borrow_mut().insert(id, holder);
        }
        let c = DownloadItem::new(83, "https://example.com/g.bin", "g.bin", "/tmp/dl");
        manager.store().append(&c);
        // The preferences SpinRow writes this key; the newest running row
        // must park itself without any other queue event.
        settings.set_int("max-concurrent", 1).unwrap();
        assert_eq!(
            manager.find(81).unwrap().status(),
            DownloadStatus::Downloading
        );
        assert_eq!(manager.find(82).unwrap().status(), DownloadStatus::Queued);
        assert!(manager.running.borrow().contains_key(&81));
        assert!(!manager.running.borrow().contains_key(&82));
        // Same teardown constraint as the defer test: leave no queued row
        // behind, or the backend restore spawns a real engine for it.
        manager.cancel_all();
        assert!(manager.running.borrow().is_empty());
        settings.set_int("max-concurrent", 3).unwrap();
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
            selected_files: None,
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
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("lifecycle");
        let settings = test_settings();

        let dir = std::env::temp_dir().join(format!("grab-test-{}", std::process::id()));
        let srv = dir.join("srv");
        let dl = dir.join("dl");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(srv.join("t.bin"), &payload).unwrap();

        let port = test_port(0);
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

            cleanup(&server, &dir);
            quit.quit();
        });
        run_loop(&main_loop, 90);
    }

    #[test]
    fn segmented_multi_connection_download() {
        let (_lock, _loop) = test_locks();
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
            cleanup(&server, &dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn adopts_content_disposition_filename() {
        let (_lock, _loop) = test_locks();
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
            cleanup(&server, &dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn prefers_shorter_content_disposition_filename() {
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("disposition-short");
        let settings = test_settings();

        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture(
            "cds",
            "short.zip",
            300_000,
            "0.05",
            &["attachment; filename=\"short.zip\""],
            41,
        );

        settings.set_boolean("show-notifications", false).unwrap();
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        // Long tracked/tokenized URL name with an extension, like file hosts emit.
        let long_name = format!("{}.zip", "a".repeat(120));
        let url = format!("http://127.0.0.1:{port}/{long_name}");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        glib::MainContext::default().spawn_local(async move {
            let item = manager
                .enqueue(&url, Some(&dest), None)
                .unwrap_or_else(|e| abort(&server, &e));
            assert_eq!(item.filename(), long_name);
            let mut waited = 0;
            while (item.filename() != "short.zip" || item.status() != DownloadStatus::Done)
                && waited < 600
            {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.filename() != "short.zip" {
                abort(
                    &server,
                    &format!("expected rename, got {:?}", item.filename()),
                );
            }
            if item.status() != DownloadStatus::Done {
                abort(&server, &format!("expected Done, got {:?}", item.status()));
            }
            let new_path = dl.join("short.zip");
            if std::fs::read(&new_path).unwrap() != payload {
                abort(&server, "bytes differ");
            }
            cleanup(&server, &dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn stale_pump_future_ignores_respawned_row() {
        // Deferring (or a quick pause-resume) aborts the engine and starts a
        // new one: the old pump future must not drag progress backwards,
        // fail the row, or steal the new engine's handle. The hanging server
        // keeps the second engine mid-transfer so any clobbering is visible.
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("stale-pump");
        let settings = test_settings();
        settings.set_int("max-concurrent", 1).unwrap();

        let Fixture {
            dir,
            dl,
            payload: _,
            port,
            server,
        } = spawn_fixture("hang", "h.bin", 20_000, "30", &[], 41);

        settings.set_boolean("show-notifications", false).unwrap();
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        let url = format!("http://127.0.0.1:{port}/h.bin");
        let dest = dl.to_string_lossy().into_owned();
        let item = manager
            .enqueue(&url, Some(&dest), Some("h.bin"))
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        assert_eq!(item.status(), DownloadStatus::Downloading);
        // Let the first engine get going, then defer: park aborts it and a
        // second engine takes over the same row.
        std::thread::sleep(std::time::Duration::from_millis(500));
        manager.defer(id);
        assert_eq!(item.status(), DownloadStatus::Downloading);
        // Let the aborted engine die and the hanging one settle, then pump:
        // the stale future's tail runs here and must stay silent.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let ctx = glib::MainContext::default();
        for _ in 0..20 {
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if item.status() != DownloadStatus::Downloading {
            abort(
                &server,
                &format!(
                    "stale pump future clobbered the respawned row: {:?}",
                    item.detail(),
                ),
            );
        }
        if !manager.running.borrow().contains_key(&id) {
            abort(&server, "respawned engine lost its handle");
        }
        manager.cancel(id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if manager.running.borrow().contains_key(&id) {
            abort(&server, "cancelled engine never exited");
        }
        cleanup(&server, &dir);
        settings.set_int("max-concurrent", 3).unwrap();
    }

    #[test]
    fn piece_rejects_changed_file_version() {
        // No locks: pure tokio + unique fixture, nothing shared.
        let Fixture {
            dir,
            dl,
            payload,
            port,
            server,
        } = spawn_fixture("verc", "v.bin", 300_000, "0", &[], 43);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = FetchCtx {
            client: http_client(),
            url: format!("http://127.0.0.1:{port}/v.bin"),
            dest: dl.join("v.bin"),
            opts: DownloadOptions {
                timeout: 30,
                ..Default::default()
            },
            timeout: Duration::from_secs(30),
            tx,
        };
        let total = payload.len() as u64;
        // Bogus total: the server's Content-Range disagrees, so this must
        // fail Changed at once instead of burning retries.
        match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, 1)) {
            Err(AttemptFail::Changed(_)) => {}
            _ => abort(&server, "wrong-total piece must fail Changed"),
        }
        // Correct total: the piece comes back whole.
        match tokio_rt().block_on(fetch_piece(&ctx, 0, 1023, total)) {
            Ok(body) => assert_eq!(body.len(), 1024),
            _ => abort(&server, "correct-total piece must succeed"),
        }
        cleanup(&server, &dir);
    }

    #[test]
    fn falls_back_to_single_stream_when_throttled() {
        let (_lock, _loop) = test_locks();
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
            cleanup(&server, &dir);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn segmented_pause_resume_keeps_bytes() {
        let (_lock, _loop) = test_locks();
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
            cleanup(&server, &dir);
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
        let mut st = SegmentState::new(4 * PIECE_MIN);
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
        assert_eq!(st.total, 4 * PIECE_MIN);
        assert_eq!(
            st.missing(),
            vec![
                (1, PIECE_MIN, 2 * PIECE_MIN - 1),
                (3, 3 * PIECE_MIN, 4 * PIECE_MIN - 1),
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
        let (_lock, _loop) = test_locks();
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
                    selected_files: None,
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
            cleanup(&server, &dir);
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
        let (_lock, _loop) = test_locks();
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
            cleanup(&server, &dir);
            let _ = std::fs::remove_file(&qf);
            quit.quit();
        });
        run_loop(&main_loop, 120);
    }

    #[test]
    fn aborted_engine_marks_failed() {
        // Simulates an engine task dying without reporting (panic): aborting
        // its handle must fail the row instead of stranding it Downloading.
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("abortwatch");
        let settings = test_settings();

        let dir = std::env::temp_dir().join(format!("grab-abw-{}", std::process::id()));
        let srv = dir.join("srv");
        let dl = dir.join("dl");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(srv.join("t.bin"), &payload).unwrap();

        let port = test_port(37);
        let ranges_log = dir.join("ranges.log");
        let server = std::process::Command::new("python3")
            .arg(format!(
                "{}/tests/throttled_server.py",
                env!("CARGO_MANIFEST_DIR")
            ))
            .arg(port.to_string())
            .arg(srv.join("t.bin"))
            .arg(&ranges_log)
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

        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings);
        let url = format!("http://127.0.0.1:{port}/t.bin");
        let dest = dl.to_string_lossy().into_owned();

        let main_loop = glib::MainLoop::new(None, false);
        let quit = main_loop.clone();
        let server = Rc::new(RefCell::new(server));
        glib::MainContext::default().spawn_local(async move {
            let item = manager
                .enqueue(&url, Some(&dest), Some("t.bin"))
                .unwrap_or_else(|e| abort(&server, &e));
            let id = item.id();
            let mut waited = 0;
            while item.status() != DownloadStatus::Downloading && waited < 200 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Downloading {
                abort(&server, "never started downloading");
            }
            // Kill the engine task without touching row state, as a panic would.
            if let Some(handle) = manager.running.borrow().get(&id) {
                handle.abort();
            }
            let mut waited = 0;
            while item.status() != DownloadStatus::Failed && waited < 100 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Failed {
                abort(
                    &server,
                    &format!("dead engine left row {:?}", item.status()),
                );
            }
            assert_eq!(item.detail(), "Download interrupted");
            cleanup(&server, &dir);
            quit.quit();
        });
        run_loop(&main_loop, 60);
    }

    #[test]
    fn failed_download_reports_cause() {
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("http-error");
        let settings = test_settings();
        let port = test_port(7);
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

    #[test]
    fn restart_with_smaller_file_keeps_partial() {
        // A resume the server answers from zero with a SMALLER object than
        // we hold is a different file (login wall, throttle page): fail
        // loudly and keep the partial bytes instead of truncating them.
        // Plain http.server ignores Range, which is exactly the shape.
        let (_lock, _loop) = test_locks();
        let _qf = test_queue_file("shrink-guard");
        let settings = test_settings();
        let dir = std::env::temp_dir().join(format!("grab-shrink-{}", std::process::id()));
        let srv = dir.join("srv");
        let dl = dir.join("dl");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        std::fs::write(srv.join("t.bin"), vec![7u8; 10 * 1024]).unwrap();
        std::fs::write(dl.join("t.bin"), vec![0u8; 1024 * 1024]).unwrap();

        let port = test_port(53);
        let server = std::process::Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--directory",
                &srv.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 http.server");
        let mut ready = false;
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ready, "test HTTP server did not listen on port {port}");

        let server = Rc::new(RefCell::new(server));
        let store = gio::ListStore::new::<DownloadItem>();
        let manager = DownloadManager::new(store, settings.clone());
        let url = format!("http://127.0.0.1:{port}/t.bin");
        let dest = dl.to_string_lossy().into_owned();
        // restore_existing, not enqueue: enqueue would dedupe away from the
        // pre-written partial, and the restore path is synchronous, so no
        // race with the engine's first metadata read.
        let item = manager
            .restore_existing(&url, &dest, "t.bin", StoredStatus::Downloading, None)
            .unwrap_or_else(|e| abort(&server, &e));
        let id = item.id();
        // Drain the engine's pump future on this thread (see MAIN_LOOP_LOCK).
        let ctx = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while manager.running.borrow().contains_key(&id) && std::time::Instant::now() < deadline {
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if item.status() != DownloadStatus::Failed {
            abort(
                &server,
                &format!("expected loud failure, got {:?}", item.status()),
            );
        }
        if !item.detail().contains("smaller file") {
            abort(&server, &format!("wrong cause: {:?}", item.detail()));
        }
        if std::fs::metadata(dl.join("t.bin"))
            .map(|m| m.len())
            .unwrap_or(0)
            != 1024 * 1024
        {
            abort(&server, "partial file was truncated");
        }
        cleanup(&server, &dir);
    }
}
