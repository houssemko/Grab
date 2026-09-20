use futures_util::StreamExt as _;
use gettextrs::{gettext, ngettext};
use gtk4::gio::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum, serde::Serialize, serde::Deserialize,
)]
#[enum_type(name = "GrabDownloadStatus")]
#[serde(rename_all = "lowercase")]
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
    pub fn label(self) -> String {
        // gettext() wraps each literal here (not at the call sites) so
        // xgettext can statically extract every status msgid.
        match self {
            DownloadStatus::Queued => gettext("Queued"),
            DownloadStatus::Downloading => gettext("Downloading"),
            DownloadStatus::Paused => gettext("Paused"),
            DownloadStatus::Done => gettext("Done"),
            DownloadStatus::Failed => gettext("Failed"),
            DownloadStatus::Cancelled => gettext("Cancelled"),
        }
    }
}

const QUEUE_VERSION: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredItem {
    url: String,
    dest_dir: String,
    filename: String,
    status: DownloadStatus,
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
    /// Recorded engine output folder for torrents (v2+). Absent on old
    /// files and for items that need no folder tracking.
    #[serde(default)]
    output_dir: Option<String>,
    /// Video-page source for yt-dlp items: only `Some(Page)` is ever
    /// written (plain downloads omit it, so old files stay clean and old
    /// app versions keep reading new ones).
    #[serde(default)]
    video_source: Option<crate::video::VideoSource>,
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
        /// Engine's real output folder for torrents (magnets land in
        /// `dest/<stub>/`; empty means unknown, use `file_path()`).
        /// Recorded at enqueue, persisted in the queue, trashed on delete.
        #[property(get, set)]
        pub output_dir: RefCell<String>,
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

    /// Best on-disk guess for reveal: inside the recorded engine folder
    /// when one exists, else the plain destination path.
    pub fn display_path(&self) -> std::path::PathBuf {
        let output_dir = self.output_dir();
        if output_dir.is_empty() {
            self.file_path()
        } else {
            std::path::Path::new(&output_dir).join(self.filename())
        }
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

/// File stem of a finished-name candidate (`Clip.mp4` → `Clip`,
/// extensionless `README` → `README`); `""` when there is none, which
/// never reserves (see `stem_reserved_in`).
fn name_stem(name: &str) -> &str {
    std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
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

/// Header-safe User-Agent from the free-text setting: keep visible ASCII
/// plus spaces, drop the rest. An invalid byte would make reqwest fail the
/// request (or worse, per version), and the value is dconf-writable — so
/// sanitize at the single choke point instead of trusting all call sites.
pub(crate) fn sanitize_user_agent(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect::<String>()
        .trim()
        .to_string()
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
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = ((h as char).to_digit(16), (l as char).to_digit(16))
        {
            decoded = Some((h << 4 | l) as u8);
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
            return Err(gettext("Magnet link is too long (max {n} characters)")
                .replace("{n}", &MAX_MAGNET_LEN.to_string()));
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
            .ok_or_else(|| gettext("Torrent file is missing from the archive"));
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(gettext("URL is too long (max {n} characters)")
            .replace("{n}", &MAX_URL_LEN.to_string()));
    }
    // An explicit scheme is authoritative: only http(s) passes, so the
    // separate validate pass is unnecessary. (Bare `host:port` inputs must
    // not take this branch: `localhost:8080/f` parses with scheme
    // "localhost" and still needs `https://` prepended below.)
    if trimmed.contains("://") {
        let u = url::Url::parse(trimmed)
            .map_err(|_| gettext("Invalid URL: {url}").replace("{url}", trimmed))?;
        if !u.username().is_empty() || u.password().is_some() {
            return Err(gettext("URLs with a username/password are not supported"));
        }
        return match u.scheme() {
            "http" | "https" => Ok(u.to_string()),
            "ftp" => Err(gettext("FTP is not supported (use http/https)")),
            s => Err(gettext("Unsupported scheme: {s} (use http/https)").replace("{s}", s)),
        };
    }
    let bare =
        !trimmed.contains(' ') && (trimmed.contains('.') || trimmed.starts_with("localhost"));
    if bare {
        let with_scheme = format!("https://{trimmed}");
        if let Ok(u) = url::Url::parse(&with_scheme)
            && u.username().is_empty()
            && u.password().is_none()
        {
            return Ok(u.to_string());
        }
    }
    Err(gettext("Invalid URL: {url}").replace("{url}", trimmed))
}

#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    pub tries: i32,
    pub timeout: i32,
    pub limit_rate: String,
    pub user_agent: String,
    /// Parallel range connections for large downloads (1 = single stream).
    pub connections: i32,
    pub proxy_mode: String,
    pub proxy_type: String,
    pub proxy_host: String,
    pub proxy_port: i32,
    /// Raw browser-auth setting (`none` when off). The direct engine
    /// exports it to a cookie jar once per attempt (see cookies.rs).
    pub cookies_browser: String,
}

/// Proxy modes for the `proxy-mode` setting.
pub const PROXY_MODE_SYSTEM: &str = "system";
pub const PROXY_MODE_MANUAL: &str = "manual";
pub const PROXY_MODE_DIRECT: &str = "direct";

pub const PROXY_MODE_VALUES: &[&str] = &[PROXY_MODE_SYSTEM, PROXY_MODE_MANUAL, PROXY_MODE_DIRECT];

/// Translated combo labels, index-aligned with [`PROXY_MODE_VALUES`].
pub fn proxy_mode_labels() -> Vec<String> {
    vec![gettext("System"), gettext("Manual"), gettext("Off")]
}

/// Combo index for a stored mode value. Unknown values fall back to
/// system (the default).
pub fn proxy_mode_index(value: &str) -> usize {
    PROXY_MODE_VALUES
        .iter()
        .position(|v| *v == value)
        .unwrap_or(0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to system.
pub fn proxy_mode_value(index: usize) -> &'static str {
    PROXY_MODE_VALUES
        .get(index)
        .copied()
        .unwrap_or(PROXY_MODE_SYSTEM)
}

pub const PROXY_TYPE_VALUES: &[&str] = &["http", "https", "socks5"];

/// Protocol names are left untranslated, like codec labels.
pub fn proxy_type_labels() -> Vec<String> {
    vec![
        "HTTP".to_string(),
        "HTTPS".to_string(),
        "SOCKS5".to_string(),
    ]
}

/// Combo index for a stored type value. Unknown values fall back to SOCKS5.
pub fn proxy_type_index(value: &str) -> usize {
    PROXY_TYPE_VALUES
        .iter()
        .position(|v| *v == value)
        .unwrap_or(2)
}

/// Stored value for a combo index. Out-of-range indexes fall back to SOCKS5.
pub fn proxy_type_value(index: usize) -> &'static str {
    PROXY_TYPE_VALUES.get(index).copied().unwrap_or("socks5")
}

/// Proxy resolved for one attempt: reqwest interceptors for the direct
/// engine, plus the CLI form for yt-dlp spawns.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedProxy {
    proxies: Vec<reqwest::Proxy>,
    /// Single URL for `--proxy` (SOCKS always remote-resolving).
    pub cli_url: String,
    /// Comma list for NO_PROXY env on yt-dlp spawns.
    pub no_proxy_env: String,
    cache_key: String,
}

impl ResolvedProxy {
    /// SOCKS5 URL for the torrent engine: librqbit's `proxy_url` demands
    /// exactly the `socks5://` scheme, so the remote-resolving `socks5h://`
    /// form normalizes down. Peer addresses arrive as IPs (trackers, PEX —
    /// DHT is off under proxy), so no hostname resolution happens on the
    /// peer path at all. HTTP(S) proxies yield `None`: the engine has no
    /// HTTP-CONNECT peer path, so those torrents stay direct instead of
    /// failing.
    pub fn torrent_socks_url(&self) -> Option<String> {
        let rest = self
            .cli_url
            .strip_prefix("socks5h://")
            .or_else(|| self.cli_url.strip_prefix("socks5://"))?;
        Some(format!("socks5://{rest}"))
    }
}

/// Loopback bypass applied when no ignore list is configured: exits
/// cannot reach the user's own machine, so proxying localhost only
/// breaks local services.
const LOOPBACK_BYPASS: &str = "localhost,127.0.0.1,::1";

/// Host match for one ignore entry: exact or subdomain suffix, with a
/// leading `*.`/`.` tolerated. Ports and CIDR ranges are out of scope.
fn ignore_entry_normalized(pattern: &str) -> Option<String> {
    let p = pattern.trim().trim_end_matches('.').to_lowercase();
    let p = p.strip_prefix("*.").unwrap_or(&p);
    let p = p.strip_prefix('.').unwrap_or(p);
    if p.is_empty() {
        return None;
    }
    Some(p.to_string())
}

/// Normalize a GNOME ignore-hosts list into plain domains for
/// reqwest's NoProxy (suffix matching): `*.local` and `.local` both
/// become `local`. Unparseable entries are dropped, never passed on.
pub(crate) fn normalize_no_proxy(patterns: &[String]) -> String {
    patterns
        .iter()
        .filter_map(|p| ignore_entry_normalized(p))
        .collect::<Vec<_>>()
        .join(",")
}

fn system_proxy_settings() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    // Settings::new panics on a missing schema (non-GNOME systems, bare
    // CI images): probe first so system mode degrades to direct there.
    source.lookup("org.gnome.system.proxy", true)?;
    Some(gio::Settings::new("org.gnome.system.proxy"))
}

