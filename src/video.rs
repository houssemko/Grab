//! Video-page downloads (YouTube, Vimeo, …) powered by the bundled yt-dlp
//! binaries.
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
//! * **The tools may be missing.** The Flatpak bundle ships `yt-dlp` in
//!   `/app/bin` and ffmpeg in the runtime, but tarball/dev builds rely on
//!   the user library directory ([`user_lib_dir`]), populated by
//!   [`install_ytdlp`] and [`install_ffmpeg`]. Callers detect the gap with
//!   [`resolve_libraries`] and offer an install action.
//!
//! All yt-dlp work runs on Grab's shared Tokio runtime
//! ([`crate::download::tokio_rt`]) so no GTK thread is ever blocked.

use gettextrs::gettext;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::oneshot;
use yt_dlp::Downloader;
use yt_dlp::client::deps::{Libraries, LibraryInstaller};
use yt_dlp::model::format::{Extension, Format, FormatType, HttpHeaders, Protocol};
use yt_dlp::model::selector::{
    AudioCodecPreference, AudioQuality, VideoCodecPreference, VideoQuality,
};
use yt_dlp::model::{DrmStatus, FORMAT_URL_LIFETIME, Video};
use yt_dlp::{DownloadPriority, DownloadStatus as YtDownloadStatus};

/// Values for the `video-quality` GSettings key and the per-item quality
/// stored on [`VideoSource::Page`]. Index-aligned with the ComboRow models
/// in Preferences and the New Download dialog — see [`quality_index`] and
/// [`quality_value`]. Labels are translated at the call sites.
pub const VIDEO_QUALITY_VALUES: &[&str] = &["best", "2160p", "1440p", "1080p", "720p", "480p"];

/// Default quality when nothing is stored (or the stored value is unknown).
pub fn default_video_quality() -> String {
    "1080p".to_string()
}

/// Model index for a stored quality value. Unknown values fall back to the
/// 1080p row so a hand-edited dconf key can't desync the combo.
pub fn quality_index(value: &str) -> usize {
    VIDEO_QUALITY_VALUES
        .iter()
        .position(|v| *v == value)
        .unwrap_or(3)
}

/// Stored value for a combo index. Out-of-range indexes (shouldn't happen,
/// but the model is rebuilt from translations) fall back to 1080p.
pub fn quality_value(index: usize) -> &'static str {
    VIDEO_QUALITY_VALUES.get(index).copied().unwrap_or("1080p")
}

/// Default file name for a resolved video when the user left the name
/// blank: the video title plus the container the worker will produce
/// (.mp4 merged, .m4a audio-only). The intake sanitizes it further.
pub fn default_video_filename(title: &str, audio_only: bool) -> String {
    if audio_only {
        format!("{title}.m4a")
    } else {
        format!("{title}.mp4")
    }
}

/// Translated ComboRow labels, index-aligned with [`VIDEO_QUALITY_VALUES`].
/// Shared by Preferences and the New Download dialog so both combos stay
/// in the same order.
pub fn quality_labels() -> Vec<String> {
    vec![
        gettext("Best"),
        gettext("2160p"),
        gettext("1440p"),
        gettext("1080p"),
        gettext("720p"),
        gettext("480p"),
    ]
}
/// Hosts routed through the video extractor instead of the plain HTTP
/// engine. Suffix-matched (`music.youtube.com` counts), lowercase.
/// DRM-free sites only: DRM-walled services (Netflix and kin) fail cleanly
/// at selection time, so listing them here would only promise what the
/// pipeline refuses to fetch. Extend here as verified sites grow — this is
/// the only place that decides what a "video page" is.
const VIDEO_DOMAINS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "vimeo.com",
    "dailymotion.com",
    "tiktok.com",
    "twitch.tv",
    "kick.com",
    "rumble.com",
    "streamable.com",
    "reddit.com",
    "twitter.com",
    "x.com",
    "bilibili.com",
    "instagram.com",
    "facebook.com",
    "fb.watch",
    "threads.com",
    "bsky.app",
    "pinterest.com",
    "pin.it",
    "tumblr.com",
    "vk.com",
    "ok.ru",
    "coub.com",
    "bitchute.com",
    "odysee.com",
    "rutube.ru",
    "nicovideo.jp",
    "ted.com",
    "archive.org",
    "drive.google.com",
    "dropbox.com",
    "mediafire.com",
    "loom.com",
    "wistia.com",
    "wistia.net",
    "soundcloud.com",
    "bandcamp.com",
];

/// Domains whose pages carry no video tracks (audio-first services). The
/// New Download dialog presets Audio-only for these; the worker also falls
/// back to audio-only on its own when no usable video format selects.
const AUDIO_FIRST_DOMAINS: &[&str] = &["soundcloud.com", "bandcamp.com"];

/// Per-row video choices from the New Download dialog: quality
/// preset, mode, format pin and liveness. Bundled so intake entry
/// points stay under the argument-count lint.
#[derive(Debug, Clone)]
pub struct VideoChoices {
    pub quality: String,
    pub audio_only: bool,
    pub video_format_id: Option<String>,
    pub is_live: bool,
}

/// Where a to-be-downloaded resource comes from. Serialized into the queue
/// file, so it stays stable across releases.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VideoSource {
    /// Ordinary file for the plain HTTP engine (or magnet/torrents handled
    /// elsewhere). The default for anything that is not a known video page.
    Direct,
    /// Media behind a page that needs yt-dlp. The persisted identity is the
    /// *page* URL; `media_url`/`expires_at` are transient — when missing or
    /// expired the page is extracted again. `quality`/`audio_only` are the
    /// per-item choices (initialized from Preferences, then independent).
    Page {
        page_url: String,
        media_url: Option<String>,
        expires_at: Option<i64>,
        #[serde(default = "default_video_quality")]
        quality: String,
        #[serde(default)]
        audio_only: bool,
        /// Whether the page is a live stream. Persisted so rows restored
        /// across launches keep their live behavior (stop-and-keep
        /// instead of pause/cancel); refreshed on every resolve.
        #[serde(default)]
        is_live: bool,
        /// Pinned video format id chosen in the dialog (`None` = the
        /// quality preset decides at attempt time). Falls back to the
        /// preset when the id vanishes from fresh metadata.
        #[serde(default)]
        video_format_id: Option<String>,
    },
}

/// Decide whether `url` goes through the video extractor.
///
/// [`crate::download::normalize_url`] runs first, so callers pass a fully
/// qualified `http(s)` URL; anything else (magnets, bare hosts, unknown
/// schemes) is [`VideoSource::Direct`].
pub fn classify(url: &str) -> VideoSource {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => {
            let is_video = u.host_str().is_some_and(video_domain);
            if is_video {
                VideoSource::Page {
                    page_url: u.to_string(),
                    media_url: None,
                    expires_at: None,
                    quality: default_video_quality(),
                    audio_only: false,
                    is_live: false,
                    video_format_id: None,
                }
            } else {
                VideoSource::Direct
            }
        }
        _ => VideoSource::Direct,
    }
}

/// Convenience predicate for the enqueue/restore paths.
pub fn is_video_page(url: &str) -> bool {
    matches!(classify(url), VideoSource::Page { .. })
}

/// Whether a resolved preview still matches the dialog's current text.
/// The dialog kick and the submit gate must agree on this: the extractor
/// canonicalizes page URLs (youtu.be → youtube.com/watch), so comparing
/// the stored canonical URL against the typed text would reject every
/// canonicalized preview and trap Add in a re-resolve loop. The
/// round-trip key (which exact text was resolved) is the stable one.
pub fn preview_fresh(info: &Option<VideoInfo>, last_ok: &str, url: &str) -> bool {
    !url.is_empty() && last_ok == url && info.is_some()
}

fn video_domain(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    VIDEO_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Whether a video-page URL belongs to an audio-first service (no video
/// tracks expected). Used to preset the dialog; the worker re-derives the
/// effective mode from the selected formats anyway.
pub fn is_audio_first(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|host| {
            AUDIO_FIRST_DOMAINS
                .iter()
                .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        })
}

/// Unix timestamp now, seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a resolved media URL is no longer usable. `None` means "not
/// resolved / unknown" and counts as expired so restores always re-extract
/// until a fresh URL is stored.
pub fn is_expired(expires_at: Option<i64>) -> bool {
    match expires_at {
        None => true,
        Some(t) => now_unix() >= t,
    }
}

/// Errors surfaced by the video pipeline. User-facing strings are translated
/// at construction; match on the variant to branch the UI
/// (install banner vs retry toast).
#[derive(Debug, Error)]
pub enum VideoError {
    /// Neither the Flatpak bundle nor the user library dir nor PATH has the
    /// yt-dlp/ffmpeg tools. Offer the install flow.
    #[error("{0}")]
    MissingLibraries(String),
    /// The page could not be extracted (wrong URL, offline, …). Retryable.
    #[error("{0}")]
    Fetch(String),
    /// Background task machinery failed (panic/join) — internal.
    #[error("{0}")]
    Runtime(String),
    /// Other translated failure.
    #[error("{0}")]
    Message(String),
}

impl VideoError {
    fn missing_tools() -> Self {
        Self::MissingLibraries(gettext("Media downloads need the yt-dlp support tools"))
    }
    fn fetch(e: impl std::fmt::Display) -> Self {
        Self::Fetch(
            gettext("Couldn't read the video page: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    fn install(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't install the media support tools: {detail}")
                .replace("{detail}", &e.to_string()),
        )
    }
    fn staging(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't prepare video staging: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    fn runtime(e: impl std::fmt::Display) -> Self {
        Self::Runtime(e.to_string())
    }
    fn unavailable() -> Self {
        Self::Message(gettext("No suitable formats found for this media"))
    }
    /// Same failure with a rejection census, so a page of manifest-only
    /// variants (live/HLS pages) reads differently from a DRM or
    /// link-less one instead of guessing.
    fn unavailable_detail(formats: &[Format]) -> Self {
        let total = formats.len();
        if total == 0 {
            return Self::Message(gettext(
                "No suitable formats found for this media (the page listed none)",
            ));
        }
        let (mut manifest, mut drm, mut no_link, mut video_only, mut unclassified) =
            (0, 0, 0, 0, 0);
        for f in formats {
            if f.protocol != Protocol::Https {
                manifest += 1;
            } else if matches!(f.has_drm, Some(DrmStatus::Yes)) {
                drm += 1;
            } else if f
                .download_info
                .url
                .as_deref()
                .filter(|u| !u.is_empty())
                .is_none()
            {
                no_link += 1;
            } else if f.format_type() == FormatType::Unknown {
                unclassified += 1;
            } else if f
                .codec_info
                .audio_codec
                .as_deref()
                .is_none_or(|c| c == "none")
            {
                video_only += 1;
            }
        }
        let mut reasons = Vec::new();
        for (n, label) in [
            (manifest, gettext("manifest")),
            (drm, gettext("DRM")),
            (no_link, gettext("no link")),
            (video_only, gettext("video-only")),
            (unclassified, gettext("unclassified")),
        ] {
            if n > 0 {
                reasons.push(format!("{label}: {n}"));
            }
        }
        Self::Message(
            gettext("No suitable formats found for this media ({total} listed: {reasons})")
                .replace("{total}", &total.to_string())
                .replace("{reasons}", &reasons.join(", ")),
        )
    }
    fn part_failed(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Media download failed: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    fn combine(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't merge video and audio: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    fn interrupted() -> Self {
        Self::Message(gettext("Download interrupted"))
    }
    fn outdated() -> Self {
        Self::Message(gettext("Video tools are too old — update them to continue"))
    }
    /// The exact [`crate::download::DEST_EXISTS`] sentence, so the pump's
    /// foreign-file requeue path picks a fresh name and retries the merge.
    fn exists() -> Self {
        Self::Message(crate::download::DEST_EXISTS.to_string())
    }
}

/// Extraction result, kept deliberately small: the queue row needs the
/// title/duration, and the *page URL* for expiry-safe re-resolve.
#[derive(Clone, Debug)]
pub struct VideoInfo {
    /// Extractor video id (not persisted; informational).
    pub id: String,
    pub title: String,
    /// Duration in seconds.
    pub duration: Option<i64>,
    /// Preformatted duration from the extractor (e.g. "41:21").
    pub duration_string: Option<String>,
    /// Canonical page URL — the identity persisted across restarts.
    pub page_url: String,
    /// Unix time after which every resolved format URL is stale, derived
    /// from the youngest `available_at` across formats.
    pub expires_at: Option<i64>,
    /// Pinnable video-only formats, tallest first (empty when the page
    /// carries none). Computed once at resolve; the dialog lists these.
    pub formats: Vec<VideoFormatOption>,
    /// Whether the page is currently live. Decides stop-and-keep
    /// behavior for HLS captures; refreshed on every resolve.
    pub is_live: bool,
}

impl VideoInfo {
    fn from(v: &Video, fallback_page: &str) -> Self {
        let page_url = v
            .webpage_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .unwrap_or(fallback_page)
            .to_string();
        let expires_at = v
            .formats
            .iter()
            .filter_map(|f| f.available_at)
            .min()
            .map(|t| t + FORMAT_URL_LIFETIME);
        Self {
            id: v.id.clone(),
            title: v.title.clone(),
            duration: v.duration,
            duration_string: v.duration_string.clone(),
            page_url,
            expires_at,
            formats: video_format_options(v),
            is_live: v.is_live.unwrap_or(false),
        }
    }
}

/// Whether we run inside the Flatpak sandbox. Only there is the Install
/// button the viable path (users cannot install host packages into the
/// sandbox); tarball/dev builds get guided self-install instead.
pub(crate) fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Package manager commands for the detected distro. `None` means unknown
/// distro: show manual install links instead of a wrong command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DistroPackages {
    /// Pretty distro name for the dialog title, e.g. "Fedora".
    pub distro: String,
    /// Full install command for yt-dlp, e.g. "sudo dnf install yt-dlp".
    pub yt_dlp: String,
    /// Full install command for ffmpeg.
    pub ffmpeg: String,
}

fn package_manager(id: &str) -> Option<&'static str> {
    match id {
        "fedora" | "rhel" | "centos" | "almalinux" | "rocky" => Some("sudo dnf install"),
        "ubuntu" | "debian" | "pop" | "linuxmint" | "elementary" | "zorin" => {
            Some("sudo apt install")
        }
        "arch" | "manjaro" | "endeavouros" | "cachyos" => Some("sudo pacman -S"),
        "opensuse-tumbleweed" | "opensuse-leap" | "sles" | "opensuse" => {
            Some("sudo zypper install")
        }
        "alpine" => Some("sudo apk add"),
        "gentoo" => Some("sudo emerge --ask"),
        "void" => Some("sudo xbps-install -S"),
        "solus" => Some("sudo eopkg install"),
        _ => None,
    }
}

