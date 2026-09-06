use futures_util::StreamExt as _;
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
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

const QUEUE_VERSION: u32 = 1;

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
    pub fn new(id: u64, url: &str, filename: &str, dest_dir: &str) -> Self {
        glib::Object::builder()
            .property("id", id)
            .property("url", url)
            .property("filename", filename)
            .property("dest-dir", dest_dir)
            .build()
    }

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

pub fn normalize_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
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
}

impl DownloadOptions {
    pub fn from_settings(s: &gio::Settings) -> Self {
        Self {
            tries: s.int("retries"),
            timeout: s.int("timeout"),
            limit_rate: s.string("speed-limit").to_string(),
            user_agent: s.string("user-agent").to_string(),
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
    Progress { downloaded: u64, total: Option<u64> },
    Finished,
    Failed(String),
}

async fn run_download(
    client: &'static reqwest::Client,
    url: String,
    dest: std::path::PathBuf,
    opts: DownloadOptions,
    rate_limit: Option<u64>,
    tx: async_channel::Sender<EngineMsg>,
) {
    let timeout = Duration::from_secs(opts.timeout.max(1) as u64);
    let mut tries = opts.tries.max(1);
    loop {
        match attempt_once(client, &url, &dest, &opts, rate_limit, timeout, &tx).await {
            Ok(()) => {
                tx.send(EngineMsg::Finished).await.ok();
                return;
            }
            Err(e) => {
                tries -= 1;
                if tries <= 0 {
                    tx.send(EngineMsg::Failed(e)).await.ok();
                    return;
                }
            }
        }
    }
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
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && start > 0 {
        return Ok(());
    }
    if status.is_client_error() || status.is_server_error() {
        return Err(format!(
            "HTTP {}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("error")
        ));
    }
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
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
    let mut downloaded = if partial { start } else { 0 };
    tx.send(EngineMsg::Progress { downloaded, total })
        .await
        .ok();
    let mut stream = resp.bytes_stream();
    let pace_start = Instant::now();
    let mut paced: u64 = 0;
    let mut last_sent = Instant::now() - Duration::from_secs(3600);
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
    match total {
        Some(t) if downloaded != t => Err("Incomplete download".to_string()),
        _ => Ok(()),
    }
}

fn parse_rate(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return None;
    }
    let (num, mult) = match s.chars().last()? {
        'K' | 'k' => (s[..s.len() - 1].to_string(), 1024u64),
        'M' | 'm' => (s[..s.len() - 1].to_string(), 1024 * 1024),
        'G' | 'g' => (s[..s.len() - 1].to_string(), 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s.to_string(), 1),
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

pub struct DownloadManager {
    store: gio::ListStore,
    settings: gio::Settings,
    running: RefCell<HashMap<u64, tokio::task::JoinHandle<()>>>,
    next_id: Cell<u64>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
    batch: Cell<bool>,
}

impl DownloadManager {
    pub fn new(store: gio::ListStore, settings: gio::Settings) -> Rc<Self> {
        Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
            batch: Cell::new(false),
        })
    }

    pub fn set_on_change(&self, cb: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(cb));
    }