/// Proxy from the desktop settings (`org.gnome.system.proxy`, manual
/// mode). PAC (`auto`) is unsupported by design — executing remote
/// proxy scripts is out of scope — as is a missing schema.
fn system_proxy() -> Option<ResolvedProxy> {
    use gtk4::gio::prelude::SettingsExt as _;
    let s = system_proxy_settings()?;
    if s.string("mode").as_str() != "manual" {
        return None;
    }
    let ignore: Vec<String> = s
        .strv("ignore-hosts")
        .iter()
        .map(|v| v.to_string())
        .collect();
    let bypassed = normalize_no_proxy(&ignore);
    let no_proxy_env = if bypassed.is_empty() {
        LOOPBACK_BYPASS.to_string()
    } else {
        bypassed
    };
    let no_proxy = reqwest::NoProxy::from_string(&no_proxy_env);
    let host = |key: &str| s.string(key).trim().to_string();
    let port = |key: &str| s.int(key);
    // Same host-safe rule as manual_proxy: dconf free text feeds URL
    // construction, so anything outside host chars fails this leg (the
    // lenient-system rule degrades to direct, never to a mangled URL).
    let valid = |h: &str, p: i32| {
        !h.is_empty()
            && (1..=65535).contains(&p)
            && h.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']')
            })
    };
    // SOCKS first: one remote-resolving tunnel covers every scheme,
    // which is also the Tor shape (system SOCKS host + port).
    let socks = host("socks-host");
    if valid(&socks, port("socks-port")) {
        let url = format!("socks5h://{}:{}", socks, port("socks-port"));
        let proxy = reqwest::Proxy::all(url.clone()).ok()?.no_proxy(no_proxy);
        return Some(ResolvedProxy {
            proxies: vec![proxy],
            cache_key: format!("{url}|{no_proxy_env}"),
            cli_url: url,
            no_proxy_env,
        });
    }
    let http = host("http-host");
    let https = host("https-host");
    if s.boolean("use-same-proxy") && valid(&http, port("http-port")) {
        let url = format!("http://{}:{}", http, port("http-port"));
        let mk = |p: reqwest::Proxy| p.no_proxy(reqwest::NoProxy::from_string(&no_proxy_env));
        let proxies = [
            reqwest::Proxy::http(url.clone()),
            reqwest::Proxy::https(url.clone()),
        ]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .into_iter()
        .map(mk)
        .collect();
        return Some(ResolvedProxy {
            proxies,
            cache_key: format!("{url}|{no_proxy_env}"),
            cli_url: url,
            no_proxy_env,
        });
    }
    // Split HTTP/HTTPS proxies: cover each scheme present. CLI gets the
    // secure leg (the sensitive half); plain-HTTP direct fallback there
    // is documented on the mode row.
    let mut proxies = Vec::new();
    if valid(&http, port("http-port")) {
        proxies.push(
            reqwest::Proxy::http(format!("http://{}:{}", http, port("http-port")))
                .ok()?
                .no_proxy(reqwest::NoProxy::from_string(&no_proxy_env)),
        );
    }
    if valid(&https, port("https-port")) {
        proxies.push(
            reqwest::Proxy::https(format!("http://{}:{}", https, port("https-port")))
                .ok()?
                .no_proxy(reqwest::NoProxy::from_string(&no_proxy_env)),
        );
    }
    if proxies.is_empty() {
        return None;
    }
    let cli_url = if valid(&https, port("https-port")) {
        format!("http://{}:{}", https, port("https-port"))
    } else {
        format!("http://{}:{}", http, port("http-port"))
    };
    Some(ResolvedProxy {
        proxies,
        cache_key: format!("{cli_url}|{no_proxy_env}"),
        cli_url,
        no_proxy_env,
    })
}

/// Proxy from the manual settings. Unlike system resolution this is an
/// explicit user demand: invalid values fail loudly instead of
/// silently leaking direct, because a typo must never look like
/// privacy.
fn manual_proxy(o: &DownloadOptions) -> Result<Option<ResolvedProxy>, String> {
    let host = o.proxy_host.trim();
    if host.is_empty() {
        return Err(gettext("Proxy host is empty"));
    }
    if !(1..=65535).contains(&o.proxy_port) {
        return Err(gettext("Proxy port is out of range (1–65535)"));
    }
    // Free-text host feeds URL construction below (`http://{host}:{port}`):
    // anything outside host-safe chars (`@`, `/`, `?`, `#`, whitespace…)
    // would smuggle userinfo, paths or log splits into the proxy URL.
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(gettext("Proxy host contains invalid characters"));
    }
    let no_proxy_env = LOOPBACK_BYPASS.to_string();
    let no_proxy = reqwest::NoProxy::from_string(&no_proxy_env);
    let (proxies, cli_url) = match o.proxy_type.as_str() {
        "http" | "https" => {
            let url = format!("http://{host}:{}", o.proxy_port);
            let mk = |p: reqwest::Proxy| p.no_proxy(reqwest::NoProxy::from_string(&no_proxy_env));
            let proxies = [
                reqwest::Proxy::http(url.clone()),
                reqwest::Proxy::https(url.clone()),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(mk)
            .collect();
            (proxies, url)
        }
        // SOCKS5 always remote-resolving: local DNS would leak every
        // hostname around the tunnel.
        "socks5" => {
            let url = format!("socks5h://{host}:{}", o.proxy_port);
            let proxy = reqwest::Proxy::all(url.clone())
                .map_err(|e| e.to_string())?
                .no_proxy(no_proxy);
            (vec![proxy], url)
        }
        other => {
            return Err(gettext("Unknown proxy type: {t}").replace("{t}", other));
        }
    };
    Ok(Some(ResolvedProxy {
        proxies,
        cache_key: format!("{cli_url}|{no_proxy_env}"),
        cli_url,
        no_proxy_env,
    }))
}

impl DownloadOptions {
    /// Snapshot the network-related GSettings keys.
    pub fn from_settings(s: &crate::settings::AppSettings) -> Self {
        Self {
            tries: s.retries(),
            timeout: s.timeout(),
            limit_rate: s.speed_limit(),
            user_agent: sanitize_user_agent(&s.user_agent()),
            connections: s.connections(),
            proxy_mode: s.proxy_mode(),
            proxy_type: s.proxy_type(),
            proxy_host: s.proxy_host(),
            proxy_port: s.proxy_port(),
            cookies_browser: s.cookies_browser(),
        }
    }

    /// Proxy for this attempt. Manual misconfiguration fails loudly;
    /// system resolution degrades to direct (missing schema, PAC mode,
    /// empty hosts) since it is opportunistic, not demanded.
    pub fn proxy_config(&self) -> Result<Option<ResolvedProxy>, String> {
        match self.proxy_mode.as_str() {
            PROXY_MODE_DIRECT => Ok(None),
            PROXY_MODE_MANUAL => manual_proxy(self),
            // System default and unknown values resolve opportunistically.
            _ => Ok(system_proxy()),
        }
    }
}

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

/// Lock a worker-shared mutex, recovering the guarded value when a
/// previous worker panic poisoned it. The item then fails with an error
/// instead of the panic cascading through every worker into the app.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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
    CLIENT.get_or_init(|| client_builder().build().expect("http client"))
}

/// reqwest client honoring this attempt's proxy, sharing one pooled
/// client per proxy config. Direct attempts keep the static client, so
/// enabling a proxy never perturbs existing connection pools.
pub(crate) fn http_client_for(proxy: Option<&ResolvedProxy>) -> reqwest::Client {
    let Some(proxy) = proxy else {
        return http_client().clone();
    };
    static PROXIED: OnceLock<Mutex<std::collections::HashMap<String, reqwest::Client>>> =
        OnceLock::new();
    let cache = PROXIED.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(client) = lock_recover(cache).get(&proxy.cache_key) {
        return client.clone();
    }
    let mut builder = client_builder();
    for p in &proxy.proxies {
        builder = builder.proxy(p.clone());
    }
    let client = builder.build().expect("proxied http client");
    lock_recover(cache).insert(proxy.cache_key.clone(), client.clone());
    client
}

