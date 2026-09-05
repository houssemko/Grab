//! wget backend: GObject model, progress parser, subprocess manager.
//!
//! GTK rule respected here: all `spawn_future_local` futures run on the main
//! thread, so widget/model updates inside them are safe. `wget` itself runs as
//! a child process; only its stderr *bytes* cross the boundary.

use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Status enum
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "WgetDownloadStatus")]
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

// ---------------------------------------------------------------------------
// DownloadItem GObject (lives in a gio::ListStore)
// ---------------------------------------------------------------------------

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
        /// 0.0..=1.0
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
        const NAME: &'static str = "WgetDownloadItem";
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

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested, no GTK needed)
// ---------------------------------------------------------------------------

/// Parse one `wget --progress=dot:mega` stderr line.
/// Example: `  1500K .......... .......... ....  25% 1.23M 45s`
/// Returns (fraction 0..1, speed, eta).
pub fn parse_progress_line(line: &str) -> Option<(f64, String, String)> {
    // ponytail: one regex-free scan; wget dot format is stable enough.
    let pct_end = line.find('%')?;
    let pct_start = line[..pct_end].rfind(|c: char| !c.is_ascii_digit())? + 1;
    let pct: f64 = line[pct_start..pct_end].trim().parse().ok()?;
    if !(0.0..=100.0).contains(&pct) {
        return None;
    }
    let rest: Vec<&str> = line[pct_end + 1..].split_whitespace().collect();
    if rest.is_empty() {
        return None;
    }
    // Final line joins speed and time: `100% 2.10M=12s`.
    let (speed, eta) = match rest.as_slice() {
        [single] => {
            let mut parts = single.splitn(2, '=');
            match (parts.next(), parts.next()) {
                (Some(s), Some(e)) => (s.trim().to_string(), e.to_string()),
                _ => return None,
            }
        }
        [s, e, ..] => (s.trim_start_matches('=').to_string(), e.to_string()),
        [] => return None,
    };
    if speed.is_empty() || eta.is_empty() {
        return None;
    }
    Some((pct / 100.0, speed, eta))
}

/// Pick the most useful failure cause from recent wget stderr lines.
/// Prefers an explicit ERROR line, else the last line seen.
pub fn failure_hint(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .rev()
        .find(|l| l.contains("ERROR"))
        .or_else(|| lines.last())
        .cloned()
}

/// Guess a filename from a URL's last path segment.
pub fn filename_from_url(url_str: &str) -> String {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.rfind(|s| !s.is_empty()).map(|s| s.to_string()))
        })
        .filter(|s| !s.contains('/') && !s.contains('\0'))
        .unwrap_or_else(|| "index.html".to_string())
}

/// Normalize user input into an absolute URL string, adding a default
/// `https://` scheme for bare `host/path` input like browsers do.
pub fn normalize_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if let Ok(u) = url::Url::parse(trimmed) {
        if matches!(u.scheme(), "http" | "https" | "ftp") {
            return Ok(u.to_string());
        }
        // "localhost:8080/x" parses with scheme "localhost" — a bare host in
        // disguise. Only retry those when the user gave no "://" at all, so
        // real "file:///..." URLs still get validate_url's scheme error.
        if trimmed.contains("://") {
            return Ok(u.to_string());
        }
    }
    if looks_like_bare_host(trimmed) {
        let with_scheme = format!("https://{trimmed}");
        if let Ok(u) = url::Url::parse(&with_scheme) {
            return Ok(u.to_string());
        }
    }
    Err(format!("Invalid URL: {trimmed}"))
}

fn looks_like_bare_host(s: &str) -> bool {
    !s.contains("://") && !s.contains(' ') && (s.contains('.') || s.starts_with("localhost"))
}

/// Only http/https/ftp go to wget (blocks `--post-file`-style flag injection
/// at the UI layer; argv still uses `--` separator defensively).
pub fn validate_url(url_str: &str) -> Result<(), String> {
    let u = url::Url::parse(url_str).map_err(|_| format!("Invalid URL: {url_str}"))?;
    match u.scheme() {
        "http" | "https" | "ftp" => Ok(()),
        s => Err(format!("Unsupported scheme: {s} (use http/https/ftp)")),
    }
}