/// Parse `/etc/os-release` content into install commands. Takes the file
/// content (not the path) so unit tests feed fixtures directly. Falls
/// back to `ID_LIKE` tokens when `ID` itself is unknown.
pub(crate) fn distro_packages(os_release: &str) -> Option<DistroPackages> {
    let mut id: Option<&str> = None;
    let mut id_like = "";
    let mut name: Option<&str> = None;
    for line in os_release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        match key {
            "ID" => id = Some(value),
            "ID_LIKE" => id_like = value,
            "NAME" => name = Some(value),
            _ => {}
        }
    }
    let id = id?;
    let pm =
        package_manager(id).or_else(|| id_like.split_whitespace().find_map(package_manager))?;
    Some(DistroPackages {
        distro: name.unwrap_or(id).to_string(),
        yt_dlp: format!("{pm} yt-dlp"),
        ffmpeg: format!("{pm} ffmpeg"),
    })
}
/// Directory where dev/tarball installs keep the yt-dlp and ffmpeg
/// binaries: `$XDG_DATA_HOME/grab/libs` (Flatpak bundles live in /app/bin,
/// so this is unused there).
pub fn user_lib_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| std::env::temp_dir().join("grab-fallback-data"));
    base.join("grab").join("libs")
}

/// Bundled-tool directory inside the Flatpak sandbox. This is Flatpak
/// convention (the app tree is mounted at `/app`), not an XDG standard —
/// XDG only defines user directories, never bundle layouts. Absent
/// outside Flatpak, where the lookup simply skips it.
const FLATPAK_APP_BIN: &str = "/app/bin";

/// Candidate directories for the tools, in priority order: the user's own
/// installs first (so Update actually takes effect over the bundle),
/// then the Flatpak bundle, then PATH (which inside Flatpak includes the
/// runtime's /usr/bin where ffmpeg ships). A stale user copy cannot pin
/// old tools: the version floor refuses it with an update prompt.
fn tool_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![user_lib_dir(), PathBuf::from(FLATPAK_APP_BIN)];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()));
    }
    dirs
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

/// Locate the yt-dlp and ffmpeg binaries, or say why they are missing.
pub fn resolve_libraries() -> Result<Libraries, VideoError> {
    let dirs = tool_search_dirs();
    let youtube = find_in_dirs("yt-dlp", &dirs).ok_or_else(VideoError::missing_tools)?;
    let ffmpeg = find_in_dirs("ffmpeg", &dirs).ok_or_else(VideoError::missing_tools)?;
    Ok(Libraries::new(youtube, ffmpeg))
}

/// Install just yt-dlp into the user library dir (tarball/dev builds).
/// Split from ffmpeg so the UI can report honest per-tool stages; the
/// crate installer exposes no progress of its own. Await from a spawned
/// task — never block the GTK thread on it.
pub async fn install_ytdlp() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::download::tokio_rt()
        .spawn(async move { LibraryInstaller::new(dir).install_youtube(None).await });
    match handle.await {
        Ok(Ok(path)) => Ok(path),
        Ok(Err(e)) => Err(VideoError::install(&e)),
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// Install just ffmpeg into the user library dir. See [`install_ytdlp`].
pub async fn install_ffmpeg() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::download::tokio_rt()
        .spawn(async move { LibraryInstaller::new(dir).install_ffmpeg(None).await });
    match handle.await {
        Ok(Ok(path)) => Ok(path),
        Ok(Err(e)) => Err(VideoError::install(&e)),
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// Minimum accepted yt-dlp version by release date. Older binaries predate
/// the JS-challenge era and fail extraction in ways that look like broken
/// pages; refusing them with an actionable message beats a mystery
/// failure. Newer versions always pass.
pub const MIN_YTDLP_VERSION: [u32; 3] = [2026, 1, 1];

/// Parse a `yt-dlp --version` first line (`2026.08.19`) into comparable
/// parts. Anything else (nightlies, forks, garbage) is unverifiable.
pub(crate) fn parse_yt_dlp_version(first_line: &str) -> Option<[u32; 3]> {
    let mut parts = first_line.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some([major, minor, patch])
}

/// Whether a release `tag` is newer than the installed version line.
/// Unparseable tags never trigger an update prompt: an unknown upstream
/// shape must not nag. Tags carry no `v` prefix (`2026.08.19`), but one
/// is tolerated.
pub(crate) fn ytdlp_update_available(installed: &str, tag: &str) -> bool {
    match (
        parse_yt_dlp_version(installed),
        parse_yt_dlp_version(tag.trim_start_matches('v')),
    ) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

/// Latest released yt-dlp tag without downloading anything: one
/// user-initiated GitHub API call for the update check. `None` on any
/// network/API failure — the row then reports the check failed instead
/// of prompting.
pub async fn latest_ytdlp_tag() -> Option<String> {
    let handle = crate::download::tokio_rt().spawn(async move {
        let fetcher = yt_dlp::client::deps::github::GitHubFetcher::new("yt-dlp", "yt-dlp");
        fetcher
            .fetch_latest_release(None)
            .await
            .ok()
            .map(|release| release.tag_name)
    });
    handle.await.ok().flatten()
}

/// Run `binary --version` off the caller's thread and return its first
/// output line. `None` covers missing binaries, spawn failures and empty
/// output alike — all mean "unusable".
async fn tool_first_line(binary: PathBuf, version_arg: &'static str) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(&binary)
            .arg(version_arg)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.lines().next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
    })
    .await
    .ok()
    .flatten()
}

/// Refuse stale or unverifiable toolchains before any network happens.
/// Returns the raw version lines for attempt logging.
pub(crate) async fn ensure_tool_versions(libs: &Libraries) -> Result<(String, String), VideoError> {
    // ffmpeg takes a single-dash -version; --version is an error there.
    let (yt, ff) = tokio::join!(
        tool_first_line(libs.youtube.clone(), "--version"),
        tool_first_line(libs.ffmpeg.clone(), "-version")
    );
    let yt = yt.ok_or_else(VideoError::missing_tools)?;
    let fresh = parse_yt_dlp_version(&yt).is_some_and(|v| v >= MIN_YTDLP_VERSION);
    if !fresh {
        return Err(VideoError::outdated());
    }
    let ff = ff.ok_or_else(VideoError::missing_tools)?;
    Ok((yt, ff))
}
/// Host part of a URL for logs. Full page URLs can carry tokens; the
/// journal gets the host, never the query string.
pub(crate) fn page_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// Browsers offered for `--cookies-from-browser`, in combo order. Values
/// are the yt-dlp browser names; labels come from [`cookies_browser_labels`].
pub const COOKIES_BROWSERS: &[&str] = &[
    "none", "brave", "chrome", "chromium", "edge", "firefox", "opera", "vivaldi", "whale",
];

/// Translated combo labels, index-aligned with [`COOKIES_BROWSERS`].
/// Browser names are proper nouns and stay untranslated; only None is prose.
pub fn cookies_browser_labels() -> Vec<String> {
    let mut labels = vec![
        "Brave".to_string(),
        "Chrome".to_string(),
        "Chromium".to_string(),
        "Edge".to_string(),
        "Firefox".to_string(),
        "Opera".to_string(),
        "Vivaldi".to_string(),
        "Whale".to_string(),
    ];
    labels.insert(0, gettext("None"));
    labels
}

/// Combo index for a stored browser value. Unknown values fall back to
/// None rather than selecting a browser the user didn't pick.
pub fn cookies_browser_index(value: &str) -> usize {
    COOKIES_BROWSERS
        .iter()
        .position(|v| *v == value)
        .unwrap_or(0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to off.
pub fn cookies_browser_value(index: usize) -> &'static str {
    COOKIES_BROWSERS.get(index).copied().unwrap_or("none")
}

/// Real home directory from the passwd database, bypassing any sandbox
/// `$HOME` remapping (inside Flatpak `$HOME` is the app sandbox dir, not
/// the user's home). `None` on non-Unix or lookup failure.
#[cfg(unix)]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer to static storage (or null); we
    // only read pw_dir up to its NUL terminator on the calling thread.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        let dir = (*pw).pw_dir;
        if dir.is_null() {
            return None;
        }
        let len = libc::strlen(dir);
        if len == 0 {
            return None;
        }
        let bytes = std::slice::from_raw_parts(dir as *const u8, len);
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

#[cfg(not(unix))]
pub(crate) fn real_home_dir() -> Option<PathBuf> {
    None
}

/// Real host config directory, even inside a Flatpak sandbox where
/// `$HOME`/`$XDG_CONFIG_HOME` point at the app's own sandbox dirs.
/// Priority: `HOST_XDG_CONFIG_HOME` (Flatpak exposes the host value),
/// then passwd-database home + `.config`, then the normal XDG fallback.
pub(crate) fn real_config_home() -> PathBuf {
    if let Some(host) = std::env::var_os("HOST_XDG_CONFIG_HOME") {
        let p = PathBuf::from(&host);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(home) = real_home_dir() {
        return home.join(".config");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/etc/xdg"))
}

/// Profile directories holding a Chromium `Cookies` database, best first:
/// `Default`, then a top-level `Cookies` file, then `Profile *`.
fn chromium_profile_dirs(config: &Path, subdir: &str) -> Vec<PathBuf> {
    let base = config.join(subdir);
    let mut out = vec![base.join("Default"), base.clone()];
    if let Ok(entries) = std::fs::read_dir(&base) {
        let mut rest: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("Profile "))
            })
            .collect();
        rest.sort();
        out.extend(rest);
    }
    out.into_iter()
        .filter(|p| p.join("Cookies").is_file())
        .collect()
}

/// Firefox profile directories under one base dir, default first. Parses
/// every `[Profile*]` section of `profiles.ini` (`Path` + `IsRelative`
/// + `Default`); falls back to a directory scan when there is no ini.
fn firefox_profile_dirs(base: &Path) -> Vec<PathBuf> {
    if let Ok(text) = std::fs::read_to_string(base.join("profiles.ini")) {
        let mut ranked: Vec<(PathBuf, bool)> = Vec::new();
        let mut path: Option<String> = None;
        let mut relative = true;
        let mut is_default = false;
        let mut flush = |path: &mut Option<String>, relative: &mut bool, is_default: &mut bool| {
            if let Some(p) = path.take() {
                let dir = if *relative && !std::path::Path::new(&p).is_absolute() {
                    base.join(&p)
                } else {
                    PathBuf::from(&p)
                };
                ranked.push((dir, *is_default));
            }
            *relative = true;
            *is_default = false;
        };
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                flush(&mut path, &mut relative, &mut is_default);
            } else if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "Path" => path = Some(value.trim().to_string()),
                    "IsRelative" => relative = value.trim() != "0",
                    "Default" => is_default = value.trim() == "1",
                    _ => {}
                }
            }
        }
        flush(&mut path, &mut relative, &mut is_default);
        ranked.sort_by_key(|(_, d)| !d);
        return ranked
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| p.join("cookies.sqlite").is_file())
            .collect();
    }
    if let Ok(entries) = std::fs::read_dir(base) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".default-release") || n.ends_with(".default"))
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| base.join(n))
            .filter(|p| p.join("cookies.sqlite").is_file())
            .collect()
    } else {
        Vec::new()
    }
}