/// Shared builder: bounded hops, no downgrades (see [`http_client`]).
fn client_builder() -> reqwest::ClientBuilder {
    // Bounded hops: a malicious server must not bounce the client
    // around without limit. Downgrades are refused outright: no
    // cookie or auth store is enabled, but a https→http bounce would
    // still let a network attacker substitute the downloaded bytes.
    // Only the URL + UA cross origins.
    //
    // No ambient proxy either: reqwest would otherwise route through
    // HTTP_PROXY-style env vars behind Direct mode's back. Proxying
    // is always explicit here (settings or nothing).
    // Automatic Referer is off for the same reason: cross-origin
    // redirects must not leak full URLs (tokens included) as Referer.
    reqwest::Client::builder()
        .no_proxy()
        .referer(false)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let downgrade = attempt
                .previous()
                .last()
                .is_some_and(|u| u.scheme() == "https")
                && attempt.url().scheme() == "http";
            if attempt.previous().len() > 5 || downgrade {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
}

pub(crate) enum EngineMsg {
    Progress {
        downloaded: u64,
        total: Option<u64>,
        /// Torrent upload counters (HTTP sends zeros): shown on the row
        /// while downloading and while seeding toward a seed limit.
        uploaded: u64,
        upload_bps: u64,
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
    /// Server-advertised Last-Modified (HTTP date). Stored for application
    /// at Finished when the keep-server-date setting is on. Best-effort:
    /// a missing or unparsable header simply sends nothing.
    LastModified(SystemTime),
    /// The server object changed mid-download (version check failed). The
    /// UI thread drops the resume bitmap so a later retry starts fresh
    /// instead of failing on the dead file version forever.
    FailedVersion(String),
    /// Torrent per-piece haves polled from the session (500ms tick).
    /// Replaces the stored bitfield; the block map redraws off progress
    /// ticks arriving on the same tick, so this needs no extra signal.
    TorrentPieces(Vec<bool>),
    /// Free-form phase label from engines whose progress has stages the
    /// byte counters don't capture (video resolve/merge). Applied as the
    /// row detail; the next Progress tick renders over it as usual.
    Phase(String),
}

/// Shared inputs for one download's engine task. Groups the params every
/// attempt function needs instead of threading eight loose arguments.
struct FetchCtx {
    client: reqwest::Client,
    url: String,
    dest: std::path::PathBuf,
    opts: DownloadOptions,
    /// Attempt-scoped browser cookie jar (`None` = setting off or
    /// export unavailable). Resolved once per attempt, not per request.
    cookies: Option<std::sync::Arc<reqwest::cookie::Jar>>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
}

async fn run_download(mut ctx: FetchCtx, connections: usize, mode: StartMode) {
    let timeout = Duration::from_secs(ctx.opts.timeout.max(1) as u64);
    let mut tries = ctx.opts.tries.max(1);
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
            match probe_ranges(
                &ctx.client,
                &ctx.url,
                &ctx.opts,
                ctx.cookies.as_ref(),
                timeout,
            )
            .await
            {
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
            }
        }
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
            ctx.tx
                .send(EngineMsg::Progress {
                    downloaded,
                    total: Some(total),
                    uploaded: 0,
                    upload_bps: 0,
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
                // ~20fps row updates: smooth determinate motion per HIG;
                // each tick is one label render on a handful of rows.
                if last_sent.elapsed() >= Duration::from_millis(50) {
                    rate = live_rate_limit();
                    ctx.tx
                        .send(EngineMsg::Progress {
                            downloaded,
                            total: Some(total),
                            uploaded: 0,
                            upload_bps: 0,
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
                    uploaded: 0,
                    upload_bps: 0,
                })
                .ok();
            Ok::<u64, String>(written)
        }
    };
    let (wres, _) = tokio::join!(writer, futures_util::future::join_all(workers));
    let written = wres.map_err(AttemptFail::Retryable)?;
    if changed.load(Ordering::SeqCst) {
        let msg = lock_recover(&first_err)
            .take()
            .unwrap_or_else(|| gettext("Download interrupted"));
        return Err(AttemptFail::Changed(msg));
    }
    if throttled.load(Ordering::SeqCst) {
        let msg = lock_recover(&first_err)
            .take()
            .unwrap_or_else(|| gettext("Download interrupted"));
        return Err(AttemptFail::Throttled(msg));
    }
    if failed.load(Ordering::SeqCst) {
        let msg = lock_recover(&first_err)
            .take()
            .unwrap_or_else(|| gettext("Download interrupted"));
        return Err(AttemptFail::Retryable(msg));
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
pub(crate) const DEST_EXISTS: &str = "Destination already exists";

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
            &ctx.opts.user_agent,
            ctx.cookies.as_ref(),
            &ctx.url,
        );
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
        }
        let resp = match tokio::time::timeout(
            ctx.timeout,
            ctx.client.execute(req.build().map_err(|e| e.to_string())?),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err(gettext("Connection timed out")),
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
                let hreq = stamp_request(
                    ctx.client.head(&ctx.url),
                    &ctx.opts.user_agent,
                    ctx.cookies.as_ref(),
                    &ctx.url,
                );
                if let Ok(built) = hreq.build()
                    && let Ok(Ok(hresp)) =
                        tokio::time::timeout(ctx.timeout, ctx.client.execute(built)).await
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
        ctx.tx
            .send(EngineMsg::Progress {
                downloaded,
                total,
                uploaded: 0,
                upload_bps: 0,
            })
            .ok();
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
            if let Some(r) = rate {
                paced += chunk.len() as u64;
                let wait = paced as f64 / r as f64 - pace_start.elapsed().as_secs_f64();
                if wait > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
            }
            // ~20fps row updates: smooth determinate motion per HIG;
            // each tick is one label render on a handful of rows.
            if last_sent.elapsed() >= Duration::from_millis(50) {
                rate = live_rate_limit();
                ctx.tx
                    .send(EngineMsg::Progress {
                        downloaded,
                        total,
                        uploaded: 0,
                        upload_bps: 0,
                    })
                    .ok();
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

fn publish_rate_limit(settings: &crate::settings::AppSettings) {
    LIVE_RATE_LIMIT.store(
        parse_rate(settings.speed_limit().as_str()).unwrap_or(0),
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

/// Human ETA ("14 minutes"): longest whole unit, for the HIG's
/// "About {eta} left" estimate phrasing. Pure for tests.
fn fmt_eta(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        ngettext("1 hour", "{n} hours", h as u32).replace("{n}", &h.to_string())
    } else if m > 0 {
        ngettext("1 minute", "{n} minutes", m as u32).replace("{n}", &m.to_string())
    } else {
        ngettext("1 second", "{n} seconds", s as u32).replace("{n}", &s.to_string())
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
pub(crate) fn piece_len(total: u64) -> u64 {
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
/// Block-map cells the piece bitmap downsamples to for display. Native
/// piece counts vary (up to 4096); the widget aggregates to this width so
/// every row renders the same compact strip.
pub(crate) const BLOCK_CELLS: usize = 256;

/// How many connections a download may use: at least 2 to bother splitting,
/// at most 16, and never more than one per MIN_SEGMENT of file.
fn split_count(total: u64, connections: usize) -> usize {
    (connections.max(1) as u64).min(total / MIN_SEGMENT).min(16) as usize
}

/// Downsample a piece bitmap to `n` display cells: cell `i` covers
/// `bits[i*len/n..(i+1)*len/n)` and reads done when at least half its
/// pieces are. Empty in, empty out.
pub(crate) fn aggregate(bits: &[bool], n: usize) -> Vec<bool> {
    if bits.is_empty() || n == 0 {
        return Vec::new();
    }
    (0..n)
        .map(|i| {
            let (lo, hi) = (i * bits.len() / n, (i + 1) * bits.len() / n);
            let span = hi.saturating_sub(lo).max(1);
            let done = bits[lo..hi.max(lo + 1)].iter().filter(|b| **b).count();
            2 * done >= span
        })
        .collect()
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

/// Rename without clobbering: `std::fs::rename` silently replaces the
/// destination. Prefers `renameat2(RENAME_NOREPLACE)` (atomic on any
/// filesystem, FAT included); falls back to claiming `new` with a hard
/// link, to a plain rename only where hard links are unsupported, and to
/// a copy where source and destination live on different filesystems
/// (staging is on tmpfs, downloads usually are not).
pub(crate) fn rename_noreplace(
    old: &std::path::Path,
    new: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    match rename_noreplace_sys(old, new) {
        // Ancient kernels (< 3.15) lack renameat2: use the portable path.
        // ENOSYS is 38 in the Linux UAPI (asm-generic and x86 alike).
        Err(e) if e.raw_os_error() == Some(38) => {}
        // Cross-device: no rename variant can span filesystems (EXDEV
        // 18); copy through a `create_new` claim instead.
        Err(e) if e.raw_os_error() == Some(18) => return copy_noreplace(old, new),
        r => return r,
    }
    // Claim `new` atomically via the link: an `exists()` pre-check followed
    // by a plain rename is a TOCTOU — a rival rename can slip in between
    // and get clobbered. Retry the link on transient errors; fall back to
    // plain rename only where hard links cannot work at all (Linux UAPI
    // numbers: EPERM 1, EOPNOTSUPP 95, ENOSYS 38), and to a copy across
    // filesystems (EXDEV 18).
    loop {
        match std::fs::hard_link(old, new) {
            Ok(()) => return std::fs::remove_file(old),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(18) => {
                return copy_noreplace(old, new);
            }
            Err(e) if matches!(e.raw_os_error(), Some(1 | 95 | 38)) => {
                return std::fs::rename(old, new);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copy `old` to `new` without replacing an existing `new`, then remove
/// `old`. Cross-device fallback for [`rename_noreplace`]: neither rename
/// nor link can span filesystems, so bytes are copied through a
/// `create_new` handle — the atomic claim, no TOCTOU — and the source is
/// unlinked only after the copy lands. A failed copy removes the partial
/// destination, which only this call could have created.
fn copy_noreplace(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(old)?;
    let permissions = src.metadata().map(|m| m.permissions()).ok();
    let mut dst = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(new)?;
    let copy = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if copy.is_err() {
        let _ = std::fs::remove_file(new);
        return copy.map(|_| ());
    }
    drop(dst);
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(new, permissions);
    }
    std::fs::remove_file(old)
}

/// `renameat2(olddirfd, old, newdirfd, new, RENAME_NOREPLACE)` without a
/// libc dependency: one syscall, three stable constants.
#[cfg(target_os = "linux")]
fn rename_noreplace_sys(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    unsafe extern "C" {
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

async fn probe_ranges(
    client: &reqwest::Client,
    url: &str,
    opts: &DownloadOptions,
    cookies: Option<&std::sync::Arc<reqwest::cookie::Jar>>,
    timeout: Duration,
) -> Result<u64, String> {
    let req = stamp_request(
        client.get(url).header("Range", "bytes=0-0"),
        &opts.user_agent,
        cookies,
        url,
    );
    let resp = match tokio::time::timeout(
        timeout,
        client.execute(req.build().map_err(|e| e.to_string())?),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err(gettext("Connection timed out")),
    };
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
    let mut last_err = Retryable(gettext("Empty response"));
    for _ in 0..PIECE_TRIES {
        let req = stamp_request(
            ctx.client
                .get(&ctx.url)
                .header("Range", format!("bytes={start}-{end}")),
            &ctx.opts.user_agent,
            ctx.cookies.as_ref(),
            &ctx.url,
        );
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
                last_err = Retryable(gettext("Connection timed out"));
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

pub struct DownloadManager {
    // Borrow discipline: RefCells are never held across `set_*` property
    // notifies or `changed()` — GTK notifies re-enter through updater
    // closures that take shared borrows, so any borrow_mut added inside
    // a notify-reachable path panics at runtime with no compile-time
    // guard. Keep borrows scoped to the smallest statement.
    store: gio::ListStore,
    settings: crate::settings::AppSettings,
    running: RefCell<HashMap<u64, tokio::task::JoinHandle<()>>>,
    next_id: Cell<u64>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
    batch: Cell<u32>,
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
    /// Torrent per-piece haves by row (session-only, main thread, never
    /// persisted: re-polled from the session on every spawn).
    torrent_pieces: RefCell<HashMap<u64, Vec<bool>>>,
    /// Server-advertised Last-Modified by row, applied at Finished when
    /// the keep-server-date setting is on. Best-effort only.
    server_mtime: RefCell<HashMap<u64, SystemTime>>,
    /// Video-page source by row (in-memory only, like the maps above):
    /// persisted on [`StoredItem`] and re-staged on restore.
    video_sources: RefCell<HashMap<u64, crate::video::VideoSource>>,
    /// Abort senders for running resolver workers, by row. Signalled (then
    /// dropped) from pause/park/cancel paths so the worker stops its
    /// extractor streams promptly; the pump tail also drops them.
    video_abort: RefCell<HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
    /// Rows currently capturing a live stream. Pause/cancel/park only
    /// signal these (no task abort, no status preset): the worker
    /// finalizes the partial and its message drives the row to Done.
    /// Set per attempt in `spawn_video`; dropped on terminal messages.
    live_rows: RefCell<std::collections::HashSet<u64>>,
    /// Set by shutdown(): stale engine futures must not re-persist or
    /// re-mark rows once the authoritative shutdown persist has run.
    draining: Cell<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemovedSnapshot {
    pub url: String,
    pub dest_dir: String,
    pub filename: String,
    pub status: DownloadStatus,
    pub progress: f64,
    pub detail: String,
    pub output_dir: String,
    pub segments: Option<SegmentState>,
    /// Staged video source, so Undo on a video row restores the Page
    /// marker instead of demoting it to a plain download.
    pub video_source: Option<crate::video::VideoSource>,
}

/// Queue + engine owner: persists the queue, spawns downloads, notifies the UI.
impl DownloadManager {
    /// Create a manager over `store`; call [`DownloadManager::restore_queue`] once.
    pub fn new(store: gio::ListStore, settings: crate::settings::AppSettings) -> Rc<Self> {
        let this = Rc::new(Self {
            store,
            settings,
            running: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            on_change: RefCell::new(None),
            batch: Cell::new(0),
            pending_names: RefCell::new(HashMap::new()),
            queued: Cell::new(0),
            epoch: RefCell::new(HashMap::new()),
            segment_state: RefCell::new(HashMap::new()),
            torrent_pieces: RefCell::new(HashMap::new()),
            server_mtime: RefCell::new(HashMap::new()),
            video_sources: RefCell::new(HashMap::new()),
            video_abort: RefCell::new(HashMap::new()),
            live_rows: RefCell::new(std::collections::HashSet::new()),
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
            .connect_changed(Some(crate::settings::key::MAX_CONCURRENT), move |_, _| {
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
            .connect_changed(Some(crate::settings::key::SPEED_LIMIT), move |_, _| {
                if let Some(s) = settings_weak.upgrade() {
                    let s = crate::settings::AppSettings::from(s);
                    publish_rate_limit(&s);
                    crate::torrent::apply_live_limits(parse_rate(s.speed_limit().trim()));
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
    /// a 1000-line import into 1000 full rewrites. Nesting-safe counter:
    /// pairs of `begin_batch` / `end_batch` may overlap.
    pub fn begin_batch(&self) {
        self.batch.set(self.batch.get() + 1);
    }

    /// Persist once after a `begin_batch` block and refresh the UI.
    pub fn end_batch(self: &Rc<Self>) {
        self.batch.set(self.batch.get().saturating_sub(1));
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

    /// Preferences backing this queue (for dialog defaults).
    pub fn settings(&self) -> &crate::settings::AppSettings {
        &self.settings
    }

    /// Video-page source staged for a row, if any.
    pub fn video_source(&self, id: u64) -> Option<crate::video::VideoSource> {
        self.video_sources.borrow().get(&id).cloned()
    }

    /// Whether the row is currently capturing a live stream (stop-and-keep
    /// applies: pausing/cancelling finalizes instead of discarding).
    pub fn is_live_video(&self, id: u64) -> bool {
        self.live_rows.borrow().contains(&id)
    }

    /// Validate, dedupe and queue a download, starting it when a slot is free.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    /// Explicit absolute dest, else the effective download dir. Shared by
    /// enqueue and the torrent collision check so both agree on the folder.
    fn resolve_dir(&self, dest_dir: Option<&str>) -> String {
        // An explicit destination must be absolute: a relative dir would
        // resolve against the launcher CWD (and fail the sandbox). Restore
        // already rejects these; live input gets the same gate.
        dest_dir
            .filter(|s| !s.is_empty() && std::path::Path::new(s).is_absolute())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.effective_download_dir())
    }

    pub fn enqueue(
        self: &Rc<Self>,
        url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(url)?;
        let dir = self.resolve_dir(dest_dir);
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
        // Torrents record engine subfolders as output_dir, so the taken
        // check covers those too: a magnet stub must never equal a recorded
        // file-torrent folder (or vice versa).
        let name = dedupe_filename(&name, |n| {
            let p = std::path::Path::new(&dir).join(n);
            p.exists()
                // No part-namespace gate here: plain rows never reach
                // `clean_dest_parts` (remove() only sweeps video rows),
                // so their stems need no reservation.
                || (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .any(|it| {
                        (it.dest_dir() == dir && it.filename() == n)
                            || it.output_dir() == p.to_string_lossy()
                    })
        });
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        if crate::torrent::is_magnet(&url) {
            // Magnets carry no archive to recompute the output folder
            // from at delete time, so record their subfolder now: the
            // engine is spawned with this folder as its destination.
            // The deduped stub above is filesystem-safe by construction.
            item.set_output_dir(
                std::path::Path::new(&dir)
                    .join(&name)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
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
        let item = self.enqueue(&pseudo, dest_dir, Some(&stub))?;
        // The engine writes with overwrite:true, so a pre-existing
        // dest/<torrent-name> would be clobbered: record a deduped
        // subfolder the engine then uses as-is (delete/reveal follow the
        // recorded dir, and restore re-attaches it).
        if let Some((base, _)) = crate::torrent::intake_plan(&bytes) {
            let dir = self.resolve_dir(dest_dir);
            if std::path::Path::new(&dir).join(&base).exists() {
                let name = dedupe_filename(&base, |n| {
                    let p = std::path::Path::new(&dir).join(n);
                    p.exists()
                        // Subfolder claims need no part-namespace gate (see
                        // plain `enqueue` above): only video-row finished
                        // names reserve stems.
                        || (0..self.store.n_items())
                            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                            .any(|it| it.output_dir() == p.to_string_lossy())
                });
                item.set_output_dir(
                    std::path::Path::new(&dir)
                        .join(&name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        Ok(item)
    }

    /// Enqueue a video page: dialog-routed (listed domains) or probe-
    /// proven (unlisted pages with extractable media). No domain gate
    /// here — the dialog owns routing, and misuse fails loudly at
    /// resolve instead of silently saving HTML.
    ///
    /// [`crate::video::VideoSource::Page`] staged before insert, so the
    /// persist inside [`DownloadManager::insert`] already carries it and
    /// [`DownloadManager::start_next`] parks the row for the resolver
    /// worker instead of feeding the page to the HTTP engine.
    ///
    /// # Errors
    /// Returns a display-ready message when the URL or filename is invalid.
    pub fn enqueue_video(
        self: &Rc<Self>,
        page_url: &str,
        dest_dir: Option<&str>,
        filename: Option<&str>,
        choices: crate::video::VideoChoices,
    ) -> Result<DownloadItem, String> {
        let url = normalize_url(page_url)?;
        let dir = self.resolve_dir(dest_dir);
        let name = filename
            .filter(|s| sane_filename(s))
            .map(|s| s.to_string())
            .unwrap_or_else(|| filename_from_url(&url));
        let name = shorten_filename(&name);
        // One readdir per intake: the reservation probe below must not
        // stat the download dir once per dedupe candidate.
        let existing = crate::video::dir_file_names(std::path::Path::new(&dir));
        let name = dedupe_filename(&name, |n| {
            let p = std::path::Path::new(&dir).join(n);
            p.exists()
                || crate::video::stem_reserved_in(&existing, name_stem(n))
                || (0..self.store.n_items())
                    .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                    .any(|it| {
                        (it.dest_dir() == dir && it.filename() == n)
                            || it.output_dir() == p.to_string_lossy()
                    })
        });
        let item = DownloadItem::new(self.alloc_id(), &url, &name, &dir);
        item.set_detail(if choices.audio_only {
            gettext("Waiting to resolve audio…")
        } else {
            gettext("Waiting to resolve media…")
        });
        self.video_sources.borrow_mut().insert(
            item.id(),
            crate::video::VideoSource::Page {
                page_url: url,
                media_url: None,
                expires_at: None,
                quality: choices.quality,
                audio_only: choices.audio_only,
                is_live: choices.is_live,
                video_format_id: choices.video_format_id,
            },
        );
        Ok(self.insert(item))
    }

    /// Re-queue one persisted entry, preserving its intent (paused/failed stay).
    /// A validated piece bitmap resumes segmented instead of restarting.
    /// Names restore verbatim (renaming would break resume identity and
    /// recorded folders): a foreign part-namespaced file landing while
    /// the app is closed is the accepted residual — live claims reserve
    /// stems, restored ones predate the reservation.
    /// The video-page source (if any) is staged *before* insert so
    /// [`DownloadManager::start_next`] parks the row instead of spawning
    /// the HTTP engine on the watch page. A source whose page URL doesn't
    /// match the row is dropped (hand-edited queue file), same trust
    /// posture as the torrent folder check.
    ///
    /// # Errors
    /// Returns a display-ready message when the stored entry is invalid.
    pub fn restore_existing(
        self: &Rc<Self>,
        url: &str,
        dest_dir: &str,
        filename: &str,
        status: DownloadStatus,
        segments: Option<SegmentState>,
        video_source: Option<crate::video::VideoSource>,
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
        // Resumed rows requeue; only settled rows keep their status.
        item.set_status(match status {
            DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Done => status,
            _ => DownloadStatus::Queued,
        });
        if let Some(st) = segments {
            self.segment_state.borrow_mut().insert(item.id(), st);
        }
        if let Some(src) = video_source {
            let matches = matches!(&src, crate::video::VideoSource::Page { page_url, .. } if *page_url == url);
            if matches {
                let audio_only = matches!(
                    &src,
                    crate::video::VideoSource::Page {
                        audio_only: true,
                        ..
                    }
                );
                item.set_detail(if audio_only {
                    gettext("Waiting to resolve audio…")
                } else {
                    gettext("Waiting to resolve media…")
                });
                self.video_sources.borrow_mut().insert(item.id(), src);
            } else {
                tracing::warn!("dropping video source with mismatched page URL");
            }
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

    fn insert_history(
        self: &Rc<Self>,
        url: String,
        dir: String,
        name: String,
        progress: f64,
        output_dir: Option<String>,
    ) {
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
        // Already trust-checked by the caller (must sit inside `dir`).
        if let Some(folder) = output_dir {
            item.set_output_dir(folder);
        }
        let size = std::fs::metadata(item.file_path())
            .map(|m| m.len())
            .unwrap_or(0);
        item.set_detail(if size > 0 {
            gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
        } else {
            gettext("Finished")
        });
        // Restored duplicates collapse too: queue files written before
        // dedup may hold several Done rows per URL; the last one wins.
        self.drop_finished_duplicates(&url, item.id());
        self.insert(item);
    }

    /// Whether finish/fail desktop notifications are enabled.
    pub fn notifications_enabled(&self) -> bool {
        self.settings.show_notifications()
    }

    /// Whether closing over active downloads notifies.
    pub fn background_notifications_enabled(&self) -> bool {
        self.settings.notify_background()
    }

    /// Configured folder, or the system Downloads folder when empty or
    /// relative (a relative dir would resolve against the launcher CWD).
    pub fn effective_download_dir(&self) -> String {
        let configured = self.settings.download_dir();
        if !configured.is_empty() && std::path::Path::new(&configured).is_absolute() {
            return configured;
        }
        glib::user_special_dir(glib::UserDirectory::Downloads)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".to_string())
    }

    /// Simultaneous-download limit (at least 1).
    pub fn max_concurrent(&self) -> usize {
        self.settings.max_concurrent().max(1) as usize
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
        // Re-stash per attempt: a stale Last-Modified from a previous run
        // must never apply when the new run's server sends none.
        self.server_mtime.borrow_mut().remove(&item.id());
        let dest = item.file_path();
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Fail fast on junk; the running engine follows the live value,
        // so later edits apply without re-queueing.
        let limit = opts.limit_rate.trim();
        if !limit.is_empty() && limit != "0" && parse_rate(limit).is_none() {
            item.set_status(DownloadStatus::Failed);
            item.set_detail(gettext("Invalid speed limit: {limit}").replace("{limit}", limit));
            self.changed();
            return;
        }
        publish_rate_limit(&self.settings);
        let url = item.url().to_string();
        if crate::torrent::is_torrent(&url) {
            return self.spawn_torrent(item, url);
        }
        if let Some(crate::video::VideoSource::Page {
            page_url,
            quality,
            audio_only,
            is_live,
            video_format_id,
            ..
        }) = self.video_source(item.id())
        {
            return self.spawn_video(
                item,
                page_url,
                quality,
                audio_only,
                video_format_id,
                is_live,
            );
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
        let generation = self.epoch.borrow().get(&item.id()).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(item.id(), generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Invalid manual proxy fails the row loudly here: silently
        // routing direct would leak around an explicit user demand.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(e);
                self.changed();
                return;
            }
        };
        let ctx = FetchCtx {
            client: http_client_for(proxy.as_ref()),
            url,
            dest,
            opts,
            cookies: None,
            timeout,
            tx,
        };
        let handle = tokio_rt().spawn(run_download(ctx, connections, mode));
        self.running.borrow_mut().insert(item.id(), handle);
        item.set_status(DownloadStatus::Downloading);
        // Every attempt starts indeterminate: retries and resumes may
        // carry a stale fraction, which would otherwise freeze as a
        // determinate bar through the whole connecting phase (the row
        // pulses while progress is zero; the engine publishes real
        // fractions on its first tick).
        item.set_progress(0.0);
        let host = url::Url::parse(&item.url())
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        item.set_detail(if host.is_empty() {
            gettext("Starting…")
        } else {
            gettext("Connecting to {host}…").replace("{host}", &host)
        });
        self.changed();

        let item_id = item.id();
        self.pump(item, item_id, generation, rx);
    }

    /// Drain one engine's message channel into its row. Shared by the HTTP
    /// and torrent engines: every arm below is engine-generic, so arms the
    /// other engine never sends simply never fire.
    fn pump(
        self: &Rc<Self>,
        item: DownloadItem,
        id: u64,
        generation: u64,
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
        // The row URL never changes and its file filter is fixed at
        // intake: hoist both out of the per-tick path (no String alloc,
        // Vec clone or map lock on every progress message).
        let url = item.url().to_string();
        let sel_suffix = match crate::torrent::get_selection(&url) {
            Some(sel) => format!(" • {} files", sel.len()),
            None => String::new(),
        };
        glib::spawn_future_local(async move {
            while let Some(msg) = rx.recv().await {
                // Superseded by a newer spawn for this row: its progress
                // reports would drag the bar backwards, and its tail would
                // fail the row or steal the new engine's handle.
                if !this.is_current(id, generation) {
                    break;
                }
                match msg {
                    EngineMsg::Progress {
                        downloaded,
                        total,
                        uploaded,
                        upload_bps,
                    } => {
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
                        // Torrent upload counters (HTTP rows send zeros, so
                        // their labels stay exactly as before).
                        let up_suffix = if uploaded > 0 || upload_bps > 0 {
                            let ratio = total.filter(|&t| t > 0).map_or_else(String::new, |t| {
                                format!(" · ratio {:.1}", uploaded as f64 / t as f64)
                            });
                            gettext(" • ↑ {upspeed}/s · {up} up{ratio}")
                                .replace("{upspeed}", &fmt_bytes(upload_bps))
                                .replace("{up}", &fmt_bytes(uploaded))
                                .replace("{ratio}", &ratio)
                        } else {
                            String::new()
                        };
                        // Torrent rows with a file filter show the count
                        // (hoisted above: fixed for the row's lifetime).
                        match total {
                            Some(t) if t > 0 => {
                                let frac = (downloaded as f64 / t as f64).clamp(0.0, 1.0);
                                item.set_progress(frac);
                                let eta = if bps > 0.0 {
                                    fmt_eta((t.saturating_sub(downloaded) as f64 / bps) as u64)
                                } else {
                                    "—".to_string()
                                };
                                item.set_detail(
                                    gettext("{pct}% ({amounts}) • {speed} • About {eta} left{filter}{up}")
                                        .replace("{pct}", &((frac * 100.0) as u64).to_string())
                                        .replace("{amounts}", &format_amounts(downloaded, t))
                                        .replace("{speed}", &speed)
                                        .replace("{eta}", &eta)
                                        .replace("{filter}", &sel_suffix)
                                        .replace("{up}", &up_suffix),
                                );
                            }
                            _ => {
                                item.set_detail(
                                    gettext("{done} • {speed}{filter}{up}")
                                        .replace("{done}", &fmt_bytes(downloaded))
                                        .replace("{speed}", &speed)
                                        .replace("{filter}", &sel_suffix)
                                        .replace("{up}", &up_suffix),
                                );
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
                                let existing =
                                    crate::video::dir_file_names(std::path::Path::new(&dir));
                                let taken = |n: &str| {
                                    std::path::Path::new(&dir).join(n).exists()
                                        // Video engines never suggest today,
                                        // but this is a generic finished-name
                                        // claim: keep the reservation uniform
                                        // across all of them.
                                        || crate::video::stem_reserved_in(
                                            &existing,
                                            name_stem(n),
                                        )
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
                            // Server file date, when asked: best-effort (a
                            // read-only handle suffices; failure keeps the
                            // download-time mtime, never fails the row).
                            let mtime = this.server_mtime.borrow_mut().remove(&id);
                            if this.settings.keep_server_date()
                                && let Some(t) = mtime
                                && let Ok(f) = std::fs::File::open(item.file_path())
                            {
                                let _ = f.set_modified(t);
                            }
                            item.set_progress(1.0);
                            item.set_status(DownloadStatus::Done);
                            item.set_detail(if size > 0 {
                                gettext("Finished • {size}").replace("{size}", &fmt_bytes(size))
                            } else {
                                gettext("Finished")
                            });
                            // One finished record per URL (Parabolic parity):
                            // an older Done row for this URL leaves now, so
                            // re-downloads replace instead of stacking.
                            this.drop_finished_duplicates(&url, id);
                            this.segment_state.borrow_mut().remove(&id);
                            this.torrent_pieces.borrow_mut().remove(&id);
                            // Filtered torrents: drop the untoggled files
                            // (0-byte placeholders and shared-piece bytes)
                            // now that every selected byte is on disk.
                            // Skipped while seeding: serving still reads
                            // those spans, and deleting them would corrupt
                            // shared pieces for peers.
                            if crate::torrent::is_torrent(&url)
                                && !this.settings.torrent_seed_finished()
                            {
                                let folder = Self::torrent_folder(&item);
                                if folder.is_dir() {
                                    crate::torrent::cleanup_unselected(&folder, &url);
                                }
                                // The intake filter is spent: keeping it
                                // would pin the index list (and confuse a
                                // re-add) for a row that is done.
                                crate::torrent::drop_selection(&url);
                            }
                            this.notify_finished(&item, Ok(()));
                        }
                        done = true;
                        break;
                    }
                    EngineMsg::Failed(e) => {
                        // A failed download keeps its URL-derived name.
                        this.pending_names.borrow_mut().remove(&id);
                        this.server_mtime.borrow_mut().remove(&id);
                        if e == DEST_EXISTS
                            && item.status() != DownloadStatus::Cancelled
                            && item.status() != DownloadStatus::Paused
                        {
                            // A foreign file appeared at our path after
                            // dedupe: pick a fresh free name and requeue
                            // instead of failing. Fresh short of absurd
                            // collision counts (dedupe caps at 9999), so
                            // this terminates.
                            let dir = item.dest_dir().to_string();
                            let current = item.filename().to_string();
                            let existing = crate::video::dir_file_names(std::path::Path::new(&dir));
                            let new_name = dedupe_filename(&current, |n| {
                                std::path::Path::new(&dir).join(n).exists()
                                    || crate::video::stem_reserved_in(&existing, name_stem(n))
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
                            this.torrent_pieces.borrow_mut().remove(&id);
                            this.notify_finished(&item, Err(e));
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
                            this.notify_finished(&item, Err(e));
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
                    EngineMsg::TorrentPieces(have) => {
                        this.torrent_pieces.borrow_mut().insert(id, have);
                    }
                    EngineMsg::Phase(detail) => {
                        if item.status() == DownloadStatus::Downloading {
                            item.set_detail(detail);
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
                        if name == current || !sane_filename(&name) {
                            continue;
                        }
                        this.pending_names.borrow_mut().insert(id, name);
                    }
                    EngineMsg::LastModified(t) => {
                        // Latest attempt wins; applied at Finished when the
                        // keep-server-date setting is on.
                        this.server_mtime.borrow_mut().insert(id, t);
                    }
                }
            }
            // Superseded pump future: touch nothing, especially not the
            // new engine's handle in `running`.
            if !this.is_current(id, generation) {
                return;
            }
            this.running.borrow_mut().remove(&id);
            this.video_abort.borrow_mut().remove(&id);
            this.live_rows.borrow_mut().remove(&id);
            if this.draining.get() {
                return;
            }
            if !done && item.status() == DownloadStatus::Downloading {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(gettext("Download interrupted"));
                this.notify_finished(&item, Err(gettext("Download interrupted")));
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
        // Magnets recorded their subfolder at enqueue (no archive exists
        // to recompute it from later); archived torrents recompute theirs
        // from metadata, so plain dest stays correct for them.
        let recorded = item.output_dir().to_string();
        let dest = if recorded.is_empty() {
            dir.clone()
        } else {
            std::path::PathBuf::from(&recorded)
        };
        let _ = std::fs::create_dir_all(&dest);
        let settings = &self.settings;
        let seed_finished = settings.torrent_seed_finished();
        let peer_limit = crate::torrent::peer_limit_of(settings);
        let download_bps = parse_rate(settings.speed_limit().trim());
        let trackers = crate::torrent::parse_trackers(&settings.torrent_trackers());
        // Invalid manual proxy fails loudly like every other engine: no
        // silent direct torrent while the user asked for a tunnel.
        let proxy = match crate::download::DownloadOptions::from_settings(settings).proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(e);
                self.changed();
                return;
            }
        };
        // SOCKS5 takes over TCP peers + HTTP trackers (DHT, listener and
        // UDP trackers go dark alongside); anything else stays direct.
        let net = crate::torrent::plan_torrent_net(
            settings.torrent_dht(),
            settings.torrent_listen_port(),
            trackers,
            proxy.as_ref(),
        );
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let is_file = crate::torrent::is_torrent_url(&magnet);
        let source = if is_file {
            match crate::torrent::archive_path_for_url(&magnet) {
                Some(path) => crate::torrent::TorrentSource::File(path),
                None => {
                    item.set_status(DownloadStatus::Failed);
                    item.set_detail(gettext("Torrent file is missing from the archive"));
                    self.changed();
                    return;
                }
            }
        } else {
            crate::torrent::TorrentSource::Magnet(magnet)
        };
        let only_files = crate::torrent::get_selection(&item.url().to_string());
        // Intake-recorded collision subfolders (file torrents only) are
        // already final; magnets use their recorded dir by construction.
        let dest_is_final = !recorded.is_empty() && is_file;
        let handle = tokio_rt().spawn(crate::torrent::run_torrent(crate::torrent::TorrentJob {
            id,
            source,
            dest,
            seed_finished,
            seed_ratio: settings.torrent_seed_ratio(),
            seed_time_min: settings.torrent_seed_time(),
            dht: net.dht,
            peer_limit,
            download_bps,
            listen_port: net.listen_port,
            trackers: net.trackers,
            socks_proxy: net.socks_proxy,
            only_files,
            dest_is_final,
            tx,
        }));
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        // Attempts start indeterminate (see spawn): stale fractions must
        // not freeze as determinate bars through "Starting torrent…".
        item.set_progress(0.0);
        item.set_detail(gettext("Starting torrent…"));
        self.changed();
        self.pump(item, id, generation, rx);
    }

    /// Spawn the resolver worker for a video-page row. Mirrors `spawn`'s
    /// contract (epoch bump, running slot, Downloading status, shared pump)
    /// so pause, cancel, retry, persist and the stale-pump guard keep
    /// working unchanged. The worker speaks [`EngineMsg`] like every other
    /// engine; its abort sender lets pause/cancel stop the extractor
    /// streams promptly instead of only dropping the Grab-side task.
    fn spawn_video(
        self: &Rc<Self>,
        item: DownloadItem,
        page_url: String,
        quality: String,
        audio_only: bool,
        video_format_id: Option<String>,
        is_live: bool,
    ) {
        let id = item.id();
        let generation = self.epoch.borrow().get(&id).cloned().unwrap_or(0) + 1;
        self.epoch.borrow_mut().insert(id, generation);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
        // Overwrite any stale sender: its task is dead or guarded stale.
        self.video_abort.borrow_mut().insert(id, abort_tx);
        // Live rows finalize in the worker on stop: track them so
        // pause/cancel/park signal without aborting or presetting.
        if is_live {
            self.live_rows.borrow_mut().insert(id);
        } else {
            self.live_rows.borrow_mut().remove(&id);
        }
        let opts = DownloadOptions::from_settings(&self.settings);
        // Invalid manual proxy fails the row loudly here, before any
        // network happens: silently routing direct would leak around an
        // explicit user demand.
        let proxy = match opts.proxy_config() {
            Ok(proxy) => proxy,
            Err(e) => {
                item.set_status(DownloadStatus::Failed);
                item.set_detail(e);
                self.changed();
                return;
            }
        };
        let job = crate::video::VideoJob {
            item_id: id,
            page_url,
            quality,
            audio_only,
            dest: item.file_path(),
            tries: opts.tries.max(1) as u32,
            timeout_secs: opts.timeout.max(1) as u64,
            user_agent: opts.user_agent.clone(),
            video_format_id,
            is_live,
            newest_codecs: self.settings.video_codec_newest(),
            cookies_browser: self.settings.cookies_browser(),
            // Audio-only rows never subtitle (no video leg exists), so
            // resolve to `None` here rather than gating at every use.
            subtitles: if audio_only {
                None
            } else {
                crate::video::subtitle_lang_active(&self.settings.subtitle_language())
            },
            proxy,
        };
        let handle = tokio_rt().spawn(async move {
            let worker_tx = tx.clone();
            match crate::video::run_video_download(job, abort_rx, worker_tx).await {
                Ok(Some(size)) => {
                    tx.send(EngineMsg::Finished { size }).ok();
                }
                // Aborted: the pauser/canceller already set the row status,
                // so send nothing and let the pump tail no-op.
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(item_id = id, error = %e.to_string(), "video attempt failed");
                    tx.send(EngineMsg::Failed(e.to_string())).ok();
                }
            }
        });
        self.running.borrow_mut().insert(id, handle);
        item.set_status(DownloadStatus::Downloading);
        // Attempts start indeterminate (see spawn): a retried row may
        // carry its old fraction, which would otherwise sit frozen
        // through the whole "Resolving media…" phase.
        item.set_progress(0.0);
        item.set_detail(if audio_only {
            gettext("Resolving audio…")
        } else {
            gettext("Resolving media…")
        });
        self.changed();
        self.pump(item, id, generation, rx);
    }

    fn notify_finished(&self, item: &DownloadItem, result: Result<(), String>) {
        if !self.notifications_enabled() {
            return;
        }
        if let Some(app) = gio::Application::default() {
            let ok = result.is_ok();
            let n = gio::Notification::new(&if ok {
                gettext("Download finished")
            } else {
                gettext("Download failed")
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
            if let Err(h) = result {
                body.push_str(&format!("\n{h}"));
            }
            n.set_body(Some(&body));
            n.set_default_action_and_target_value("app.present", None);
            app.send_notification(Some(&format!("dl-{}", item.id())), &n);
        }
    }

    /// Pause a running download, freeing its slot for the next queued item.
    pub fn pause(self: &Rc<Self>, id: u64) {
        if self.live_rows.borrow().contains(&id) {
            // Live captures finalize in the worker: signal it and let
            // its Finished flip the row to Done. Aborting the task or
            // presetting Paused would drop the recording or strand it.
            // The pump tail frees the slot on completion.
            self.stop_video_worker(id);
            return;
        }
        self.stop_video_worker(id);
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
                item.set_detail(
                    gettext("Paused • {pct}%")
                        .replace("{pct}", &((item.progress() * 100.0) as u64).to_string()),
                );
            }
        }
        // Persist the bitmap too: a kill while paused must resume segmented.
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    /// Re-queue a paused download.
    pub fn resume(self: &Rc<Self>, id: u64) {
        if let Some(item) = self.find(id)
            && item.status() == DownloadStatus::Paused
        {
            item.set_status(DownloadStatus::Queued);
            self.persist_queue();
            self.changed();
            self.start_next();
        }
    }

    /// Rename a download: the list label plus the file on disk when it is
    /// already fetched. Only completed rows (a real file to move) and
    /// queued rows (nothing on disk yet) qualify: mid-transfer the engine
    /// owns the path, and torrent rows map to session outputs, not files.
    ///
    /// # Errors
    /// Returns a display-ready message when the row cannot be renamed.
    pub fn rename_download(self: &Rc<Self>, id: u64, new_name: &str) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        if crate::torrent::is_torrent(&item.url()) {
            return Err(gettext("Renaming torrent downloads isn't supported"));
        }
        match item.status() {
            DownloadStatus::Done | DownloadStatus::Queued => {}
            DownloadStatus::Downloading => {
                return Err(gettext("Pause or wait for the download to finish first"));
            }
            _ => {
                return Err(gettext(
                    "Only completed or not-yet-started downloads can be renamed",
                ));
            }
        }
        let name = new_name.trim();
        if name == item.filename() {
            return Ok(());
        }
        if name.is_empty() || !sane_filename(name) {
            return Err(gettext("That isn't a valid file name"));
        }
        if item.status() == DownloadStatus::Done {
            let new_path = std::path::PathBuf::from(item.dest_dir().to_string()).join(name);
            match rename_noreplace(&item.file_path(), &new_path) {
                Ok(()) => {}
                // Deleted behind our back: the label update below still applies.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!("Could not rename {}: {e}", item.filename()));
                }
            }
        }
        item.set_filename(name);
        self.persist_queue();
        self.changed();
        Ok(())
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

    /// Signal a running resolver worker to stop its extractor streams. The
    /// Grab task abort follows (or already ran): whichever wins, the
    /// attempt is over and a later resume replays from the sidecar.
    fn stop_video_worker(&self, id: u64) {
        if let Some(stop) = self.video_abort.borrow_mut().remove(&id) {
            let _ = stop.send(());
        }
    }

    /// Stop the engine for `id`, keeping file, bitmap and progress, and
    /// mark it queued. Unlike pause the row yields its slot; unlike cancel
    /// nothing is deleted and progress is kept. Live rows only signal:
    /// the worker finalizes and its message completes the row.
    fn park(&self, id: u64) {
        if self.live_rows.borrow().contains(&id) {
            self.stop_video_worker(id);
            return;
        }
        self.stop_video_worker(id);
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
                item.set_detail(item.status().label());
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
        if let Some(pos) = pos
            && let Some(obj) = self.store.item(pos)
        {
            self.store.remove(pos);
            self.store.append(&obj);
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
        self.cancel_inner(id, false);
        self.persist_queue();
        self.changed();
        self.start_next();
    }

    fn cancel_inner(&self, id: u64, keep_partial: bool) {
        // Live rows keep what's recorded (Stop, not Cancel): signal the
        // worker and skip the task abort plus the Cancelled preset, so
        // its Finished still lands. Discard via explicit row removal.
        let live = self.live_rows.borrow().contains(&id);
        self.stop_video_worker(id);
        if !live {
            if let Some(handle) = self.running.borrow().get(&id) {
                handle.abort();
            }
            self.running.borrow_mut().remove(&id);
        }
        self.pending_names.borrow_mut().remove(&id);
        let had_segments = self.segment_state.borrow_mut().remove(&id).is_some();
        self.torrent_pieces.borrow_mut().remove(&id);
        if let Some(item) = self.find(id) {
            // Segmented partials have holes: without the bitmap they can
            // never be appended to, so cancel drops the file — unless the
            // row may come back via Undo, which restores the bitmap.
            if had_segments && !keep_partial {
                let _ = std::fs::remove_file(item.file_path());
            }
            // Unfinished torrent rows drop their session entry but keep
            // partial files, so cancel/retry and remove/Undo resume instead
            // of restarting (explicit delete discards the files instead).
            if crate::torrent::is_torrent(&item.url()) && item.status() != DownloadStatus::Done {
                crate::torrent::forget_download(id);
            }
            if !live {
                item.set_status(DownloadStatus::Cancelled);
                item.set_detail(item.status().label());
            }
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
    /// The partial file is kept so Undo can resume segmented rows instead
    /// of restarting them (plain cancel still discards it).
    pub fn remove(self: &Rc<Self>, id: u64) {
        self.cancel_inner(id, true);
        self.epoch.borrow_mut().remove(&id);
        // Video rows keep split parts beside the finished file
        // (`<stem>.<kind>.<ext>`, yt-dlp defaults) plus the staging
        // sidecar: drop both with the row. Pause/cancel keep them for
        // resume; remove and delete never resume (an Undo'd row
        // re-extracts fresh URLs and restarts), so orphaned parts would
        // otherwise sit in Downloads until deleted by hand. Live rows are
        // exempt: their worker was only signaled, not aborted, and its
        // finalize path cleans up after itself.
        if !self.live_rows.borrow().contains(&id) && self.video_sources.borrow().contains_key(&id) {
            crate::video::clean_staging(&crate::video::staging_dir(id));
            if let Some(item) = self.find(id) {
                crate::video::clean_dest_parts(&item.file_path());
            }
        }
        // The snapshot carries the source for Undo; the live map drops it
        // with the row (cancel keeps it, remove doesn't).
        self.video_sources.borrow_mut().remove(&id);
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

    /// Segment bitmap snapshot for Undo: clone before [`DownloadManager::remove`]
    /// drops the row's live bitmap so [`DownloadManager::unremove`] can resume it.
    pub fn segments_of(&self, id: u64) -> Option<SegmentState> {
        self.segment_state.borrow().get(&id).cloned()
    }

    /// Re-insert a previously removed download (Undo). Restores the prior
    /// status except `Downloading`, which restarts as `Queued`. A restored
    /// segment bitmap resumes instead of restarting; a stale one (partial
    /// file gone) is dropped by the spawn-time file checks.
    pub fn unremove(self: &Rc<Self>, snap: RemovedSnapshot) -> DownloadItem {
        let item = DownloadItem::new(self.alloc_id(), &snap.url, &snap.filename, &snap.dest_dir);
        item.set_progress(snap.progress.clamp(0.0, 1.0));
        item.set_detail(snap.detail);
        item.set_status(match snap.status {
            DownloadStatus::Downloading => DownloadStatus::Queued,
            s => s,
        });
        item.set_output_dir(snap.output_dir);
        if let Some(st) = snap.segments {
            self.segment_state.borrow_mut().insert(item.id(), st);
        }
        // Re-stage before insert: the persist inside `insert` already
        // carries it and `start_next` dispatches on it.
        if let Some(src) = snap.video_source {
            self.video_sources.borrow_mut().insert(item.id(), src);
        }
        self.insert(item.clone());
        item
    }

    /// Engine's real output folder for a torrent row: recorded at enqueue
    /// (magnets), else recomputed from the archived .torrent, else the
    /// row's own path as a last resort.
    fn torrent_folder(item: &DownloadItem) -> std::path::PathBuf {
        let recorded = item.output_dir().to_string();
        if !recorded.is_empty() {
            return std::path::PathBuf::from(recorded);
        }
        let dest = std::path::PathBuf::from(item.dest_dir().to_string());
        crate::torrent::torrent_output_dir(&dest, &item.url()).unwrap_or_else(|| item.file_path())
    }

    /// Per-piece completion for the block map, native resolution:
    /// segmented HTTP bitmap, torrent session haves, or a prefix fill
    /// from byte progress for plain single-stream rows. Empty when the
    /// row is unknown or nothing is known yet.
    pub fn piece_bitmap(&self, id: u64) -> Vec<bool> {
        if let Some(st) = self.segment_state.borrow().get(&id) {
            return st.done.clone();
        }
        if let Some(have) = self.torrent_pieces.borrow().get(&id) {
            return have.clone();
        }
        let Some(item) = self.find(id) else {
            return Vec::new();
        };
        if crate::torrent::is_torrent(&item.url()) || item.progress() <= 0.0 {
            return Vec::new();
        }
        let filled = (item.progress().clamp(0.0, 1.0) * BLOCK_CELLS as f64) as usize;
        (0..BLOCK_CELLS).map(|i| i < filled).collect()
    }

    /// Move the downloaded file to Trash, then remove the row.
    ///
    /// # Errors
    /// Returns a display-ready message when trashing fails.
    pub fn delete_download(self: &Rc<Self>, id: u64) -> Result<(), String> {
        let item = self.find(id).ok_or_else(|| gettext("Download not found"))?;
        // Torrent rows (any status): drop the session entry and the
        // archive, drop the row, then Trash the real files. Multi-file
        // torrents live in dest/<torrent-name>/ rather than the stub path
        // (never written), so trash the folder recorded at enqueue
        // (magnets) or recomputed from the archived .torrent; a missing
        // path is fine when metadata never resolved. Matches the HTTP
        // delete contract (Trash, recoverable).
        if crate::torrent::is_torrent(&item.url()) {
            let path = Self::torrent_folder(&item);
            crate::torrent::forget_download(id);
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
        // Collected subtitle sidecars travel with the video: trash every
        // `<stem>.<lang>.srt` this row could own. The language pref is
        // global rather than per-row, so every offered code is a
        // candidate; failures only warn (the row delete must not fail
        // over a sidecar). Plain rows never wrote sidecars — gate on the
        // staged video source like remove()'s part cleanup does.
        if self.video_sources.borrow().contains_key(&id) {
            for lang in crate::video::subtitle_content_languages() {
                let sidecar = crate::video::sidecar_path_for(&item.file_path(), lang);
                match gio::File::for_path(&sidecar).trash(gio::Cancellable::NONE) {
                    Ok(()) => {}
                    Err(e) if e.kind::<gio::IOErrorEnum>() == Some(gio::IOErrorEnum::NotFound) => {}
                    Err(e) => tracing::warn!(
                        sidecar = %sidecar.display(),
                        error = %e,
                        "subtitle sidecar left behind"
                    ),
                }
            }
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
            |m, id| m.cancel_inner(id, false),
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

    /// Drop every finished row (files stay on disk). Still-seeding
    /// torrents leave the session first: clearing the record must not
    /// leave invisible uploading running. Returns the cleared count.
    pub fn clear_finished(self: &Rc<Self>) -> usize {
        let before = self.store.n_items();
        self.for_matching(
            |s| matches!(s, DownloadStatus::Done),
            |m, id| m.drop_finished_row(id),
        );
        let n = (before - self.store.n_items()) as usize;
        if n > 0 {
            // Same hygiene as a restart: archives and staged selections
            // no remaining row references go now (cleared rows have no
            // Undo path holding them, unlike `remove`).
            let referenced: std::collections::HashSet<String> = (0..self.store.n_items())
                .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
                .map(|it| it.url().to_string())
                .filter(|u| crate::torrent::is_torrent_url(u))
                .collect();
            crate::torrent::sweep_archives(&referenced);
            crate::torrent::prune_selections(&referenced);
            self.persist_queue();
            self.changed();
        }
        n
    }

    /// Finished rows (for the clear-finished confirm and toast counts).
    pub fn finished_count(&self) -> usize {
        (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| it.status() == DownloadStatus::Done)
            .count()
    }

    /// Drop finished rows for `url` other than `keep_id`: one finished
    /// record per URL (Parabolic parity). Only Done rows — active,
    /// paused, failed and cancelled rows are user intent and never
    /// touched. Unnormalizable URLs skip quietly (rows are validated
    /// at intake, so this is unreachable in practice). Store-only;
    /// callers persist.
    pub(crate) fn drop_finished_duplicates(&self, url: &str, keep_id: u64) {
        let Ok(key) = normalize_url(url) else {
            return;
        };
        let ids: Vec<u64> = (0..self.store.n_items())
            .filter_map(|i| self.store.item(i).and_downcast::<DownloadItem>())
            .filter(|it| {
                it.status() == DownloadStatus::Done
                    && it.id() != keep_id
                    && normalize_url(&it.url()).is_ok_and(|u| u == key)
            })
            .map(|it| it.id())
            .collect();
        for id in ids {
            self.drop_finished_row(id);
        }
    }

    /// Drop one finished row: a lingering torrent session leaves first
    /// (see `clear_finished`), staged sources and epochs release, and
    /// the row leaves the store. Files stay on disk. Done rows hold no
    /// engines, bitmaps or partials, so unlike `remove` there is nothing
    /// to cancel, snapshot or clean.
    fn drop_finished_row(&self, id: u64) {
        if let Some(item) = self.find(id)
            && crate::torrent::is_torrent(&item.url())
        {
            crate::torrent::forget_download(id);
        }
        self.video_sources.borrow_mut().remove(&id);
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
    fn is_current(&self, id: u64, generation: u64) -> bool {
        self.epoch.borrow().get(&id).cloned().unwrap_or(0) == generation
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
        // Test seam only (debug builds run the suite against temp files):
        // release builds always use the real location, so a crafted
        // launcher environment can never redirect queue state.
        if cfg!(debug_assertions)
            && let Some(p) = std::env::var_os("GRAB_QUEUE_FILE")
        {
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
        if self.batch.get() > 0 {
            return;
        }
        let mut items = Vec::new();
        for i in 0..self.store.n_items() {
            if let Some(it) = self.store.item(i).and_downcast::<DownloadItem>() {
                // Cancelled rows carry no intent: never persisted.
                if it.status() != DownloadStatus::Cancelled {
                    let segments = self.segment_state.borrow().get(&it.id()).cloned();
                    let selected_files = crate::torrent::get_selection(&it.url().to_string());
                    // Empty means "no folder tracked": omit it so old files
                    // stay clean and old app versions keep reading new ones.
                    // Same for the video source: only Page rows write it.
                    let output_dir = it.output_dir().to_string();
                    let output_dir = (!output_dir.is_empty()).then_some(output_dir);
                    let video_source = self
                        .video_source(it.id())
                        .filter(|s| matches!(s, crate::video::VideoSource::Page { .. }));
                    items.push(StoredItem {
                        url: it.url().to_string(),
                        dest_dir: it.dest_dir().to_string(),
                        filename: it.filename().to_string(),
                        status: it.status(),
                        progress: it.progress(),
                        segments,
                        selected_files,
                        output_dir,
                        video_source,
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
                if let Some(parent) = Self::queue_file().parent()
                    && let Ok(dir) = std::fs::File::open(parent)
                {
                    let _ = dir.sync_all();
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
                    .partition(|it| !matches!(it.status, DownloadStatus::Done));
                active.truncate(MAX_QUEUE_ITEMS);
                let skip = done.len().saturating_sub(MAX_QUEUE_ITEMS - active.len());
                items = active
                    .into_iter()
                    .chain(done.into_iter().skip(skip))
                    .collect();
            }
            self.batch.set(self.batch.get() + 1);
            // Two phases: every insert below can spawn an engine via
            // start_next, so collect all restore decisions first and only
            // then apply them — a mid-loop validation failure can no longer
            // leave half-spawned engines behind.
            struct PendingRestore {
                item: StoredItem,
                output_dir: Option<String>,
                segments: Option<SegmentState>,
            }
            let mut pending = Vec::with_capacity(items.len());
            for item in items {
                // The recorded engine folder is only trusted when it sits
                // directly inside the row's own dest; otherwise it stays
                // unknown and delete falls back to the recomputed path.
                let output_dir = item.output_dir.clone().filter(|dir| {
                    let folder = std::path::PathBuf::from(dir);
                    folder.is_absolute()
                        && folder
                            .parent()
                            .is_some_and(|p| p == std::path::Path::new(&item.dest_dir))
                });
                // Mirror restore_existing's cheap validations now, so the
                // apply phase below cannot fail (and strand engines) partway.
                if normalize_url(&item.url).is_err() {
                    tracing::warn!("skipping queue entry with bad URL");
                    continue;
                }
                if !sane_filename(&item.filename) {
                    tracing::warn!(
                        "skipping queue entry: Invalid filename in queue: {}",
                        item.filename
                    );
                    continue;
                }
                if !std::path::Path::new(&item.dest_dir).is_absolute() {
                    tracing::warn!(
                        "skipping queue entry: Invalid destination in queue: {}",
                        item.dest_dir
                    );
                    continue;
                }
                // A stored bitmap resumes segmented; anything
                // misshapen is dropped (single-stream fallback stays
                // correct via the spawn-time file checks).
                let segments = match &item.segments {
                    Some(s)
                        if s.total > 0
                            && s.total <= MAX_SEGMENTED_TOTAL
                            && s.done.len() == s.total.div_ceil(piece_len(s.total)) as usize =>
                    {
                        Some(s.clone())
                    }
                    _ => None,
                };
                pending.push(PendingRestore {
                    item,
                    output_dir,
                    segments,
                });
            }
            for p in pending {
                match p.item.status {
                    DownloadStatus::Done => {
                        self.insert_history(
                            p.item.url,
                            p.item.dest_dir,
                            p.item.filename,
                            p.item.progress,
                            p.output_dir,
                        );
                    }
                    status => {
                        match self.restore_existing(
                            &p.item.url,
                            &p.item.dest_dir,
                            &p.item.filename,
                            status,
                            p.segments,
                            p.item.video_source.clone(),
                        ) {
                            Ok(restored) => {
                                // Re-attach the recorded engine folder.
                                if let Some(dir) = p.output_dir.clone() {
                                    restored.set_output_dir(dir);
                                }
                                // Re-stage the intake file selection: the
                                // live map is in-memory only, so without
                                // this a restart drops the filter and the
                                // resume downloads every file.
                                if let Some(sel) = p.item.selected_files {
                                    crate::torrent::stage_selection(&restored.url(), sel);
                                }
                            }
                            Err(e) => tracing::warn!("skipping queue entry: {e}"),
                        }
                    }
                }
            }
            self.batch.set(self.batch.get().saturating_sub(1));
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
            if let Some(item) = self.find(id)
                && let Some(st) = self.segment_state.borrow_mut().get_mut(&id)
            {
                truncate_to_prefix(&item.file_path(), st);
                st.forget_beyond_prefix();
            }
        }
        // Persist BEFORE returning: the bitmaps are what let the next launch
        // resume segmented instead of restarting. Pump tails exit silently
        // once draining is set, so this is the only persist that matters.
        self.persist_queue();
    }
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