#[derive(Debug, Clone, Default)]
pub struct WgetOptions {
    pub tries: i32,
    pub timeout: i32,
    pub limit_rate: String,
    pub user: String,
    pub password: String,
    pub user_agent: String,
}

impl WgetOptions {
    pub fn from_settings(s: &gio::Settings) -> Self {
        Self {
            tries: s.int("retries"),
            timeout: s.int("timeout"),
            limit_rate: s.string("speed-limit").to_string(),
            user: String::new(),
            password: String::new(),
            user_agent: s.string("user-agent").to_string(),
        }
    }
}

/// Build a wget argv. argv[0] is the program name for `Subprocess::newv`.
pub fn build_wget_argv(url: &str, dest_file: &std::path::Path, opts: &WgetOptions) -> Vec<String> {
    let mut argv = vec![
        "wget".to_string(),
        "--continue".to_string(),
        "--progress=dot:mega".to_string(),
        format!("--tries={}", opts.tries.max(1)),
        format!("--timeout={}", opts.timeout.max(1)),
    ];
    if !opts.limit_rate.trim().is_empty() && opts.limit_rate.trim() != "0" {
        argv.push(format!("--limit-rate={}", opts.limit_rate.trim()));
    }
    if !opts.user.is_empty() {
        argv.push(format!("--user={}", opts.user));
    }
    if !opts.password.is_empty() {
        argv.push(format!("--password={}", opts.password));
    }
    if !opts.user_agent.trim().is_empty() {
        argv.push(format!("--user-agent={}", opts.user_agent.trim()));
    }
    argv.push(format!("--output-document={}", dest_file.to_string_lossy()));
    argv.push("--".to_string());
    argv.push(url.to_string());
    argv
}

// ---------------------------------------------------------------------------
// Manager: owns the ListStore, enforces max-concurrent, drives wget
// ---------------------------------------------------------------------------

const SIGSTOP: i32 = 19;
const SIGCONT: i32 = 18;

pub struct DownloadManager {
    store: gio::ListStore,
    settings: gio::Settings,
    running: RefCell<HashMap<u64, gio::Subprocess>>,
    next_id: Cell<u64>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
}