/// Chromium config subdirs per browser, most common first (sandbox grants
/// cover the primaries; alternates still resolve outside Flatpak).
fn chromium_subdirs(browser: &str) -> &'static [&'static str] {
    match browser {
        "brave" => &["BraveSoftware/Brave-Browser"],
        "chrome" => &[
            "google-chrome",
            "google-chrome-beta",
            "google-chrome-unstable",
        ],
        "chromium" => &["chromium", "chromium-beta"],
        "edge" => &[
            "microsoft-edge",
            "microsoft-edge-beta",
            "microsoft-edge-dev",
        ],
        "opera" => &["opera", "opera-beta"],
        "vivaldi" => &["vivaldi", "vivaldi-snapshot"],
        "whale" => &["naver-whale"],
        _ => &[],
    }
}

/// Absolute browser profile directory for `--cookies-from-browser`, resolved
/// against the real host config/home dirs (see [`real_config_home`]) so it
/// works inside the Flatpak sandbox where `$HOME` is remapped. Testable core:
/// `config_home` stands in for [`real_config_home`], `home` for the passwd
/// home (snap Firefox lives under it, not under the config dir).
pub(crate) fn browser_profile_dir_in(
    config_home: &Path,
    home: &Path,
    browser: &str,
) -> Option<PathBuf> {
    if browser == "firefox" {
        return [
            home.join(".mozilla/firefox"),
            config_home.join("mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
        ]
        .into_iter()
        .find_map(|base| firefox_profile_dirs(&base).into_iter().next());
    }
    chromium_subdirs(browser)
        .iter()
        .find_map(|sub| chromium_profile_dirs(config_home, sub).into_iter().next())
}

/// [`browser_profile_dir_in`] against the real host directories. When
/// `HOST_XDG_CONFIG_HOME` is set (Flatpak, and the unit tests), the home
/// dir is its parent — `<home>/.config` — so both roots stay consistent
/// without touching the sandbox `$HOME`.
pub(crate) fn browser_profile_dir(browser: &str) -> Option<PathBuf> {
    let config = real_config_home();
    let home = std::env::var_os("HOST_XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .and_then(|p| p.parent().map(PathBuf::from))
        .or_else(real_home_dir)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    browser_profile_dir_in(&config, &home, browser)
}

/// Spec for `--cookies-from-browser`: `browser:/absolute/profile/dir` when
/// the profile resolves on disk, else the bare browser name so yt-dlp falls
/// back to its own `$HOME`-relative lookup (correct outside Flatpak).
/// `None`/unknown means off. The profile path must be the profile
/// *directory* — yt-dlp opens and decrypts the cookie database itself
/// (keyring included); a raw `Cookies` file is not a `--cookies` export.
pub(crate) fn cookies_browser_spec(value: &str) -> Option<String> {
    if value.is_empty() || value == "none" || !COOKIES_BROWSERS.contains(&value) {
        return None;
    }
    if let Some(dir) = browser_profile_dir(value) {
        return Some(format!("{value}:{}", dir.display()));
    }
    Some(value.to_string())
}

/// Shared root for extraction scratch space.
pub fn staging_root() -> PathBuf {
    std::env::temp_dir().join("grab-video")
}

/// Per-item staging dir holding the split `.video`/`.audio` parts and the
/// muxed result while a merged video download is in flight.
pub fn staging_dir(item_id: u64) -> PathBuf {
    staging_root().join(item_id.to_string())
}

/// Remove a staging dir. Guarded: never deletes anything outside Grab's own
/// staging root, so a buggy caller can't nuke user data.
pub fn clean_staging(dir: &Path) {
    if dir.starts_with(staging_root()) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// How long one metadata extraction may take before it counts as failed.
/// Without a ceiling a throttled host parks the dialog on its spinner
/// forever; the error path (with Retry) is strictly more useful.
const FETCH_TIMEOUT_SECS: u64 = 60;

/// Integer-valued fields the model demands as i64/u64 but extractors
/// sometimes emit as floats (Instagram reels report fractional
/// durations). Truncation matches the old display semantics.
const INT_FIELDS: &[&str] = &[
    "duration",
    "timestamp",
    "release_timestamp",
    "release_year",
    "view_count",
    "like_count",
    "comment_count",
    "channel_follower_count",
    "age_limit",
    "available_at",
    "filesize",
    "filesize_approx",
    "language_preference",
    "source_preference",
    "asr",
    "width",
    "height",
];

/// Truncate float values to integers for [`INT_FIELDS`] in one JSON
/// object. Anything else (strings, bools, nulls, objects) is untouched.
fn coerce_int_fields(obj: &mut serde_json::Map<String, serde_json::Value>) {
    for key in INT_FIELDS {
        if let Some(v) = obj.get_mut(*key)
            && let Some(f) = v.as_f64()
        {
            *v = serde_json::json!(f.trunc() as i64);
        }
    }
}

/// Fill in fields the bundled yt-dlp binary omits but the crate's model
/// demands. Without this, one sparse object (a thumbnail without
/// `preference`, an x.com page without `live_status`, an Instagram reel
/// with a fractional duration) fails the entire preview parse. Arrays
/// Grab never reads are dropped instead of repaired; formats (which the
/// pipeline does read) get neutral defaults for their required scalars.
fn sanitize_video_json(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    coerce_int_fields(obj);
    obj.entry("id").or_insert(serde_json::json!(""));
    obj.entry("title").or_insert(serde_json::json!(""));
    obj.entry("age_limit").or_insert(serde_json::json!(0));
    obj.entry("live_status").or_insert(serde_json::json!(""));
    obj.entry("playable_in_embed")
        .or_insert(serde_json::json!(false));
    obj.entry("extractor").or_insert(serde_json::json!(""));
    obj.entry("extractor_key").or_insert(serde_json::json!(""));
    if !obj.get("_version").is_some_and(|v| v.is_object()) {
        obj.insert(
            "_version".to_string(),
            serde_json::json!({"version": "", "repository": ""}),
        );
    } else if let Some(ver) = obj.get_mut("_version").and_then(|v| v.as_object_mut()) {
        // Present-but-sparse version blocks fail the parse the same way.
        ver.entry("version").or_insert(serde_json::json!(""));
        ver.entry("repository").or_insert(serde_json::json!(""));
    }
    // downloader_options is never read: a sparse object (just
    // http_chunk_size today) would fail the parse for nothing.
    obj.remove("downloader_options");
    // Unread arrays/objects: one sparse entry must not fail the video,
    // so they are always reset — not just when absent. Some extractors
    // (TikTok) also emit explicit nulls, which serde defaults don't
    // cover (those only fill in missing keys). Formats are read by the
    // pipeline, so they are repaired entry by entry below instead.
    for key in ["thumbnails", "chapters", "tags", "categories"] {
        obj.insert(key.to_string(), serde_json::json!([]));
    }
    for key in ["subtitles", "automatic_captions"] {
        obj.insert(key.to_string(), serde_json::json!({}));
    }
    obj.insert("heatmap".to_string(), serde_json::Value::Null);
    if !obj.get("formats").is_some_and(|v| v.is_array()) {
        obj.insert("formats".to_string(), serde_json::json!([]));
    }
    if let Some(formats) = obj.get_mut("formats").and_then(|f| f.as_array_mut()) {
        formats.retain(|f| f.is_object());
        for format in formats.iter_mut() {
            if let Some(entry) = format.as_object_mut() {
                coerce_int_fields(entry);
                entry.entry("format").or_insert(serde_json::json!(""));
                entry.entry("format_id").or_insert(serde_json::json!(""));
                entry.entry("http_headers").or_insert(serde_json::json!({}));
                if entry.contains_key("fragments") {
                    entry.insert("fragments".to_string(), serde_json::json!([]));
                }
            }
        }
    }
}

/// Fetch one page's `--dump-single-json` through the crate's [`Executor`]
/// (same spawn/timeout/output semantics as its extractors) with exactly
/// the arguments its default extractors use, then parse leniently (see
/// [`sanitize_video_json`]). Used instead of the crate's
/// `fetch_video_infos`, whose strict model breaks whenever the binary's
/// JSON gains or drops a field.
async fn fetch_video_page(
    youtube_bin: &Path,
    url: &str,
    cookies_browser: &str,
    timeout: Duration,
) -> Result<Video, VideoError> {
    let mut args = vec![
        "--no-progress".to_string(),
        "--dump-single-json".to_string(),
    ];
    if let Some(spec) = cookies_browser_spec(cookies_browser) {
        args.push(format!("--cookies-from-browser={spec}"));
    }
    args.push(url.to_string());
    let executor = yt_dlp::executor::Executor::new(youtube_bin, args, timeout);
    let output = executor.execute().await.map_err(VideoError::fetch)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&output.stdout).map_err(VideoError::fetch)?;
    sanitize_video_json(&mut value);
    let mut video: Video = serde_json::from_value(value).map_err(VideoError::fetch)?;
    for format in &mut video.formats {
        format.video_id = Some(video.id.clone());
    }
    Ok(video)
}

/// Extract metadata for one video page. The media URLs inside the returned
/// [`VideoInfo`] are only passed on to the download step; the *page URL* is
/// what survives restarts.
pub async fn fetch_video_infos(
    libs: Libraries,
    url: String,
    cookies_browser: String,
) -> Result<VideoInfo, VideoError> {
    let handle = crate::download::tokio_rt().spawn(async move {
        let (yt_version, _ff_version) = ensure_tool_versions(&libs).await?;
        tracing::info!(yt_dlp = %yt_version, url_host = %page_host(&url), "resolving video page");
        let out = staging_root();
        std::fs::create_dir_all(&out).map_err(VideoError::staging)?;
        let video = match tokio::time::timeout(
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            fetch_video_page(
                &libs.youtube,
                &url,
                &cookies_browser,
                Duration::from_secs(300),
            ),
        )
        .await
        {
            Ok(Ok(video)) => video,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(VideoError::fetch(gettext("the lookup timed out")));
            }
        };
        Ok::<_, VideoError>(VideoInfo::from(&video, &url))
    });
    match handle.await {
        Ok(r) => r,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// One video-only format, deduplicated and labeled for the dialog combo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormatOption {
    pub id: String,
    pub label: String,
    pub height: u32,
}

/// Human size for format labels. Decimal units, one fraction digit.
fn fmt_video_bytes(n: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let f = n as f64;
    if f >= GB {
        format!("{:.1} GB", f / GB)
    } else if f >= MB {
        format!("{:.1} MB", f / MB)
    } else {
        format!("{n} B")
    }
}

/// Listable video-only formats for one video: best per height, tallest
/// first. Only directly fetchable streams qualify (plain HTTPS, no DRM);
/// HLS variants fill heights with no direct stream (the worker pulls
/// those via ffmpeg); muxed files stay on the automatic path, which
/// already adopts them. Audio-only formats never appear here.
pub fn video_format_options(video: &Video) -> Vec<VideoFormatOption> {
    use std::collections::HashMap;
    let mut best: HashMap<u32, &Format> = HashMap::new();
    for f in &video.formats {
        if f.protocol != Protocol::Https {
            continue;
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            continue;
        }
        let vcodec = f.codec_info.video_codec.as_deref().unwrap_or("none");
        if vcodec == "none" {
            continue;
        }
        let acodec = f.codec_info.audio_codec.as_deref().unwrap_or("none");
        if acodec != "none" {
            continue;
        }
        let Some(h) = f.video_resolution.height.filter(|&h| h > 0) else {
            continue;
        };
        let replace = match best.get(&h) {
            None => true,
            Some(cur) => {
                let cur_rank = codec_rank(cur.codec_info.video_codec.as_deref().unwrap_or("none"));
                let new_rank = codec_rank(vcodec);
                (new_rank < cur_rank)
                    || (new_rank == cur_rank
                        && filesize_of(f).unwrap_or(0) > filesize_of(cur).unwrap_or(0))
            }
        };
        if replace {
            best.insert(h, f);
        }
    }
    // HLS gap-fill: heights with no direct stream still list, so the
    // dialog can pin them and the worker routes them to ffmpeg.
    for f in &video.formats {
        if HlsSel::from_format(f).is_none() {
            continue;
        }
        let Some(h) = f.video_resolution.height.filter(|&h| h > 0) else {
            continue;
        };
        best.entry(h).or_insert(f);
    }
    let mut out: Vec<VideoFormatOption> = best
        .into_iter()
        .map(|(height, f)| {
            // HLS variants show their transport, not a codec that
            // ffmpeg — not the engine — will consume.
            let short = if f.protocol == Protocol::M3U8Native {
                "HLS".to_string()
            } else {
                f.codec_info
                    .video_codec
                    .as_deref()
                    .unwrap_or("?")
                    .split('.')
                    .next()
                    .unwrap_or("?")
                    .to_string()
            };
            let label = match filesize_of(f) {
                Some(n) => format!("{height}p · {short} · {}", fmt_video_bytes(n)),
                None => format!("{height}p · {short}"),
            };
            VideoFormatOption {
                id: f.format_id.clone(),
                label,
                height,
            }
        })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.height));
    out
}