    pub fn is_batching(&self) -> bool {
        self.batch.get()
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

    pub fn store(&self) -> &gio::ListStore {
        &self.store
    }

    pub fn find(&self, id: u64) -> Option<DownloadItem> {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .find(|it| it.id() == id)
    }

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

    pub fn restore_existing(
        self: &Rc<Self>,
        url: &str,
        dest_dir: &str,
        filename: &str,
        status: StoredStatus,
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
        item.set_detail("Finished".to_string());
        self.insert(item);
    }

    pub fn notifications_enabled(&self) -> bool {
        self.settings.boolean("show-notifications")
    }

    pub fn effective_download_dir(&self) -> String {
        let configured = self.settings.string("download-dir").to_string();
        if !configured.is_empty() {
            return configured;
        }
        glib::user_special_dir(glib::UserDirectory::Downloads)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_string())
    }

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
        let (tx, rx) = async_channel::unbounded();
        let handle = tokio_rt().spawn(run_download(http_client(), url, dest, opts, rate, tx));
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
                    EngineMsg::Finished => {
                        this.running.borrow_mut().remove(&id);
                        if item.status() == DownloadStatus::Cancelled
                            || item.status() == DownloadStatus::Paused
                        {
                            this.changed();
                            this.start_next();
                            break;
                        }
                        item.set_progress(1.0);
                        item.set_status(DownloadStatus::Done);
                        item.set_detail("Finished".to_string());
                        this.notify_finished(&item, true, None);
                        this.persist_queue();
                        this.changed();
                        this.start_next();
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        this.running.borrow_mut().remove(&id);
                        if item.status() == DownloadStatus::Cancelled
                            || item.status() == DownloadStatus::Paused
                        {
                            this.changed();
                            this.start_next();
                            break;
                        }
                        item.set_status(DownloadStatus::Failed);
                        item.set_detail(e.clone());
                        this.notify_finished(&item, false, Some(e));
                        this.persist_queue();
                        this.changed();
                        this.start_next();
                        break;
                    }
                }
            }
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
        self.changed();
    }

    pub fn resume(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id) {
            if item.status() == DownloadStatus::Paused {
                item.set_status(DownloadStatus::Queued);
                self.changed();
                self.start_next();
            }
        }
    }

    pub fn cancel(self: &Rc<Self>, id: u64) {
        if let Some(handle) = self.running.borrow().get(&id) {
            handle.abort();
        }
        self.running.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            item.set_status(DownloadStatus::Cancelled);
            item.set_detail("Cancelled".to_string());
        }
        self.persist_queue();
        self.changed();
        self.start_next();
    }

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

    pub fn delete_download(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let item = self
            .find(id)
            .ok_or_else(|| "Download not found".to_string())?;
        match std::fs::remove_file(item.file_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Could not delete {}: {e}", item.filename())),
        }
        self.remove(id);
        Ok(())
    }

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

    pub fn has_active(&self) -> bool {
        self.any_status(|s| {
            matches!(
                s,
                DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
            )
        })
    }

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
                    items.push(StoredItem {
                        url: it.url().to_string(),
                        dest_dir: it.dest_dir().to_string(),
                        filename: it.filename().to_string(),
                        status,
                        progress: it.progress(),
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
            if queue.version != QUEUE_VERSION {
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
                        if let Err(e) =
                            self.restore_existing(&item.url, &item.dest_dir, &item.filename, status)
                        {
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

    pub fn shutdown(&self) {
        for handle in self.running.borrow().values() {
            handle.abort();
        }
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
    }

    #[test]
    fn delete_download_removes_file_and_row() {
        let _lock = QUEUE_FILE_LOCK.lock().unwrap();
        let _qf = test_queue_file("del");
        let settings = test_settings();
        let dir = std::env::temp_dir().join(format!("grab-del-{}", std::process::id()));
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
                },
                StoredItem {
                    url: "https://example.com/b.iso".to_string(),
                    dest_dir: "/tmp/dl".to_string(),
                    filename: "b.iso".to_string(),
                    status: StoredStatus::Done,
                    progress: 1.0,
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
                    StoredStatus::Queued
                )
                .is_err());
        }
        assert!(manager
            .restore_existing(
                "https://example.com/f.iso",
                "relative/dir",
                "f.iso",
                StoredStatus::Queued
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
        let server_py = format!("{}/tests/throttled_server.py", env!("CARGO_MANIFEST_DIR"));
        let server = std::process::Command::new("python3")
            .arg(&server_py)
            .arg(port.to_string())
            .arg(srv.join("t.bin"))
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

            let item2 = manager.enqueue(&url, Some(&dest), Some("t2.bin")).unwrap();
            manager.cancel(item2.id());
            assert_eq!(item2.status(), DownloadStatus::Cancelled);

            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        let watchdog = main_loop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(90));
            if watchdog.is_running() {
                eprintln!("TEST TIMEOUT");
                std::process::exit(2);
            }
        });
        main_loop.run();
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
            let fail = |msg: &str| -> ! {
                eprintln!("TEST FAILURE: {msg}");
                let _ = server.borrow_mut().kill();
                std::process::exit(1);
            };
            let store = gio::ListStore::new::<DownloadItem>();
            let manager = DownloadManager::new(store, settings);
            let item = manager
                .enqueue(
                    &format!("http://127.0.0.1:{port}/nope.bin"),
                    None,
                    Some("nope.bin"),
                )
                .unwrap_or_else(|e| fail(&e));
            let mut waited = 0;
            while item.status() != DownloadStatus::Failed && waited < 200 {
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                waited += 1;
            }
            if item.status() != DownloadStatus::Failed {
                fail(&format!("expected Failed, got {:?}", item.status()));
            }
            if !item.detail().contains("404") {
                fail(&format!("expected 404 hint, got {:?}", item.detail()));
            }
            let _ = server.borrow_mut().kill();
            quit.quit();
        });
        let watchdog = main_loop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(60));
            if watchdog.is_running() {
                eprintln!("TEST TIMEOUT");
                std::process::exit(2);
            }
        });
        main_loop.run();
    }
}