impl DownloadManager {
    pub fn new(store: gio::ListStore, settings: gio::Settings) -> Rc<Self> {
        Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
        })
    }

    /// UI refresh hook (status page vs list, header sensitivity).
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
            .filter(|s| !s.is_empty() && !s.contains('/') && !s.contains('\0'))
            .map(|s| s.to_string())
            .unwrap_or_else(|| filename_from_url(&url));
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        self.store.append(&item);
        self.persist_queue();
        self.changed();
        self.start_next();
        Ok(item)
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
        let opts = WgetOptions::from_settings(&self.settings);
        let dest = item.file_path();
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let argv = build_wget_argv(&item.url(), &dest, &opts);
        let argv_refs: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
        let flags = gio::SubprocessFlags::STDERR_PIPE | gio::SubprocessFlags::STDOUT_SILENCE;
        let proc = match gio::Subprocess::newv(&argv_refs, flags) {
            Ok(p) => p,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(format!("Could not start wget: {e}"));
                self.changed();
                return;
            }
        };
        self.running.borrow_mut().insert(item.id(), proc.clone());
        item.set_status(DownloadStatus::Downloading);
        item.set_detail("Starting…".to_string());
        self.changed();

        let this = Rc::clone(self);
        let id = item.id();
        glib::spawn_future_local(async move {
            // Drain stderr line by line (main thread — safe to touch the item).
            // NOTE: read_line_utf8_future yields None ONLY at real EOF. The
            // byte-based read_line* can NOT tell a blank line ("\n" alone,
            // which wget prints after "Saving to:") from EOF — both come back
            // empty. Treating that as EOF closes the pipe early and wget dies
            // writing progress to a broken pipe (EPIPE → exit 1).
            // Recent non-progress lines: on failure the last one usually
            // names the cause ("ERROR 404: Not Found", "Connection refused").
            let mut last_lines: Vec<String> = Vec::new();
            if let Some(stderr) = proc.stderr_pipe() {
                let reader = gio::DataInputStream::new(&stderr);
                loop {
                    match reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
                        Ok(Some(line)) => {
                            if let Some((frac, speed, eta)) = parse_progress_line(&line) {
                                // Skip updates for paused items (SIGSTOP freezes output anyway).
                                if item.status() == DownloadStatus::Downloading {
                                    item.set_progress(frac);
                                    item.set_speed(speed.clone());
                                    item.set_eta(eta.clone());
                                    item.set_detail(format!(
                                        "{}% • {} • ETA {}",
                                        (frac * 100.0) as u64,
                                        speed,
                                        eta
                                    ));
                                }
                            } else {
                                let trimmed = line.trim().to_string();
                                if !trimmed.is_empty() {
                                    if last_lines.len() >= 8 {
                                        last_lines.remove(0);
                                    }
                                    last_lines.push(trimmed);
                                }
                            }
                        }
                        Ok(None) => break, // real EOF: wget closed stderr
                        Err(_) => break,
                    }
                }
            }
            // Wait for exit without blocking: bridge the callback API.
            let (done_tx, done_rx) = async_channel::bounded::<bool>(1);
            proc.wait_check_async(gio::Cancellable::NONE, move |res| {
                let _ = done_tx.send_blocking(res.is_ok());
            });
            let exit_ok = done_rx.recv().await.unwrap_or(false);
            this.running.borrow_mut().remove(&id);

            // Cancelled items were already marked by cancel(); don't overwrite.
            if item.status() == DownloadStatus::Cancelled || item.status() == DownloadStatus::Paused
            {
                this.changed();
                this.start_next();
                return;
            }
            if exit_ok && dest.exists() {
                item.set_progress(1.0);
                item.set_status(DownloadStatus::Done);
                item.set_detail("Finished".to_string());
                this.notify_finished(&item, true, None);
            } else {
                item.set_status(DownloadStatus::Failed);
                let hint = failure_hint(&last_lines);
                match &hint {
                    Some(h) => item.set_detail(h.clone()),
                    None if item.detail().is_empty() || item.detail() == "Starting…" => {
                        item.set_detail("wget exited with an error".to_string())
                    }
                    None => {}
                }
                this.notify_finished(&item, false, hint);
            }
            this.persist_queue();
            this.changed();
            this.start_next();
        });
    }

    fn notify_finished(&self, item: &DownloadItem, ok: bool, hint: Option<String>) {
        if !self.settings.boolean("show-notifications") {
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
        if let Some(proc) = self.running.borrow().get(&id) {
            proc.send_signal(SIGSTOP);
        }
        if let Some(item) = self.find(id) {
            if item.status() == DownloadStatus::Downloading {
                item.set_status(DownloadStatus::Paused);
                item.set_detail(format!("Paused • {}%", (item.progress() * 100.0) as u64));
            }
        }
        self.changed();
    }

    pub fn resume(self: &Rc<Self>, id: u64) {
        let mut need_spawn = false;
        if let Some(proc) = self.running.borrow().get(&id) {
            proc.send_signal(SIGCONT);
        } else if let Some(item) = self.find(id) {
            if item.status() == DownloadStatus::Paused {
                // Process went away (e.g. after restart): re-queue with -c.
                item.set_status(DownloadStatus::Queued);
                need_spawn = true;
            }
        }
        if let Some(item) = self.find(id) {
            if item.status() == DownloadStatus::Paused && !need_spawn {
                item.set_status(DownloadStatus::Downloading);
            }
        }
        self.changed();
        if need_spawn {
            self.start_next();
        }
    }

    pub fn cancel(self: &Rc<Self>, id: u64) {
        if let Some(proc) = self.running.borrow().get(&id) {
            proc.force_exit();
        }
        if let Some(item) = self.find(id) {
            item.set_status(DownloadStatus::Cancelled);
            item.set_detail("Cancelled".to_string());
        }
        // running entry is removed by the spawn future's epilogue; drop it
        // here too in case the future already finished.
        self.running.borrow_mut().remove(&id);
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

    pub fn cancel_all(self: &Rc<Self>) {
        let ids: Vec<u64> = (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            })
            .map(|it| it.id())
            .collect();
        for id in ids {
            self.cancel(id);
        }
    }

    pub fn retry_failed(self: &Rc<Self>) {
        let ids: Vec<u64> = (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Failed | DownloadStatus::Cancelled
                )
            })
            .map(|it| it.id())
            .collect();
        for id in ids {
            self.retry(id);
        }
    }

    pub fn has_active(&self) -> bool {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .any(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Paused
                )
            })
    }

    pub fn has_failed(&self) -> bool {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .any(|it| {
                matches!(
                    it.status(),
                    DownloadStatus::Failed | DownloadStatus::Cancelled
                )
            })
    }

    // -- persistence: pending urls only (finished history is intentionally
    // dropped — add a real history store when someone asks for it).
    fn queue_file() -> std::path::PathBuf {
        let mut dir = glib::user_data_dir();
        dir.push("grab");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("queue.txt")
    }

    fn persist_queue(&self) {
        let mut lines = Vec::new();
        for i in 0..self.store.n_items() {
            if let Some(it) = self.store.item(i).and_downcast::<DownloadItem>() {
                match it.status() {
                    DownloadStatus::Queued
                    | DownloadStatus::Paused
                    | DownloadStatus::Downloading
                    | DownloadStatus::Failed => {
                        lines.push(format!(
                            "{}\t{}\t{}",
                            it.url(),
                            it.dest_dir(),
                            it.filename()
                        ));
                    }
                    _ => {}
                }
            }
        }
        let _ = std::fs::write(Self::queue_file(), lines.join("\n"));
    }

    pub fn restore_queue(self: &Rc<Self>) {
        let Ok(text) = std::fs::read_to_string(Self::queue_file()) else {
            return;
        };
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 2 && !parts[0].is_empty() {
                let filename = parts.get(2).copied().filter(|s| !s.is_empty());
                let _ = self.enqueue(parts[0], Some(parts[1]), filename);
            }
        }
    }

    /// SIGTERM everything on shutdown so no wget outlives the UI.
    pub fn shutdown(&self) {
        for proc in self.running.borrow().values() {
            proc.force_exit();
        }
        self.persist_queue();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dot_mega_lines() {
        let (frac, speed, eta) =
            parse_progress_line("  1500K .......... .......... ..........  25% 1.23M 45s")
                .expect("should parse");
        assert!((frac - 0.25).abs() < 1e-9);
        assert_eq!(speed, "1.23M");
        assert_eq!(eta, "45s");
    }

    #[test]
    fn parses_real_wget125_output() {
        // Captured from `wget --progress=dot:mega` (wget 1.25, localhost).
        let (frac, speed, eta) = parse_progress_line(
            "     0K ........ ........ ........ ........ ........ ........ 37%  279M 0s",
        )
        .expect("should parse");
        assert!((frac - 0.37).abs() < 1e-9);
        assert_eq!(speed, "279M");
        assert_eq!(eta, "0s");
    }

    #[test]
    fn parses_complete_line() {
        let (frac, _, _) =
            parse_progress_line("  9425K .......... .......... ..........  100% 2.10M=12s")
                .expect("should parse");
        assert!((frac - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_noise() {
        assert!(parse_progress_line("Saving to: ‘index.html’").is_none());
        assert!(parse_progress_line("HTTP request sent, awaiting response... 200 OK").is_none());
        assert!(parse_progress_line("").is_none());
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
        assert!(validate_url("ftp://example.com/f").is_ok());
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
        // No more url-crate jargon in user-facing errors.
        assert_eq!(
            normalize_url("example .com").unwrap_err(),
            "Invalid URL: example .com"
        );
    }

    #[test]
    fn failure_hints() {
        let lines = vec![
            "--2026-09-05--  http://x/f.iso".to_string(),
            "Connecting to x... connected.".to_string(),
            "HTTP request sent, awaiting response... 404 File not found".to_string(),
            "2026-09-05 ERROR 404: File not found.".to_string(),
        ];
        assert_eq!(
            failure_hint(&lines).as_deref(),
            Some("2026-09-05 ERROR 404: File not found.")
        );
        let lines = vec!["Connecting to x... failed: Connection refused.".to_string()];
        assert_eq!(
            failure_hint(&lines).as_deref(),
            Some("Connecting to x... failed: Connection refused.")
        );
        assert_eq!(failure_hint(&[]), None);
    }

    #[test]
    fn argv_shape() {
        let opts = WgetOptions {
            tries: 3,
            timeout: 30,
            ..Default::default()
        };
        let argv = build_wget_argv(
            "https://example.com/f.iso",
            std::path::Path::new("/tmp/dl/f.iso"),
            &opts,
        );
        assert_eq!(argv[0], "wget");
        assert!(argv.contains(&"--continue".to_string()));
        assert!(argv.contains(&"--progress=dot:mega".to_string()));
        // URL is last, after `--` separator.
        assert_eq!(argv[argv.len() - 1], "https://example.com/f.iso");
        assert_eq!(argv[argv.len() - 2], "--");
    }

    /// Full lifecycle against a local HTTP server: download → pause (bytes
    /// freeze) → resume → Done (bytes match) → cancel. Headless: no widgets,
    /// just the ListStore + MainLoop.
    #[test]
    fn manager_pause_resume_cancel() {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", env!("GRAB_SCHEMA_DIR"));
        // Memory backend: the test never touches the user's real dconf db,
        // even if it times out or panics mid-run.
        std::env::set_var("GSETTINGS_BACKEND", "memory");

        let dir = std::env::temp_dir().join(format!("wgetmgr-test-{}", std::process::id()));
        let srv = dir.join("srv");
        let dl = dir.join("dl");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(srv.join("t.bin"), &payload).unwrap();

        // PID-unique port: never collide with orphaned servers from earlier runs.
        let port = 20000 + (std::process::id() % 5000) as u16;
        let server_log = dir.join("server.log");
        let server_log_file = std::fs::File::create(&server_log).unwrap();
        // Throttled single-file server (~160KB/s) so pause() always lands
        // mid-transfer. No http.server quirks, no Range/keep-alive surprises.
        let server_py = format!("{}/tests/throttled_server.py", env!("CARGO_MANIFEST_DIR"));
        let server = std::process::Command::new("python3")
            .arg(&server_py)
            .arg(port.to_string())
            .arg(srv.join("t.bin"))
            .stdout(std::process::Stdio::null())
            .stderr(server_log_file)
            .spawn()
            .expect("python3 throttled server");
        // Wait for accept (connect+close is enough; no GET, no body served).
        let mut ready = false;
        for _ in 0..100 {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ready, "test HTTP server did not listen on port {port}");

        let settings = gio::Settings::new("io.github.houssemko.Grab");
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

            // Let it run, then pause and check bytes freeze.
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

            // Resume to completion.
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

            // Cancel path.
            let item2 = manager.enqueue(&url, Some(&dest), Some("t2.bin")).unwrap();
            manager.cancel(item2.id());
            assert_eq!(item2.status(), DownloadStatus::Cancelled);

            // Cleanup.
            let _ = server.borrow_mut().kill();
            let _ = std::fs::remove_dir_all(&dir);
            quit.quit();
        });
        // Watchdog: never hang CI.
        let watchdog = main_loop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(90));
            if watchdog.is_running() {
                eprintln!("TEST TIMEOUT");
                std::process::exit(2);
            }
        });
        main_loop.run();
        // (Server is killed by the driver on all in-loop paths; the watchdog
        // path may orphan it, but its port is PID-unique.)
    }
}