/// Newest-first codec rank, mirroring yt-dlp's `+vcodec:av01` sort:
/// AV1 wins ties at the same height, then VP9, HEVC, AVC1, anything
/// else. Older codecs are only dropped in favor of newer ones — never
/// at the cost of resolution, and never into an empty list.
fn codec_rank(vcodec: &str) -> u8 {
    let c = vcodec.to_ascii_lowercase();
    if c.starts_with("av01") || c.starts_with("av1") {
        0
    } else if c.starts_with("vp9") {
        1
    } else if c.starts_with("hev1") || c.starts_with("hvc1") || c.starts_with("h265") {
        2
    } else if c.starts_with("avc1") || c.starts_with("h264") {
        3
    } else {
        4
    }
}
/// One HLS manifest variant for the ffmpeg fallback path: owned
/// values, like [`StreamSel`]. ffmpeg resolves the variant playlist
/// (and segment keys) itself.
#[derive(Debug, Clone)]
struct HlsSel {
    url: String,
    headers: HttpHeaders,
    height: Option<u32>,
    /// Separate audio rendition URL when the page lists audio-only HLS
    /// formats: used when the variant's own master names no audio.
    /// Set after selection; [`HlsSel::from_format`] leaves it empty.
    fallback_audio: Option<String>,
}

impl HlsSel {
    /// Build from an extractor format: manifest protocol, DRM-free,
    /// with a playlist URL.
    fn from_format(f: &Format) -> Option<Self> {
        if f.protocol != Protocol::M3U8Native {
            return None;
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            return None;
        }
        Some(Self {
            url: f.download_info.url.clone().filter(|u| !u.is_empty())?,
            headers: f.download_info.http_headers.clone(),
            height: f.video_resolution.height.filter(|&h| h > 0),
            fallback_audio: None,
        })
    }
}

/// Stored quality value to a height cap: `None` (Best) takes the
/// tallest variant available.
fn quality_height(value: &str) -> Option<u32> {
    match value {
        "best" => None,
        "2160p" => Some(2160),
        "1440p" => Some(1440),
        "1080p" => Some(1080),
        "720p" => Some(720),
        "480p" => Some(480),
        _ => Some(1080),
    }
}

/// Best HLS variant for a height cap: smallest height at or above the
/// cap, else the tallest available. Mirrors the crate's
/// closest-at-or-above preset semantics.
fn select_hls_format(formats: &[Format], want: Option<u32>) -> Option<HlsSel> {
    let cands: Vec<HlsSel> = formats.iter().filter_map(HlsSel::from_format).collect();
    match want {
        Some(h) => cands
            .iter()
            .find(|s| s.height.is_some_and(|x| x >= h))
            .or_else(|| cands.iter().max_by_key(|s| s.height.unwrap_or(0)))
            .cloned(),
        None => cands.into_iter().max_by_key(|s| s.height.unwrap_or(0)),
    }
}

/// Find one HLS variant by dialog-pinned id.
fn find_hls_format(formats: &[Format], id: &str) -> Option<HlsSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(HlsSel::from_format)
}

/// URL of an audio-only HLS format, when the page lists HLS audio
/// beside (not inside) its video variants: used when the picked
/// variant's own master names no audio rendition. Highest bitrate
/// wins; subtitles masquerading as audio-less video never qualify.
fn select_hls_audio_url(formats: &[Format]) -> Option<String> {
    formats
        .iter()
        .filter(|f| {
            f.protocol == Protocol::M3U8Native
                && !matches!(f.has_drm, Some(DrmStatus::Yes))
                && f.codec_info
                    .video_codec
                    .as_deref()
                    .is_none_or(|c| c == "none")
                && f.codec_info
                    .audio_codec
                    .as_deref()
                    .is_some_and(|c| c != "none")
        })
        .filter_map(|f| {
            f.download_info.url.clone().and_then(|url| {
                if url.is_empty() {
                    return None;
                }
                let bitrate = f.rates_info.total_rate.map(|r| r.into_inner() as i64);
                Some((bitrate.unwrap_or(0), url))
            })
        })
        .max_by_key(|(bitrate, _)| *bitrate)
        .map(|(_, url)| url)
}

/// `-headers` value for ffmpeg: the extractor-resolved headers
/// (variant and segment requests often need the same auth), with the
/// configured user agent winning over the extractor's.
fn ffmpeg_headers(headers: &HttpHeaders, user_agent: &str) -> String {
    let ua = if user_agent.trim().is_empty() {
        headers.user_agent.as_str()
    } else {
        user_agent.trim()
    };
    let mut out = String::new();
    for (name, value) in [
        ("User-Agent", ua),
        ("Accept", headers.accept.as_str()),
        ("Accept-Language", headers.accept_language.as_str()),
        ("Sec-Fetch-Mode", headers.sec_fetch_mode.as_str()),
    ] {
        let value = value.trim();
        // Reject embedded newlines like the reqwest side does
        // (HeaderValue::from_str): extractor JSON controls these
        // values, and a CR/LF would inject headers into ffmpeg.
        if value.is_empty() || value.bytes().any(|b| b == b'\r' || b == b'\n') {
            continue;
        }
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out
}

/// Total bytes written from one ffmpeg `-progress pipe:1` line
/// (`total_size=N`). The pump renders byte amounts with unknown total,
/// the same shape as length-less engine rows.
fn parse_progress_size(line: &str) -> Option<u64> {
    let (key, raw) = line.split_once('=')?;
    if key.trim() != "total_size" {
        return None;
    }
    raw.trim().parse().ok()
}

/// One variant (`EXT-X-STREAM-INF`) of an HLS master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsVariant {
    uri: String,
    height: Option<u32>,
    audio_group: Option<String>,
    /// Raw CODECS attribute: variants naming both an audio and a video
    /// codec carry muxed segments, so no separate audio is needed.
    codecs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One audio rendition (`EXT-X-MEDIA TYPE=AUDIO`) of a master playlist.
struct HlsAudio {
    group: String,
    uri: Option<String>,
    default: bool,
}

/// Split an HLS tag attribute list on commas outside quotes
/// (`BANDWIDTH=123,RESOLUTION=640x360,AUDIO="a"`).
fn split_hls_attrs(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(line[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(line[start..].trim());
    parts
}

/// Parse one `KEY=VALUE` attribute pair, unquoting the value.
fn hls_attr(part: &str) -> Option<(&str, &str)> {
    let (key, value) = part.split_once('=')?;
    Some((key.trim(), value.trim().trim_matches('"')))
}

/// Height from a `RESOLUTION=WxH` value.
fn hls_resolution_height(value: &str) -> Option<u32> {
    value.split_once('x')?.1.parse().ok()
}

/// Whether a variant's CODECS list names an audio codec alongside
/// video: such segments are muxed, so the variant needs no rendition.
fn hls_codecs_have_audio(codecs: &str) -> bool {
    let c = codecs.to_ascii_lowercase();
    ["mp4a", "ac-3", "ec-3", "opus", "vorbis", "flac", "alac"]
        .iter()
        .any(|a| c.contains(a))
}

/// Join a playlist-relative URI against its base, carrying the base
/// query string over when the reference has none (tokenized masters
/// like Twitter's authenticate every URL with the same query — the
/// same default yt-dlp applies via `variant_query`).
fn join_hls_url(base: &url::Url, reference: &str) -> Option<url::Url> {
    let mut url = base.join(reference).ok()?;
    if url.query().is_none()
        && let Some(query) = base.query()
    {
        url.set_query(Some(query));
    }
    Some(url)
}

/// Parse an HLS master playlist into variants + audio renditions,
/// resolving relative URIs against the playlist URL. Returns `None`
/// when the text is a media playlist (segments, no variants), which
/// the caller downloads directly.
fn parse_hls_master(text: &str, base: &url::Url) -> Option<(Vec<HlsVariant>, Vec<HlsAudio>)> {
    let mut variants = Vec::new();
    let mut audios = Vec::new();
    let mut pending: Option<HlsVariant> = None;
    let mut is_master = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            is_master = true;
            let mut variant = HlsVariant {
                uri: String::new(),
                height: None,
                audio_group: None,
                codecs: None,
            };
            for part in split_hls_attrs(rest) {
                match hls_attr(part) {
                    Some(("RESOLUTION", v)) => variant.height = hls_resolution_height(v),
                    Some(("AUDIO", v)) => variant.audio_group = Some(v.to_string()),
                    Some(("CODECS", v)) => variant.codecs = Some(v.to_string()),
                    _ => {}
                }
            }
            pending = Some(variant);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let (mut kind, mut group, mut uri, mut default) = ("", "", None, false);
            for part in split_hls_attrs(rest) {
                match hls_attr(part) {
                    Some(("TYPE", v)) => kind = v,
                    Some(("GROUP-ID", v)) => group = v,
                    Some(("URI", v)) => uri = Some(v.to_string()),
                    Some(("DEFAULT", "YES")) => default = true,
                    _ => {}
                }
            }
            if kind == "AUDIO" && !group.is_empty() {
                let uri = uri
                    .and_then(|u| join_hls_url(base, &u))
                    .map(|u| u.to_string());
                audios.push(HlsAudio {
                    group: group.to_string(),
                    uri,
                    default,
                });
            }
        } else if !line.starts_with('#')
            && let Some(mut variant) = pending.take()
            && let Some(uri) = join_hls_url(base, line)
        {
            variant.uri = uri.to_string();
            variants.push(variant);
        }
    }
    if !is_master {
        return None;
    }
    Some((variants, audios))
}

/// Concrete ffmpeg inputs for one HLS capture: the video playlist
/// plus, when the variant carries separate audio, its rendition URL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HlsInput {
    video: String,
    audio: Option<String>,
}

/// Pick the variant for a height cap (smallest at or above, else
/// tallest — the same rule as [`select_hls_format`]), preferring
/// variants that actually carry audio, and attach their rendition.
fn pick_hls_variant(
    variants: &[HlsVariant],
    audios: &[HlsAudio],
    want: Option<u32>,
) -> Option<HlsInput> {
    let mut cands: Vec<&HlsVariant> = variants.iter().filter(|v| !v.uri.is_empty()).collect();
    if cands.is_empty() {
        return None;
    }
    // Sounding variants first, then shortest: a video downloads with
    // audio, so certainty beats an exact height-cap fit. Proven audio
    // (CODECS names it, or the group is defined — a group without URI
    // is muxed in the variant) outranks assumed-muxed (no AUDIO tag),
    // which outranks dangling group references.
    let audio_score = |v: &HlsVariant| {
        if v.codecs.as_deref().is_some_and(hls_codecs_have_audio) {
            0
        } else {
            match &v.audio_group {
                None => 1,
                Some(group) if audios.iter().any(|a| &a.group == group) => 0,
                Some(_) => 2,
            }
        }
    };
    cands.sort_by_key(|v| (audio_score(v), v.height.unwrap_or(0)));
    let pick = match want {
        // Sounding variants first even against the cap: a shorter
        // variant with audio beats silent height-fit.
        Some(h) => cands
            .iter()
            .filter(|v| audio_score(v) < 2)
            .find(|v| v.height.is_some_and(|x| x >= h))
            .or_else(|| cands.iter().find(|v| v.height.is_some_and(|x| x >= h)))
            .or_else(|| cands.iter().max_by_key(|v| v.height.unwrap_or(0)))
            .cloned(),
        None => cands
            .iter()
            .filter(|v| audio_score(v) < 2)
            .max_by_key(|v| v.height.unwrap_or(0))
            .or_else(|| cands.iter().max_by_key(|v| v.height.unwrap_or(0)))
            .cloned(),
    }?;
    // DEFAULT-marked rendition first, like a player would auto-select.
    let audio = pick.audio_group.as_deref().and_then(|group| {
        audios
            .iter()
            .filter(|a| a.group == group && a.uri.is_some())
            .min_by_key(|a| !a.default)
            .and_then(|a| a.uri.clone())
    });
    Some(HlsInput {
        video: pick.uri.clone(),
        audio,
    })
}

/// Request headers for playlist fetches: extractor-resolved headers
/// with the configured user agent winning. Empty values are skipped.
fn hls_request_headers(headers: &HttpHeaders, user_agent: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let ua = if user_agent.trim().is_empty() {
        headers.user_agent.as_str()
    } else {
        user_agent.trim()
    };
    let mut map = HeaderMap::new();
    for (name, value) in [
        ("user-agent", ua),
        ("accept", headers.accept.as_str()),
        ("accept-language", headers.accept_language.as_str()),
        ("sec-fetch-mode", headers.sec_fetch_mode.as_str()),
    ] {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// Resolve what ffmpeg should open for one HLS format URL: when the URL
/// is a master playlist, pick the height-appropriate variant (plus its
/// audio rendition, if separate); otherwise — media playlist, fetch
/// failure, anything unexpected — hand ffmpeg the URL untouched, which
/// is exactly today's behavior.
/// Fetch one playlist as text: `None` on any network, status or
/// body failure. Callers fall back to handing ffmpeg the URL blind.
async fn fetch_playlist_text(
    client: &reqwest::Client,
    url: &str,
    headers: &HttpHeaders,
    user_agent: &str,
) -> Option<String> {
    match client
        .get(url)
        .headers(hls_request_headers(headers, user_agent))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
        _ => None,
    }
}

async fn resolve_hls_input(
    url: &str,
    headers: &HttpHeaders,
    user_agent: &str,
    want: Option<u32>,
) -> HlsInput {
    let fallback = || HlsInput {
        video: url.to_string(),
        audio: None,
    };
    let Ok(base) = url::Url::parse(url) else {
        return fallback();
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok();
    let Some(client) = client else {
        return fallback();
    };
    let Some(text) = fetch_playlist_text(&client, url, headers, user_agent).await else {
        return fallback();
    };
    if !text.contains("#EXT-X-STREAM-INF") {
        return fallback();
    }
    let Some((variants, audios)) = parse_hls_master(&text, &base) else {
        return fallback();
    };
    pick_hls_variant(&variants, &audios, want).unwrap_or_else(fallback)
}

/// Find one format by id, accepting only what the pipeline can fetch.
/// `None` covers unknown ids and HLS/DRM/missing-URL formats alike: the
/// caller falls back to the quality preset.
fn find_usable_format(formats: &[Format], id: &str) -> Option<StreamSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(|f| StreamSel::from_format(f).ok())
}

/// Map a stored quality value to the extractor selector. Unknown values
/// fall back to 1080p (same fallback as the combo mapping).
pub fn selector_for_quality(value: &str) -> VideoQuality {
    match value {
        "best" => VideoQuality::Best,
        "2160p" => VideoQuality::CustomHeight(2160),
        "1440p" => VideoQuality::CustomHeight(1440),
        "1080p" => VideoQuality::CustomHeight(1080),
        "720p" => VideoQuality::CustomHeight(720),
        "480p" => VideoQuality::CustomHeight(480),
        _ => VideoQuality::CustomHeight(1080),
    }
}

/// One stream selected for download: owned values so the pipeline holds no
/// borrow on the extractor metadata across awaits.
#[derive(Debug, Clone)]
struct StreamSel {
    format_id: String,
    ext: String,
    url: String,
    headers: HttpHeaders,
    size: Option<u64>,
    /// Whether the stream carries an audio track (muxed files do).
    has_audio: bool,
}

impl StreamSel {
    /// Build from an extractor format, accepting only what the pipeline
    /// can actually fetch: plain-HTTPS, DRM-free streams with a URL.
    /// Anything else (HLS manifests, encrypted formats) is a clean
    /// unavailable error — never playlist bytes merged as media, never
    /// encrypted garbage saved as a finished file.
    fn from_format(f: &Format) -> Result<Self, VideoError> {
        if f.protocol != Protocol::Https {
            return Err(VideoError::unavailable());
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            return Err(VideoError::unavailable());
        }
        Ok(Self {
            format_id: f.format_id.clone(),
            ext: f.download_info.ext.as_str().to_string(),
            url: f
                .download_info
                .url
                .clone()
                .ok_or_else(VideoError::unavailable)?,
            headers: f.download_info.http_headers.clone(),
            size: filesize_of(f),
            has_audio: f
                .codec_info
                .audio_codec
                .as_deref()
                .is_some_and(|c| c != "none"),
        })
    }
}

fn filesize_of(f: &Format) -> Option<u64> {
    f.file_info
        .filesize
        .or(f.file_info.filesize_approx)
        .filter(|&n| n > 0)
        .map(|n| n as u64)
}

/// Sidecar recording completed parts, so a retry (or a relaunch after a
/// crash) can skip straight to the merge instead of re-downloading.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct VideoManifest {
    page_url: String,
    quality: String,
    audio_only: bool,
    video_format_id: Option<String>,
    video_ext: String,
    video_bytes: u64,
    audio_format_id: String,
    audio_ext: String,
    audio_bytes: u64,
    /// Final output size once the muxed file has been renamed into place.
    /// Lets a retry after a crash adopt the finished file without any work.
    final_bytes: Option<u64>,
}

impl VideoManifest {
    /// Whether the sidecar describes this exact attempt (same page, prefs,
    /// and selected formats). Anything else means the formats shifted
    /// under us and the parts must be re-downloaded.
    fn matches(
        &self,
        page_url: &str,
        quality: &str,
        audio_only: bool,
        video: Option<(&str, &str)>,
        audio: (&str, &str),
    ) -> bool {
        self.page_url == page_url
            && self.quality == quality
            && self.audio_only == audio_only
            && self.audio_format_id == audio.0
            && self.audio_ext == audio.1
            && match (video, &self.video_format_id) {
                (Some((id, ext)), Some(mid)) => id == mid && ext == self.video_ext,
                (None, None) => self.video_ext.is_empty(),
                _ => false,
            }
    }

    /// Whether the recorded part files are all present with exactly the
    /// recorded sizes. Equality (not >=) so a truncated or replaced part
    /// forces a re-download instead of a corrupt merge. Zero-byte records
    /// never count (a pending manifest carries zeros), and neither do
    /// sparse shells (pre-allocated zeros from a killed attempt).
    fn parts_present(&self, dir: &Path) -> bool {
        let audio_part = part_path(dir, "audio", &self.audio_ext);
        let audio_ok = self.audio_bytes > 0
            && file_len(&audio_part) == Some(self.audio_bytes)
            && !is_sparse_shell(&audio_part);
        let video_ok = match &self.video_format_id {
            Some(_) => {
                let video_part = part_path(dir, "video", &self.video_ext);
                self.video_bytes > 0
                    && file_len(&video_part) == Some(self.video_bytes)
                    && !is_sparse_shell(&video_part)
            }
            None => true,
        };
        audio_ok && video_ok
    }
}

/// Fixed part names inside a row's staging dir: retry-after-crash finds the
/// same paths the failed attempt wrote.
pub(crate) fn part_path(dir: &Path, kind: &str, ext: &str) -> PathBuf {
    dir.join(format!("{kind}.{ext}"))
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

pub(crate) fn read_manifest(dir: &Path) -> Option<VideoManifest> {
    std::fs::read_to_string(manifest_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

async fn write_manifest(dir: &Path, manifest: &VideoManifest) -> Result<(), VideoError> {
    let text = serde_json::to_string_pretty(manifest).map_err(VideoError::staging)?;
    tokio::fs::write(manifest_path(dir), text)
        .await
        .map_err(VideoError::staging)
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

/// Allocated bytes on disk, for sparse-shell detection. `None` where the
/// platform cannot say (non-Unix): callers treat that as dense, i.e. the
/// pre-existing behavior.
#[cfg(unix)]
fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).map(|m| m.blocks() * 512).ok()
}

#[cfg(not(unix))]
fn allocated_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether a part file is a sparse shell: full apparent size, (almost)
/// nothing on disk. The download engine pre-allocates part files and
/// tracks real progress in a sidecar it deletes on abort, so a killed
/// attempt leaves exactly this shape behind — and a naive size check
/// would then "adopt" gigabytes of zeros.
fn is_sparse_shell(path: &Path) -> bool {
    match (file_len(path), allocated_bytes(path)) {
        (Some(len), Some(allocated)) => len > 0 && allocated < len,
        _ => false,
    }
}

/// What the next attempt should do, decided from the sidecar and disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Finished file already in place (adopt it).
    Finished,
    /// Parts verified on disk (merge only).
    CombineOnly,
    /// Parts started but incomplete: proceed WITHOUT wiping so the
    /// download engine resumes them in place (Range-append for simple
    /// streams, segment skip via the `.parts` sidecar). Safe because the
    /// manifest matched: same formats, same bytes.
    Resume,
    /// (Re)download everything, wiping staging first.
    Fresh,
}

/// Inputs for [`resume_plan`], bundled so the signature stays small.
pub(crate) struct ResumeQuery<'a> {
    pub manifest: Option<&'a VideoManifest>,
    pub dir: &'a Path,
    pub dest: &'a Path,
    pub page_url: &'a str,
    pub quality: &'a str,
    pub audio_only: bool,
    pub video: Option<(&'a str, &'a str)>,
    pub audio: (&'a str, &'a str),
    /// Freshly selected total sizes, for the over-long check. `None`
    /// means unknown: without a total, oversize is undetectable and any
    /// existing bytes are reusable.
    pub video_total: Option<u64>,
    pub audio_total: Option<u64>,
}

pub(crate) fn resume_plan(q: &ResumeQuery) -> ResumePlan {
    let Some(m) = q.manifest else {
        return ResumePlan::Fresh;
    };
    if !m.matches(q.page_url, q.quality, q.audio_only, q.video, q.audio) {
        return ResumePlan::Fresh;
    }
    if let Some(final_bytes) = m.final_bytes
        && file_len(q.dest) == Some(final_bytes)
    {
        return ResumePlan::Finished;
    }
    if m.parts_present(q.dir) {
        return ResumePlan::CombineOnly;
    }
    // Sparse shells (full size, nothing on disk) left by a killed attempt
    // must not reach the engine: it equates size with completeness and
    // would "adopt" zeros. Over-long parts cannot be resumed into either
    // (nothing valid past the total). Either way, wipe and start over.
    // Anything else with bytes on disk is resumable — the manifest match
    // above is the identity check.
    let (_, audio_ext) = q.audio;
    let audio_part = part_path(q.dir, "audio", audio_ext);
    let video_part = q
        .video
        .map(|(_, video_ext)| part_path(q.dir, "video", video_ext));
    if is_sparse_shell(&audio_part) || video_part.as_ref().is_some_and(|p| is_sparse_shell(p)) {
        return ResumePlan::Fresh;
    }
    let overlong = |path: &Path, total: Option<u64>| {
        total.is_some_and(|t| file_len(path).is_some_and(|n| n > t))
    };
    if overlong(&audio_part, q.audio_total) {
        return ResumePlan::Fresh;
    }
    if let Some(video_part) = &video_part
        && overlong(video_part, q.video_total)
    {
        return ResumePlan::Fresh;
    }
    let video_has = video_part
        .as_ref()
        .and_then(|p| file_len(p.as_path()))
        .is_some_and(|n| n > 0);
    let audio_has = file_len(&audio_part).is_some_and(|n| n > 0);
    if video_has || audio_has {
        ResumePlan::Resume
    } else {
        ResumePlan::Fresh
    }
}

/// Inputs for one resolver-worker attempt. Built on the main thread in
/// `spawn_video`; everything the pipeline needs, nothing GTK.
#[derive(Debug, Clone)]
pub struct VideoJob {
    pub item_id: u64,
    pub page_url: String,
    pub quality: String,
    pub audio_only: bool,
    pub dest: PathBuf,
    pub tries: u32,
    pub timeout_secs: u64,
    pub user_agent: String,
    /// Dialog-pinned video format id, if the user picked an exact format.
    /// `None` means the quality preset decides at attempt time.
    pub video_format_id: Option<String>,
    /// Whether the page is currently live. A live HLS capture finalizes
    /// and keeps its partial on stop instead of discarding it.
    pub is_live: bool,
    /// Raw browser-auth setting (`none` when off). Resolved to a
    /// `--cookies-from-browser` spec inside the worker.
    pub cookies_browser: String,
}

/// Progress reports are throttled to this many bytes between row updates:
/// per-chunk reports would churn the UI for no visible gain.
const PROGRESS_GRANULARITY: u64 = 65536;

/// Run one attempt: resolve → download parts → merge → rename into place.
/// Returns the final size, or `None` when aborted (the pauser/canceller
/// already set the row status; the caller sends nothing).
///
/// # Errors
/// Returns a display-ready [`VideoError`]; the caller reports it as Failed.
pub async fn run_video_download(
    job: VideoJob,
    mut abort: oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
    use yt_dlp::VideoSelection as _;

    let staging = staging_dir(job.item_id);
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(VideoError::staging)?;
    let libs = resolve_libraries()?;
    let (yt_version, ff_version) = ensure_tool_versions(&libs).await?;
    tracing::info!(
        item_id = job.item_id,
        host = %page_host(&job.page_url),
        quality = %job.quality,
        audio_only = job.audio_only,
        yt_dlp = %yt_version,
        ffmpeg = %ff_version,
        "starting video attempt"
    );
    // The downloader timeout covers the ffmpeg merge: a full-length
    // merge on a slow CPU dwarfs any network timeout, so never go below
    // the crate default (the user's setting extends it). Metadata uses
    // the crate's extractor timeout via [`fetch_video_page`].
    let timeout = Duration::from_secs(job.timeout_secs.max(300));
    let youtube_bin = libs.youtube.clone();
    let ffmpeg_bin = libs.ffmpeg.clone();
    let mut builder = Downloader::builder(libs, staging.clone()).with_timeout(timeout);
    if !job.user_agent.is_empty() {
        builder = builder.with_user_agent(job.user_agent.clone());
    }
    // Authenticated extraction for gated pages via the browser profile;
    // the part downloads reuse the extractor-resolved headers as before.
    if let Some(spec) = cookies_browser_spec(&job.cookies_browser) {
        builder = builder.with_cookies_from_browser(spec);
    }
    let downloader = builder.build().await.map_err(VideoError::fetch)?;
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
    for attempt in 0..job.tries.max(1) {
        match fetch_video_page(
            &youtube_bin,
            &job.page_url,
            &job.cookies_browser,
            Duration::from_secs(300),
        )
        .await
        {
            Ok(v) => {
                video = Some(v);
                break;
            }
            Err(e) if attempt + 1 < job.tries.max(1) => {
                tracing::debug!("video resolve failed, retrying: {e}");
                tokio::time::sleep(Duration::from_secs(u64::from(attempt) + 1)).await;
            }
            Err(e) => return Err(e),
        }
    }
    let Some(video) = video else {
        return Err(VideoError::fetch("empty response"));
    };

    // Select streams: newest codec first (AV1, then VP9/HEVC/AVC1 —
    // same ranking as yt-dlp's `+vcodec:av01` sort), best audio. Older
    // codecs stay as automatic fallback, never a failure. Rejections
    // (HLS/DRM/missing URL) degrade candidates to absent here; the plan
    // below decides between split, single-file and audio-only from
    // what's fetchable.
    // A pinned format id (dialog pick) wins over the preset; when it
    // vanishes from fresh metadata the preset takes over again instead
    // of failing the row.
    let mut video_sel: Option<StreamSel> = if job.audio_only {
        None
    } else if let Some(pinned) = job.video_format_id.as_deref() {
        find_usable_format(&video.formats, pinned).or_else(|| {
            tracing::info!(
                item_id = job.item_id,
                pinned,
                "pinned video format gone, falling back to preset"
            );
            video
                .select_video_format(
                    selector_for_quality(&job.quality),
                    VideoCodecPreference::AV1,
                )
                .and_then(|f| StreamSel::from_format(f).ok())
        })
    } else {
        video
            .select_video_format(
                selector_for_quality(&job.quality),
                VideoCodecPreference::AV1,
            )
            .and_then(|f| StreamSel::from_format(f).ok())
    };
    let mut audio_sel: Option<StreamSel> = video
        .select_audio_format(AudioQuality::Best, AudioCodecPreference::Any)
        .and_then(|f| StreamSel::from_format(f).ok());
    // Muxed-only sources (one file, both tracks — archive.org, file
    // lockers): adopt the file directly instead of failing on the missing
    // split counterpart. A downloaded track beats a failed row; the
    // manifest records the effective single-part mode so retries agree.
    // Unclassified last resort (TikTok-style sparse extractors): both
    // codec fields missing leaves media typed Unknown — invisible to
    // every selector above. Only video-container extensions qualify,
    // so storyboards and manifests can never adopt here.
    let mut audio_only = job.audio_only;
    if audio_sel.is_none() {
        let muxed = video_sel.take_if(|v| v.has_audio).or_else(|| {
            video
                .best_audio_video_format()
                .ok()
                .and_then(|m| StreamSel::from_format(m).ok())
        });
        if let Some(m) = muxed {
            audio_sel = Some(m);
            audio_only = true;
        }
    }
    if audio_sel.is_none() {
        let unknown = video.formats.iter().find(|f| {
            f.format_type() == FormatType::Unknown
                && matches!(
                    f.download_info.ext,
                    Extension::Mp4
                        | Extension::Webm
                        | Extension::Avi
                        | Extension::Flv
                        | Extension::Ts
                )
                && StreamSel::from_format(f).is_ok()
        });
        if let Some(m) = unknown.and_then(|m| StreamSel::from_format(m).ok()) {
            audio_sel = Some(m);
            audio_only = true;
        }
    }
    // HLS fallback (x.com VODs, live replays): nothing above is
    // directly fetchable, but manifest variants exist. VOD captures go
    // through yt-dlp (variant/audio selection, retries, merging); live
    // captures stay on direct ffmpeg for stop-and-keep. A dialog-pinned
    // HLS id wins over the quality preset.
    let hls_sel: Option<HlsSel> = if audio_sel.is_some() {
        None
    } else {
        job.video_format_id
            .as_deref()
            .and_then(|id| find_hls_format(&video.formats, id))
            .or_else(|| select_hls_format(&video.formats, quality_height(&job.quality)))
    };
    if let Some(mut hls) = hls_sel {
        // An abort that fired during resolve means stop-before-start:
        // for live rows there is deliberately no pauser preset waiting
        // on a message, so report instead of going quiet (the pump tail
        // would fail the row either way — this names the cause).
        if job.is_live && abort.try_recv().is_ok() {
            return Err(VideoError::interrupted());
        }
        // Page-level audio rendition as last resort: the variant's own
        // master may name no audio while the page lists HLS audio beside
        // its video variants.
        if hls.fallback_audio.is_none() {
            hls.fallback_audio = select_hls_audio_url(&video.formats);
        }
        tracing::info!(
            item_id = job.item_id,
            page_host = %page_host(&job.page_url),
            height = ?hls.height,
            audio_only = job.audio_only,
            "downloading HLS variant",
        );
        // Live captures stay on direct ffmpeg (stop-and-keep needs its
        // SIGTERM finalizing); VOD captures go through yt-dlp, whose
        // native fragment handling (retries, keys, audio merging)
        // beats a hand-rolled ffmpeg invocation.
        if job.is_live {
            return run_hls_ffmpeg(&ffmpeg_bin, &staging, &job, hls, abort, timeout, tx).await;
        }
        return run_hls_ytdlp(
            &youtube_bin,
            &ffmpeg_bin,
            &staging,
            &job,
            abort,
            timeout,
            tx,
        )
        .await;
    }
    let Some(audio_sel) = audio_sel else {
        return Err(VideoError::unavailable_detail(&video.formats));
    };

    tracing::info!(
        item_id = job.item_id,
        video = ?video_sel.as_ref().map(|s| s.format_id.as_str()),
        audio = %audio_sel.format_id,
        audio_only = audio_only,
        "formats selected"
    );

    // Retry discipline from the sidecar.
    let manifest = read_manifest(&staging);
    let query = ResumeQuery {
        manifest: manifest.as_ref(),
        dir: &staging,
        dest: &job.dest,
        page_url: &job.page_url,
        quality: &job.quality,
        audio_only,
        video: video_sel
            .as_ref()
            .map(|s| (s.format_id.as_str(), s.ext.as_str())),
        audio: (audio_sel.format_id.as_str(), audio_sel.ext.as_str()),
        video_total: video_sel.as_ref().and_then(|s| s.size),
        audio_total: audio_sel.size,
    };
    let plan = resume_plan(&query);
    match plan {
        ResumePlan::Finished => return Ok(file_len(&job.dest)),
        ResumePlan::Fresh => {
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
            // Record this attempt's selection up front: a pause from here
            // on leaves a matchable sidecar, so the next attempt resumes
            // instead of wiping.
            write_manifest(
                &staging,
                &VideoManifest {
                    page_url: job.page_url.clone(),
                    quality: job.quality.clone(),
                    audio_only,
                    video_format_id: video_sel.as_ref().map(|s| s.format_id.clone()),
                    video_ext: video_sel
                        .as_ref()
                        .map(|s| s.ext.clone())
                        .unwrap_or_default(),
                    video_bytes: 0,
                    audio_format_id: audio_sel.format_id.clone(),
                    audio_ext: audio_sel.ext.clone(),
                    audio_bytes: 0,
                    final_bytes: None,
                },
            )
            .await?;
        }
        ResumePlan::CombineOnly => {}
        ResumePlan::Resume => {
            phase(gettext("Resuming download…"));
        }
    }
    let have_parts = plan == ResumePlan::CombineOnly;

    // Byte-weighted progress across both parts: the pump renders the same
    // rich detail (percent, amounts, speed, ETA) as HTTP rows.
    let combined_total: Option<u64> =
        match (video_sel.as_ref().and_then(|s| s.size), audio_sel.size) {
            (Some(v), Some(a)) => Some(v + a),
            (None, Some(a)) if audio_only => Some(a),
            _ => None,
        };
    let v_done = Arc::new(AtomicU64::new(0));
    let a_done = Arc::new(AtomicU64::new(0));
    let sent = Arc::new(AtomicU64::new(0));
    let make_cb = |mine: Arc<AtomicU64>, other: Arc<AtomicU64>| {
        let tx = tx.clone();
        let sent = sent.clone();
        move |done: u64, total: u64| {
            mine.store(done, Ordering::Relaxed);
            let prev = sent.load(Ordering::Relaxed);
            if done.saturating_sub(prev) >= PROGRESS_GRANULARITY || (total > 0 && done >= total) {
                sent.store(done, Ordering::Relaxed);
                tx.send(EngineMsg::Progress {
                    downloaded: done + other.load(Ordering::Relaxed),
                    total: combined_total,
                    uploaded: 0,
                    upload_bps: 0,
                })
                .ok();
            }
        }
    };

    let mgr = downloader.download_manager().clone();
    // NOTE: Grab's speed limit does not apply here — the crate's download
    // manager exposes no rate knob, so parts run unthrottled. Retries and
    // timeouts still follow the user's settings.
    let vpart: Option<PathBuf> = video_sel
        .as_ref()
        .map(|s| part_path(&staging, "video", &s.ext));
    let apart: PathBuf = part_path(&staging, "audio", &audio_sel.ext);
    let mut ids: Vec<u64> = Vec::new();
    if !have_parts {
        if let Some(v) = &video_sel {
            let path = vpart
                .clone()
                .unwrap_or_else(|| part_path(&staging, "video", &v.ext));
            let id = mgr
                .enqueue_with_progress_and_headers(
                    v.url.as_str(),
                    path,
                    Some(DownloadPriority::Normal),
                    make_cb(Arc::clone(&v_done), Arc::clone(&a_done)),
                    Some(v.headers.clone()),
                )
                .await;
            ids.push(id);
        }
        let id = mgr
            .enqueue_with_progress_and_headers(
                audio_sel.url.as_str(),
                apart.clone(),
                Some(DownloadPriority::Normal),
                make_cb(Arc::clone(&a_done), Arc::clone(&v_done)),
                Some(audio_sel.headers.clone()),
            )
            .await;
        ids.push(id);
    }

    let work = async {
        if !have_parts {
            for id in &ids {
                match mgr.wait_for_completion(*id).await {
                    Some(YtDownloadStatus::Completed) => {}
                    Some(YtDownloadStatus::Failed { reason }) => {
                        return Err(VideoError::part_failed(&reason));
                    }
                    _ => return Err(VideoError::interrupted()),
                }
            }
            // Measure reality, not the plan: a zero-byte "completed" part
            // must fail now, or the retry would combine empties forever.
            let video_bytes = vpart.as_ref().and_then(|p| file_len(p)).unwrap_or(0);
            let audio_bytes = file_len(&apart).unwrap_or(0);
            if audio_bytes == 0 || (vpart.is_some() && video_bytes == 0) {
                return Err(VideoError::part_failed("empty stream"));
            }
            // Record the sidecar BEFORE merging, so a crash during the
            // merge still retries without re-downloading.
            write_manifest(
                &staging,
                &VideoManifest {
                    page_url: job.page_url.clone(),
                    quality: job.quality.clone(),
                    audio_only,
                    video_format_id: video_sel.as_ref().map(|s| s.format_id.clone()),
                    video_ext: video_sel
                        .as_ref()
                        .map(|s| s.ext.clone())
                        .unwrap_or_default(),
                    video_bytes,
                    audio_format_id: audio_sel.format_id.clone(),
                    audio_ext: audio_sel.ext.clone(),
                    audio_bytes,
                    final_bytes: None,
                },
            )
            .await?;
        }
        // Only merged video shows a merge phase: audio-only rows adopt
        // the part directly, so announcing a merge would be wrong.
        if vpart.is_some() {
            phase(gettext("Merging…"));
        }
        finish_merge(
            &downloader,
            &staging,
            vpart.as_deref(),
            &apart,
            &job.dest,
            video_sel.as_ref().map(|s| s.ext.as_str()),
        )
        .await
        .map(Some)
    };
    tokio::select! {
        r = work => r,
        _ = abort => {
            for id in &ids {
                mgr.cancel(*id).await;
            }
            Ok(None)
        }
    }
}

/// Merge verified parts and rename the result into place. Audio-only rows
/// adopt the audio part directly (no ffmpeg round-trip).
async fn finish_merge(
    downloader: &Downloader,
    staging: &Path,
    vpart: Option<&Path>,
    apart: &Path,
    dest: &Path,
    video_ext: Option<&str>,
) -> Result<u64, VideoError> {
    let final_tmp: PathBuf = match (vpart, video_ext) {
        (Some(v), Some(ext)) => {
            let muxed = staging.join(format!("muxed.{ext}"));
            downloader
                .combine_audio_and_video_to_path(apart, v, &muxed)
                .await
                .map_err(VideoError::combine)?;
            muxed
        }
        // Audio-only: the part already is the finished file (container
        // contract from the intake default: .m4a; players sniff content).
        _ => apart.to_path_buf(),
    };
    // Atomic claim: a foreign file appearing after intake dedupe requeues
    // with a fresh name through the pump's DEST_EXISTS path, parts intact.
    // Any other move failure (permissions, full disk, …) is a merge-phase
    // error, not a staging one.
    match crate::download::rename_noreplace(&final_tmp, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    // Record the finished size so a later retry adopts the file.
    let final_bytes = file_len(dest);
    if let Some(mut m) = read_manifest(staging) {
        m.final_bytes = final_bytes;
        let _ = write_manifest(staging, &m).await;
    }
    let _ = tokio::fs::remove_dir_all(staging).await;
    Ok(final_bytes.unwrap_or(0))
}

/// Ask ffmpeg to stop gracefully: SIGTERM finalizes the container so
/// the partial plays, unlike SIGKILL. Escalates to SIGKILL after
/// `grace`, then waits out the exit either way. Returns whether the
/// process exited on its own accord — only then is the file adoptable.
async fn terminate_ffmpeg(child: &mut tokio::process::Child, grace: Duration) -> bool {
    if let Some(pid) = child.id() {
        // SAFETY: constant signal number; ESRCH (already dead) is harmless.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(grace, child.wait()).await.is_ok() {
        return true;
    }
    child.kill().await.ok();
    let _ = child.wait().await;
    false
}

/// Move a finished HLS capture into place. Captures that never got
/// data (stopped before the first segment) fail instead of stranding
/// an empty Done row.
async fn adopt_hls_output(
    out: &Path,
    dest: &Path,
    staging: &Path,
) -> Result<Option<u64>, VideoError> {
    if file_len(out).unwrap_or(0) == 0 {
        let _ = tokio::fs::remove_dir_all(staging).await;
        return Err(VideoError::part_failed("nothing recorded"));
    }
    // Atomic claim into place (EXDEV-safe, no clobber).
    match crate::download::rename_noreplace(out, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
    }
    let _ = tokio::fs::remove_dir_all(staging).await;
    Ok(file_len(dest))
}

/// Stop a live HLS capture and keep what's recorded: terminate
/// gracefully, then adopt the partial if it finalized. Used for both
/// user stops and timeouts on live rows — a stalled or stopped live
/// capture still yields playable media instead of nothing.
async fn stop_hls_capture(
    child: &mut tokio::process::Child,
    progress: tokio::task::JoinHandle<u64>,
    logs: tokio::task::JoinHandle<String>,
    out_path: PathBuf,
    dest: PathBuf,
    staging: PathBuf,
) -> Result<Option<u64>, VideoError> {
    let exited_clean = terminate_ffmpeg(child, Duration::from_secs(10)).await;
    progress.abort();
    let _ = logs.await;
    if !exited_clean {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(VideoError::part_failed("nothing recorded"));
    }
    adopt_hls_output(&out_path, &dest, &staging).await
}

/// yt-dlp `-f` spec for one HLS attempt over the page URL (yt-dlp
/// re-resolves and merges itself). Height dominates like the picker;
/// no codec filter, so a weird page can never fail on sorting. A
/// pinned muxed id may gain a redundant second audio track via `+ba`,
/// which players ignore — always pairing audio beats risking silence.
fn hls_format_spec(quality: &str, pinned: Option<&str>, audio_only: bool) -> String {
    if audio_only {
        return "ba/b".to_string();
    }
    if let Some(id) = pinned.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("{id}+ba/b");
    }
    match quality_height(quality) {
        Some(h) => format!("bv[height<={h}]+ba/bv*[height<={h}]+ba/b"),
        None => "bv+ba/bv*+ba/b".to_string(),
    }
}

/// Record one live HLS variant with ffmpeg (`-c copy`: the variant
/// playlist, segment requests and any playlist keys are ffmpeg's
/// business). Live rows stop-and-keep via SIGTERM finalizing; VOD rows
/// go through [`run_hls_ytdlp`] instead. Returns the final size, or
/// `None` when a VOD attempt aborts.
async fn run_hls_ffmpeg(
    ffmpeg: &Path,
    staging: &Path,
    job: &VideoJob,
    hls: HlsSel,
    abort: oneshot::Receiver<()>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
    let _ = tokio::fs::remove_dir_all(staging).await;
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    let out_path = staging.join(if job.audio_only { "hls.m4a" } else { "hls.mp4" });
    // Master playlists resolve to a height-appropriate variant plus its
    // audio rendition when separate; anything else passes through as today's
    // single input.
    let mut input = resolve_hls_input(&hls.url, &hls.headers, &job.user_agent, hls.height).await;
    // Page-level fallback: the master named no audio, but the page
    // lists an audio-only HLS rendition beside its video variants.
    if input.audio.is_none() {
        input.audio.clone_from(&hls.fallback_audio);
    }
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-nostats")
        .arg("-progress")
        .arg("pipe:1");
    let headers = ffmpeg_headers(&hls.headers, &job.user_agent);
    // `-headers` is per-input: it must precede each `-i` it covers.
    let input_arg = |cmd: &mut tokio::process::Command, url: &str| {
        if !headers.is_empty() {
            cmd.arg("-headers").arg(&headers);
        }
        cmd.arg("-i").arg(url);
    };
    if job.audio_only {
        // Audio rendition alone when split (skips the video segments),
        // else the variant with video dropped.
        input_arg(&mut cmd, input.audio.as_deref().unwrap_or(&input.video));
        cmd.arg("-vn");
    } else if let Some(audio) = &input.audio {
        input_arg(&mut cmd, &input.video);
        input_arg(&mut cmd, audio);
        cmd.arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
    } else {
        input_arg(&mut cmd, &input.video);
    }
    cmd.arg("-c")
        .arg("copy")
        .arg(&out_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(VideoError::runtime)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VideoError::runtime("ffmpeg gave no progress pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VideoError::runtime("ffmpeg gave no log pipe"))?;
    // Drain both pipes concurrently: an unread pipe stalls ffmpeg once
    // full, and the tail diagnoses failures. Progress reports byte
    // amounts with unknown total, like length-less engine rows (live
    // streams are unbounded, so no block map applies).
    let tx_p = tx.clone();
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let (mut sent, mut have) = (0u64, 0u64);
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(n) = parse_progress_size(&line) {
                have = have.max(n);
                if have.saturating_sub(sent) >= PROGRESS_GRANULARITY {
                    sent = have;
                    tx_p.send(EngineMsg::Progress {
                        downloaded: have,
                        total: None,
                        uploaded: 0,
                        upload_bps: 0,
                    })
                    .ok();
                }
            }
        }
        have
    });
    let logs = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 8192 {
                        tail.drain(..tail.len() - 8192);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let live = job.is_live;
    let status = tokio::select! {
        biased;
        _ = abort => {
            if live {
                // Live captures keep what's recorded (see
                // stop_hls_capture); VOD attempts just stop.
                return stop_hls_capture(
                    &mut child,
                    progress,
                    logs,
                    out_path,
                    job.dest.clone(),
                    staging.to_path_buf(),
                )
                .await;
            }
            child.kill().await.ok();
            let _ = child.wait().await;
            progress.abort();
            logs.abort();
            let _ = tokio::fs::remove_dir_all(staging).await;
            return Ok(None);
        }
        waited = tokio::time::timeout(timeout, child.wait()) => match waited {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                child.kill().await.ok();
                progress.abort();
                logs.abort();
                return Err(VideoError::runtime(&e));
            }
            Err(_) => {
                // A stalled live capture still yields what it got.
                if live {
                    return stop_hls_capture(
                        &mut child,
                        progress,
                        logs,
                        out_path,
                        job.dest.clone(),
                        staging.to_path_buf(),
                    )
                    .await;
                }
                child.kill().await.ok();
                progress.abort();
                logs.abort();
                return Err(VideoError::part_failed("timed out"));
            }
        },
    };
    let _ = progress.await;
    let log_tail = logs.await.unwrap_or_default();
    if !status.success() {
        let detail = log_tail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("ffmpeg reported failure")
            .trim()
            .to_string();
        return Err(VideoError::part_failed(detail));
    }
    adopt_hls_output(&out_path, &job.dest, staging).await
}

/// Parse a byte size from yt-dlp progress (`~50.00MiB`, `10.5K`, `3B`).
/// Binary and decimal suffixes both occur across versions.
fn parse_ytdlp_size(raw: &str) -> Option<u64> {
    let raw = raw.trim().trim_start_matches('~');
    let split = raw
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map(|(i, _)| i)
        .unwrap_or(raw.len());
    let number: f64 = raw[..split].parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let factor = match raw[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// One parsed yt-dlp `--newline` download line: completion fraction
/// plus the stated total when the line carries one (`of X`).
fn parse_ytdlp_progress(line: &str) -> Option<(f64, Option<u64>)> {
    let rest = line.strip_prefix("[download]")?.trim();
    let mut words = rest.split_whitespace();
    let pct: f64 = words.next()?.strip_suffix('%')?.parse().ok()?;
    if !(0.0..=100.0).contains(&pct) {
        return None;
    }
    let mut total = None;
    let words: Vec<&str> = words.collect();
    if let Some(of) = words.iter().position(|w| *w == "of")
        && let Some(size) = words.get(of + 1)
    {
        total = parse_ytdlp_size(size);
    }
    Some((pct / 100.0, total))
}

/// Whether a `--newline` line announces a merge/extract phase.
fn is_ytdlp_merge_line(line: &str) -> bool {
    line.starts_with("[Merger]") || line.starts_with("[ExtractAudio]")
}

/// Final path from `--print after_move:filepath`: a bare absolute
/// path line (every other stdout line carries a `[tag]` prefix).
fn parse_ytdlp_after_move(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    (!trimmed.is_empty() && !trimmed.starts_with('[') && trimmed.starts_with('/'))
        .then_some(trimmed)
}

/// Newly completed piece indices as byte progress grows against a
/// known total. Shared by the HLS progress tasks so the byte→cell
/// math stays unit-tested in one place.
fn piece_marks(piece_len: u64, marked: &mut u64, downloaded: u64) -> Vec<u64> {
    let mut out = Vec::new();
    if piece_len == 0 {
        return out;
    }
    while *marked < downloaded / piece_len {
        out.push(*marked);
        *marked += 1;
    }
    out
}

/// SIGKILL a spawned downloader and the ffmpeg it may have started:
/// both run in a dedicated process group (`process_group(0)` at
/// spawn), so one killpg reaps the tree instead of orphaning ffmpeg
/// mid-merge.
fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: constant signal number; ESRCH (already dead) is harmless.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// The finished file of one yt-dlp attempt: the `after_move` path
/// when it landed under staging, else the largest non-temp file.
/// Temp suffixes (parts, metadata sidecars) never qualify.
fn discover_ytdlp_output(staging: &Path, after_move: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = after_move
        && let Ok(canonical) = std::fs::canonicalize(path)
        && canonical.starts_with(staging)
        && canonical.is_file()
    {
        return Some(canonical);
    }
    std::fs::read_dir(staging)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && !matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("part" | "ytdl" | "temp" | "tmp" | "frag")
                )
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

/// One VOD HLS attempt through the yt-dlp binary: native fragment
/// handling (retries, parallel fragments, keys) plus merging and
/// audio extraction, with Grab parsing `--newline` progress. No
/// resume sidecar of its own — `.part` files in staging resume across
/// attempts instead. Returns the final size, or `None` when aborted.
async fn run_hls_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    abort: oneshot::Receiver<()>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(VideoError::staging)?;
    let spec = hls_format_spec(&job.quality, job.video_format_id.as_deref(), job.audio_only);
    let mut cmd = tokio::process::Command::new(youtube_bin);
    cmd.arg("--no-playlist")
        .arg("--newline")
        .arg("-f")
        .arg(&spec)
        .arg("-o")
        .arg(staging.join("grab-hls.%(ext)s"))
        .arg("--ffmpeg-location")
        .arg(ffmpeg_bin.parent().unwrap_or_else(|| Path::new("/usr/bin")))
        .arg("--retries")
        .arg(job.tries.max(1).to_string())
        .arg("--print")
        .arg("after_move:filepath");
    if job.audio_only {
        cmd.arg("--extract-audio").arg("--audio-format").arg("m4a");
    } else {
        cmd.arg("--merge-output-format").arg("mp4");
    }
    if let Some(spec) = cookies_browser_spec(&job.cookies_browser) {
        cmd.arg(format!("--cookies-from-browser={spec}"));
    }
    if !job.user_agent.is_empty() {
        cmd.arg("--user-agent").arg(&job.user_agent);
    }
    cmd.arg(&job.page_url);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(VideoError::runtime)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| VideoError::runtime("yt-dlp gave no output pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| VideoError::runtime("yt-dlp gave no log pipe"))?;
    // Progress lines may land on either stream depending on version;
    // parse both, collect the log tail for failure diagnostics.
    let tx_p = tx.clone();
    let progress = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let (mut max_dl, mut max_total, mut marked) = (0u64, None, 0u64);
        let mut after_move = None::<String>;
        let mut merged = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if is_ytdlp_merge_line(&line) && !merged {
                merged = true;
                tx_p.send(EngineMsg::Phase(gettext("Merging…"))).ok();
            } else if let Some(path) = parse_ytdlp_after_move(&line) {
                after_move = Some(path.to_string());
            } else if let Some((frac, total)) = parse_ytdlp_progress(&line) {
                if let Some(t) = total {
                    if max_total.is_none() {
                        if t > 0 {
                            tx_p.send(EngineMsg::SegmentsInit { total: t }).ok();
                        }
                    } else if Some(t) != max_total {
                        // New file (second format leg): restart the map on
                        // the new total instead of mixing scales.
                        tx_p.send(EngineMsg::SegmentsInit { total: t }).ok();
                        marked = 0;
                    }
                    max_total = Some(t.max(max_total.unwrap_or(0)));
                }
                if let Some(t) = max_total
                    && t > 0
                {
                    let have = max_dl.max((frac * t as f64) as u64);
                    max_dl = have;
                    for idx in piece_marks(crate::download::piece_len(t), &mut marked, have) {
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
    let logs = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
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
        let detail = log_tail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("yt-dlp reported failure")
            .trim()
            .to_string();
        return Err(VideoError::part_failed(detail));
    }
    let final_tmp = discover_ytdlp_output(staging, after_move.as_deref());
    let Some(final_tmp) = final_tmp else {
        return Err(VideoError::part_failed("no output file produced"));
    };
    // Atomic claim into place (EXDEV-safe, no clobber).
    match crate::download::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::combine(&e)),
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
