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
use yt_dlp::client::deps::{Libraries, LibraryInstaller};
use yt_dlp::model::format::{Extension, Format, FormatType, Protocol};
use yt_dlp::model::selector::{VideoCodecPreference, VideoQuality};
use yt_dlp::model::{DrmStatus, FORMAT_URL_LIFETIME, Video};

/// Values for the `video-quality` GSettings key and the per-item quality
/// stored on [`VideoSource::Page`]. Index-aligned with the ComboRow models
/// in Preferences and the New Download dialog — see [`quality_index`] and
/// [`quality_value`]. Labels are translated at the call sites.
pub const VIDEO_QUALITY_VALUES: &[&str] = &["best", "2160p", "1440p", "1080p", "720p", "480p"];

/// Default quality when nothing is stored (or the stored value is unknown).
pub fn default_video_quality() -> String {
    "1080p".to_string()
}

/// Combo-box index for a stored value against an ordered value table.
/// Unknown values fall back to `fallback`, so a hand-edited dconf key
/// can never desync the combo. Fallbacks differ per combo by design —
/// pass them through, don't unify them. Pure.
pub(crate) fn combo_index(values: &[&str], value: &str, fallback: usize) -> usize {
    values.iter().position(|v| *v == value).unwrap_or(fallback)
}

/// Stored value for a combo-box index against an ordered value table.
/// Out-of-range indexes (the model is rebuilt from translations) fall
/// back instead of panicking. Pure.
pub(crate) fn combo_value<'a>(values: &[&'a str], index: usize, fallback: &'a str) -> &'a str {
    values.get(index).copied().unwrap_or(fallback)
}

/// Model index for a stored quality value. Unknown values fall back to the
/// 1080p row so a hand-edited dconf key can't desync the combo.
pub fn quality_index(value: &str) -> usize {
    combo_index(VIDEO_QUALITY_VALUES, value, 3)
}

/// Stored value for a combo index. Out-of-range indexes (shouldn't happen,
/// but the model is rebuilt from translations) fall back to 1080p.
pub fn quality_value(index: usize) -> &'static str {
    combo_value(VIDEO_QUALITY_VALUES, index, "1080p")
}

/// Default file name for a resolved video when the user left the name
/// blank: yt-dlp's default output template (`%(title)s [%(id)s].%(ext)s`)
/// with the container the worker will produce. Grab keeps passing yt-dlp
/// a literal `-o` path, so the template is emulated here at naming time
/// rather than expanded by yt-dlp — the pipeline (dedupe, rename
/// claims, resume) needs the final name up front. An empty id falls back
/// to the bare title. The intake sanitizes it further.
///
/// The extension is the remux target when the row is a video download
/// with remux enabled: without it the worker remuxes to mkv and claims
/// matroska bytes under an mp4 name. Audio-only ignores remux (no video
/// leg exists) and always takes m4a. The target is lowercased here so
/// the contract holds no matter the caller (all current callers pass
/// the allowlisted lowercase already).
pub fn default_video_filename(
    title: &str,
    id: &str,
    audio_only: bool,
    remux_ext: Option<&str>,
) -> String {
    let ext = if audio_only {
        "m4a".to_string()
    } else {
        remux_ext.unwrap_or("mp4").to_ascii_lowercase()
    };
    let id = id.trim();
    if id.is_empty() {
        format!("{title}.{ext}")
    } else {
        format!("{title} [{id}].{ext}")
    }
}

/// Whether a row name is just the page URL derived at intake:
/// dialog-less rows skip the picker, so their names are URL stems
/// ("watch"). Matches the derived stem modulo intake-dedupe ` (N)`
/// suffixes. Dialog-seeded and typed names never match (unless
/// perversely identical to the URL stem). Pure for tests.
pub(crate) fn is_url_derived_name(current: &str, page_url: &str) -> bool {
    let derived = crate::download::filename_from_url(page_url);
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

/// Per-row video choices from the New Download dialog: quality
/// preset, audio-only switch, format pin and liveness. Bundled so
/// intake entry points stay under the argument-count lint.
#[derive(Debug, Clone)]
pub struct VideoChoices {
    pub quality: String,
    /// Per-download audio-only switch from the New Download dialog.
    /// Deliberately not a preference: no global default exists.
    pub audio_only: bool,
    pub video_format_id: Option<String>,
    pub is_live: bool,
    /// yt-dlp id of the playlist entry this row was picked from, if any.
    /// Instagram stamps every story/highlight entry with the collection
    /// URL, so the row's page URL re-resolves the whole tray: the worker
    /// selects the picked entry by this id instead.
    pub playlist_item_id: Option<String>,
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
    /// per-item choices from the New Download dialog (audio has no
    /// global default by design).
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
        /// yt-dlp id of the playlist entry this row was picked from, if
        /// any (see [`VideoChoices::playlist_item_id`]). Persisted so
        /// rows restored across launches still resolve the picked story.
        #[serde(default)]
        playlist_item_id: Option<String>,
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
                    playlist_item_id: None,
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
pub fn preview_fresh(info: &Option<ProbeResult>, last_ok: &str, url: &str) -> bool {
    !url.is_empty() && last_ok == url && info.is_some()
}

fn video_domain(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    VIDEO_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Google Drive file id from share/download URLs: `file/d/<id>` or
/// `uc`/`open`/`download?id=<id>`. Drive serves videos and plain
/// files behind the same URLs, but yt-dlp's Drive extractor is
/// playback-API-only (PDFs, docs and zips fail it with HTTP 400
/// instead of formats), so the dialog falls back to this id for a
/// direct download instead of stranding non-video files. Id shape
/// mirrors yt-dlp's own pattern (`[a-zA-Z0-9_-]{28,}`). Pure for tests.
pub fn drive_file_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !(host == "drive.google.com"
        || host.ends_with(".drive.google.com")
        || host == "docs.google.com"
        || host.ends_with(".docs.google.com")
        || host == "drive.usercontent.google.com"
        || host.ends_with(".drive.usercontent.google.com"))
    {
        return None;
    }
    let valid = |id: &str| {
        id.len() >= 28
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    // /file/d/<id>(/...)
    if let Some(mut segs) = parsed.path_segments()
        && segs.next() == Some("file")
        && segs.next() == Some("d")
        && let Some(id) = segs.next()
        && valid(id)
    {
        return Some(id.to_string());
    }
    // ?id=<id> on the uc/open/download endpoints.
    if let Some(first) = parsed.path_segments().and_then(|mut s| s.next())
        && matches!(first, "uc" | "open" | "download")
    {
        for (key, value) in parsed.query_pairs() {
            if key == "id" && valid(&value) {
                return Some(value.into_owned());
            }
        }
    }
    None
}

/// Direct download URL for a Drive share link: the export endpoint
/// with `confirm=t` (same shape yt-dlp uses for its `source`
/// format, so virus-scan-sized files skip the confirmation page).
/// The plain engine follows the redirect and the server filename
/// wins via Content-Disposition. Pure for tests.
pub fn drive_direct_url(url: &str) -> Option<String> {
    drive_file_id(url).map(|id| {
        format!("https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t")
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
    /// Whether the resolved formats include anything fetchable (see
    /// [`has_fetchable_media`]): the dialog probe offers the video path
    /// for unlisted pages on this, instead of the domain list.
    pub fetchable: bool,
}

impl VideoInfo {
    fn from(v: &Video, fallback_page: &str, newest_first: bool) -> Self {
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
            formats: video_format_options(v, newest_first),
            is_live: v.is_live.unwrap_or(false),
            fetchable: has_fetchable_media(v),
        }
    }
}

/// What one probe of a URL resolved to: either a single video page or a
/// multi-item collection (playlist, stories, highlights). The dialog
/// branches on this: singles get the format-picker preview, collections
/// get the item picker.
#[derive(Clone, Debug)]
pub enum ProbeResult {
    Single(VideoInfo),
    Playlist(PlaylistInfo),
}

impl ProbeResult {
    /// Canonical URL of the probed page (the collection URL for
    /// playlists). Used as the round-trip freshness key.
    pub fn page_url(&self) -> &str {
        match self {
            ProbeResult::Single(v) => &v.page_url,
            ProbeResult::Playlist(p) => &p.page_url,
        }
    }

    /// Display title: video title or collection title.
    pub fn title(&self) -> &str {
        match self {
            ProbeResult::Single(v) => &v.title,
            ProbeResult::Playlist(p) => &p.title,
        }
    }

    /// Extractor video id for a single-video probe; empty for collections
    /// (their items carry their own ids, applied per row at queue time).
    pub fn video_id(&self) -> &str {
        match self {
            ProbeResult::Single(v) => &v.id,
            ProbeResult::Playlist(_) => "",
        }
    }

    /// Whether the probe found anything worth offering: a fetchable
    /// single, or a collection with at least one queueable item.
    pub fn fetchable(&self) -> bool {
        match self {
            ProbeResult::Single(v) => v.fetchable,
            ProbeResult::Playlist(p) => !p.items.is_empty(),
        }
    }
}

/// Which flavor of multi-item collection a probe found. Affects the
/// picker labels ("3 stories" vs "3 items") and whether single-story
/// pastes are retargeted to the owner's tray; the download pipeline
/// otherwise treats them alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistKind {
    Playlist,
    Stories,
    Highlights,
}

impl PlaylistKind {
    /// Classify from the probed URL (what the user typed) with the
    /// extractor key as backup. Heuristic by design: a wrong guess only
    /// mislabels the item count.
    fn classify(page_url: &str, extractor_key: &str) -> Self {
        let url = page_url.to_lowercase();
        let key = extractor_key.to_lowercase();
        if url.contains("highlight") || key.contains("highlight") {
            Self::Highlights
        } else if url.contains("/stories/") || key.contains("stories") {
            Self::Stories
        } else {
            Self::Playlist
        }
    }
}

/// One entry of a probed collection. Deliberately small like
/// [`VideoInfo`]: each queued row re-resolves its own item page at
/// download time, so the picker only needs identity + label.
#[derive(Clone, Debug)]
pub struct PlaylistItem {
    /// 1-based position (`playlist_index` when the extractor reports it).
    pub index: usize,
    pub id: String,
    pub title: String,
    /// Canonical per-item page URL — the identity queued and persisted.
    pub page_url: String,
    /// Duration in seconds, when the listing reports one (flat listings
    /// usually don't).
    pub duration: Option<i64>,
}

/// A probed multi-item collection: the picker lists [`PlaylistInfo::items`],
/// the queue gets one row per chosen item.
#[derive(Clone, Debug)]
pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    /// The collection URL that was probed.
    pub page_url: String,
    pub kind: PlaylistKind,
    /// Entries reported by the extractor, before the picker cap.
    pub total: usize,
    pub items: Vec<PlaylistItem>,
}

/// Whether a resolved page carries anything fetchable: at least one
/// plain-HTTPS or HLS-manifest format with a URL and no DRM — the same
/// acceptance the worker applies, minus container specifics the planner
/// refines. The dialog probe uses this to offer the video path for
/// unlisted pages; an empty extraction falls back to a plain file
/// download.
pub fn has_fetchable_media(video: &Video) -> bool {
    video.formats.iter().any(|f| {
        matches!(f.protocol, Protocol::Https | Protocol::M3U8Native)
            && !matches!(f.has_drm, Some(DrmStatus::Yes))
            && f.download_info
                .url
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty())
    })
}

/// Whether the string is an HTTP(S) URL: the only scheme the dialog
/// probes for media (magnets and friends belong to their own flows).
/// Pure for tests.
pub fn is_http_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Whether an HTTP(S) URL is obviously a direct file (not a page):
/// its path ends in a well-known file extension. Such links skip the
/// media probe entirely — the plain engine downloads them better
/// anyway (segmented, resumable, throttled). Query/fragment stripped
/// before matching; comparison is case-insensitive. A page
/// masquerading with a file extension is the accepted residual (the
/// probe only runs on ambiguous links). Pure for tests.
pub fn is_direct_file_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(ext) = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        // A dot is required: extensionless terminal segments (`/md`,
        // `/raw`) are not extensions, and treating them as their own
        // extension would skip the probe for page-like URLs.
        .and_then(|last| last.rsplit_once('.').map(|(_, e)| e))
        .filter(|e| !e.is_empty())
    else {
        return false;
    };
    DIRECT_FILE_EXTS
        .binary_search(&ext.to_ascii_lowercase().as_str())
        .is_ok()
}

/// Extensions treated as direct files (never probed): installers and
/// packages, archives, documents, ebooks, fonts, images, disk images,
/// data, keys, media, torrents, and other unambiguous downloads.
/// Deliberately absent: pages and scripts (`html`, `php`, …), stream
/// playlists (`m3u8`, `m3u`, `pls`), and segments (`ts`). Sorted for
/// `binary_search` — keep it sorted; a test pins that.
const DIRECT_FILE_EXTS: &[&str] = &[
    "3g2",
    "3gp",
    "7z",
    "aac",
    "ac3",
    "ai",
    "aif",
    "aiff",
    "amr",
    "apk",
    "apks",
    "appimage",
    "arw",
    "asc",
    "avi",
    "avif",
    "azw",
    "azw3",
    "bin",
    "bmp",
    "bz2",
    "cab",
    "cbr",
    "cbz",
    "chm",
    "cr2",
    "crt",
    "crx",
    "css",
    "csv",
    "dat",
    "db",
    "deb",
    "djvu",
    "dmg",
    "dng",
    "doc",
    "docx",
    "dts",
    "eot",
    "eps",
    "epub",
    "exe",
    "f4v",
    "fb2",
    "flac",
    "flatpak",
    "flatpakref",
    "flv",
    "gif",
    "gpg",
    "gz",
    "heic",
    "heif",
    "ico",
    "img",
    "ipa",
    "iso",
    "jar",
    "jpeg",
    "jpg",
    "js",
    "json",
    "jxl",
    "key",
    "kra",
    "log",
    "lz4",
    "m2ts",
    "m4a",
    "m4b",
    "m4v",
    "md",
    "mid",
    "midi",
    "mjs",
    "mka",
    "mkv",
    "mobi",
    "mov",
    "mp3",
    "mp4",
    "mpeg",
    "mpg",
    "msi",
    "mts",
    "nef",
    "nzb",
    "odb",
    "odc",
    "odf",
    "odg",
    "odp",
    "ods",
    "odt",
    "oga",
    "ogg",
    "ogv",
    "opus",
    "otf",
    "ovpn",
    "par2",
    "pdf",
    "pem",
    "pkg",
    "ppt",
    "pptx",
    "psd",
    "qcow2",
    "rar",
    "raw",
    "rpm",
    "rtf",
    "run",
    "sfv",
    "sig",
    "snap",
    "sqlite",
    "srt",
    "svg",
    "tar",
    "tgz",
    "tif",
    "tiff",
    "torrent",
    "ttf",
    "txt",
    "vdi",
    "vhd",
    "vhdx",
    "vmdk",
    "vtt",
    "war",
    "wav",
    "webm",
    "wma",
    "wmv",
    "woff",
    "woff2",
    "xapk",
    "xcf",
    "xls",
    "xlsx",
    "xpi",
    "xz",
    "yaml",
    "yml",
    "zip",
    "zst",
];

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
/// Directory where on-demand tool installs keep the yt-dlp and ffmpeg
/// binaries: `$XDG_DATA_HOME/grab/libs`. Both Flatpak and tarball/dev
/// builds fetch the tools here ([`install_ytdlp`], [`install_ffmpeg`]);
/// `/app/bin` and PATH remain as fallbacks for system-provided copies.
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
/// installs first (so Update actually takes effect over any bundled copy),
/// then `/app/bin` (Flatpak), then PATH (which inside Flatpak includes the
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

/// Install just yt-dlp into the user library dir. Split from ffmpeg so the
/// UI can report honest per-tool stages; the crate installer exposes no
/// progress of its own. Await from a spawned task — never block the GTK
/// thread on it.
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

/// Install the ffmpeg toolchain (ffmpeg *and* ffprobe) into the user
/// library dir. The yt-dlp crate's installer only extracts the `ffmpeg`
/// binary, which leaves `--ffmpeg-location` pointing at a dir without
/// ffprobe — so Grab downloads the static-build archive itself and
/// extracts both tools in one pass. See [`install_ytdlp`].
pub async fn install_ffmpeg() -> Result<PathBuf, VideoError> {
    let dir = user_lib_dir();
    let handle =
        crate::download::tokio_rt().spawn(async move { install_ffmpeg_toolchain(dir).await });
    match handle.await {
        Ok(res) => res,
        Err(e) => Err(VideoError::runtime(&e)),
    }
}

/// Download one boul2gom/ffmpeg-builds archive and extract the `ffmpeg`
/// and `ffprobe` binaries into `dir`. Returns the ffmpeg path. Await
/// from a spawned task — never block the GTK thread on it.
async fn install_ffmpeg_toolchain(dir: PathBuf) -> Result<PathBuf, VideoError> {
    use yt_dlp::client::deps::ffmpeg::BuildFetcher;

    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(VideoError::install)?;
    let release = BuildFetcher::new()
        .fetch_binary()
        .await
        .map_err(VideoError::install)?;
    let archive = dir.join(&release.name);
    release
        .download(&archive)
        .await
        .map_err(VideoError::install)?;
    tokio::task::spawn_blocking(move || extract_ffmpeg_toolchain(&archive, &dir))
        .await
        .map_err(VideoError::runtime)?
        .map_err(VideoError::install)
}

/// Extract the `ffmpeg` and `ffprobe` binaries from a static-build
/// archive into `dir`, mark them executable, and delete the archive.
/// Entries are matched by file name, so both flat zips (`ffmpeg` at the
/// root, as the crate's own extractor assumes) and `bin/`-style layouts
/// work. Returns the ffmpeg path; a missing ffmpeg entry is an error,
/// a missing ffprobe entry is not — callers keep working the way they
/// did before this toolchain existed.
fn extract_ffmpeg_toolchain(archive: &Path, dir: &Path) -> Result<PathBuf, String> {
    // Always clean up the (large) archive, even when extraction fails.
    let result = extract_ffmpeg_toolchain_inner(archive, dir);
    std::fs::remove_file(archive).ok();
    result
}

/// Inner extraction; see [`extract_ffmpeg_toolchain`].
fn extract_ffmpeg_toolchain_inner(archive: &Path, dir: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;

    let file = std::fs::File::open(archive)
        .map_err(|e| format!("couldn't open ffmpeg archive {}: {e}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("couldn't read ffmpeg archive: {e}"))?;
    let mut ffmpeg_path = None;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("couldn't read ffmpeg archive entry: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let tool = match Path::new(entry.name()).file_name().and_then(|n| n.to_str()) {
            Some("ffmpeg") => "ffmpeg",
            Some("ffprobe") => "ffprobe",
            _ => continue,
        };
        let dest = dir.join(tool);
        let mut out = std::fs::File::create(&dest)
            .map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("couldn't extract {}: {e}", dest.display()))?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("couldn't mark {} executable: {e}", dest.display()))?;
        if tool == "ffmpeg" {
            ffmpeg_path = Some(dest);
        }
    }
    ffmpeg_path.ok_or_else(|| "ffmpeg binary not found in the downloaded archive".to_string())
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
/// output alike — all mean "unusable". Uses the shared runtime's handle
/// directly (not `tokio::task::spawn_blocking`) so this stays callable
/// from the GTK thread, which has no tokio context entered.
async fn tool_first_line(binary: PathBuf, version_arg: &'static str) -> Option<String> {
    crate::download::tokio_rt()
        .spawn_blocking(move || {
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

/// Display-ready version line for an installed tool binary: yt-dlp's
/// `--version` output labeled ("2026.08.19" → "yt-dlp 2026.08.19"),
/// ffmpeg's first line trimmed to its version
/// token ("ffmpeg version n9.0.1 …" → "ffmpeg n9.0.1"). `None` when the
/// binary can't be probed. For the install-progress popover; the
/// Preferences tools row keeps its own synchronous probe.
pub(crate) async fn tool_display_version(
    binary: PathBuf,
    version_arg: &'static str,
) -> Option<String> {
    let line = tool_first_line(binary.clone(), version_arg).await?;
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy() == "ffmpeg")
    {
        let token = line.split_whitespace().nth(2)?;
        return Some(format!("ffmpeg {token}"));
    }
    if binary
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("yt-dlp"))
    {
        return Some(format!("yt-dlp {line}"));
    }
    Some(line)
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
    "none", "brave", "chrome", "chromium", "edge", "firefox", "opera", "vivaldi", "whale", "zen",
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
        "Zen".to_string(),
    ];
    labels.insert(0, gettext("None"));
    labels
}

/// Combo index for a stored browser value. Unknown values fall back to
/// None rather than selecting a browser the user didn't pick.
pub fn cookies_browser_index(value: &str) -> usize {
    combo_index(COOKIES_BROWSERS, value, 0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to off.
pub fn cookies_browser_value(index: usize) -> &'static str {
    combo_value(COOKIES_BROWSERS, index, "none")
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

/// Chromium config subdirs per browser, most common first. One entry covers
/// the whole family: stable, beta, nightly/canary and dev builds all
/// resolve under it. Within one channel the first profile wins
/// ([`chromium_profile_dirs`] order); across channels the freshest `Cookies`
/// database wins — see [`freshest_chromium_profile`].
fn chromium_subdirs(browser: &str) -> &'static [&'static str] {
    match browser {
        "brave" => &[
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser-Beta",
            "BraveSoftware/Brave-Browser-Nightly",
            // Rebranded builds seen in the wild; skipped when absent.
            // Add non-standard forks only with a reported real path.
            "BraveSoftware/Brave-Origin-Beta",
            "BraveSoftware/Brave-Origin-Nightly",
            "BraveSoftware/Brave-Browser-Origin-Nightly",
        ],
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
            // Not shipped on Linux today; harmless if absent.
            "microsoft-edge-canary",
        ],
        "opera" => &["opera", "opera-beta", "opera-developer"],
        "vivaldi" => &["vivaldi", "vivaldi-snapshot"],
        "whale" => &["naver-whale"],
        _ => &[],
    }
}

/// Best profile directory across every channel subdir of one Chromium
/// browser: each channel contributes its preferred profile (same order as
/// [`chromium_profile_dirs` — `Default` first), then the channel whose
/// `Cookies` database was modified most recently wins. That is the browser
/// the user actually runs; a stale install whose directory merely exists
/// (e.g. Brave stable shadowing a daily-driven Origin Beta) must not win.
/// Ties keep channel order via the stable sort; unreadable mtimes sort
/// after every readable one, so the outcome stays deterministic.
fn freshest_chromium_profile(config_home: &Path, browser: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = chromium_subdirs(browser)
        .iter()
        .filter_map(|sub| chromium_profile_dirs(config_home, sub).into_iter().next())
        .collect();
    candidates.sort_by(|a, b| {
        let mtime = |profile: &PathBuf| {
            std::fs::metadata(profile.join("Cookies"))
                .and_then(|meta| meta.modified())
                .ok()
        };
        // Descending; `Option` orders `None` last.
        mtime(b).cmp(&mtime(a))
    });
    candidates.into_iter().next()
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
    // Zen is Firefox-based (same profiles.ini + cookies.sqlite layout),
    // but keeps its profiles under ~/.zen instead of ~/.mozilla/firefox.
    if browser == "zen" {
        return [home.join(".zen"), config_home.join("zen")]
            .into_iter()
            .find_map(|base| firefox_profile_dirs(&base).into_iter().next());
    }
    freshest_chromium_profile(config_home, browser)
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
    // yt-dlp has no "zen" browser; Zen is Firefox-based (same
    // cookies.sqlite layout), so its resolved profile is handed to the
    // Firefox extractor.
    let ytdlp_browser = if value == "zen" { "firefox" } else { value };
    if let Some(dir) = browser_profile_dir(value) {
        return Some(format!("{ytdlp_browser}:{}", dir.display()));
    }
    Some(ytdlp_browser.to_string())
}

/// Shared trailing argv for every yt-dlp spawn: YouTube player-client
/// workaround, browser cookies, user agent, then the page URL behind
/// `--`. One helper so these flags can never drift between extraction,
/// parts, HLS and live-resolve spawns (or let a hostile URL parse as a
/// flag). `None` user agent keeps today's extraction behavior (yt-dlp
/// default UA there).
pub(crate) fn ytdlp_identity_args(
    cookies_browser: &str,
    user_agent: Option<&str>,
    page_url: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    // YouTube force-enables SABR-only streaming for the `web` player
    // client (yt-dlp#12482): its formats come back URL-less and its
    // playability status fails the whole extraction ("The page needs to
    // be reloaded"). The `web` client only enters yt-dlp's default
    // rotation when a JS runtime is available — e.g. the Flatpak's
    // bundled deno — so exclude it everywhere and stay on the working
    // clients. Scoped to the youtube extractor: a no-op for other sites.
    args.push("--extractor-args".to_string());
    args.push("youtube:player_client=-web".to_string());
    if let Some(spec) = cookies_browser_spec(cookies_browser) {
        args.push(format!("--cookies-from-browser={spec}"));
    }
    if let Some(ua) = user_agent.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--user-agent".to_string());
        args.push(ua.to_string());
    }
    // `--` before the page URL: option parsing ends here, so a hostile
    // or malformed URL can never be read as a flag.
    args.push("--".to_string());
    args.push(page_url.to_string());
    args
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

/// Create a staging dir and verify it really lives under Grab's staging
/// root: `create_dir_all` follows symlinks, so a pre-planted link at the
/// predicted path would otherwise redirect parts into attacker-chosen
/// dirs. Returns the canonical path on success.
pub fn ensure_staging_dir(dir: &Path) -> Result<PathBuf, VideoError> {
    std::fs::create_dir_all(dir).map_err(VideoError::staging)?;
    let canon = std::fs::canonicalize(dir).map_err(VideoError::staging)?;
    let root = std::fs::canonicalize(staging_root()).map_err(VideoError::staging)?;
    if canon.starts_with(&root) {
        Ok(canon)
    } else {
        Err(VideoError::staging(gettext(
            "staging directory escaped its root",
        )))
    }
}

/// Remove a staging dir. Guarded: never deletes anything outside Grab's own
/// staging root, so a buggy caller can't nuke user data. Canonicalized on
/// both sides so a symlinked root can't widen the guard either.
pub fn clean_staging(dir: &Path) {
    let (Ok(canon), Ok(root)) = (
        std::fs::canonicalize(dir),
        std::fs::canonicalize(staging_root()),
    ) else {
        return;
    };
    if canon.starts_with(&root) {
        let _ = std::fs::remove_dir_all(canon);
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

/// Repair one format entry in place: neutral defaults for required
/// scalars, transport/container inference for sparse extractors (same
/// rationale as [`sanitize_video_json`]: one sparse entry must not fail
/// the whole parse, and formats are read by the pipeline so they are
/// repaired instead of reset).
fn sanitize_format_entry(entry: &mut serde_json::Map<String, serde_json::Value>) {
    coerce_int_fields(entry);
    entry.entry("format").or_insert(serde_json::json!(""));
    entry.entry("format_id").or_insert(serde_json::json!(""));
    entry.entry("http_headers").or_insert(serde_json::json!({}));
    // Missing transport with an http(s) URL: plain HTTPS fetch
    // is the only sane default — without it the format is
    // invisible to every selector below.
    if !entry.get("protocol").is_some_and(|v| v.is_string()) {
        let http = entry
            .get("url")
            .and_then(|u| u.as_str())
            .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
        if http {
            entry.insert("protocol".to_string(), serde_json::json!("https"));
        }
    }
    if !entry.get("ext").is_some_and(|v| v.is_string()) {
        // Missing container with a telling URL: sparse
        // extractors sometimes omit `ext` while pointing at a
        // plain video file. Sniff the path suffix (containers
        // only, never manifests or storyboards) so the Unknown
        // fallback can still adopt instead of failing the row.
        let suffix = entry
            .get("url")
            .and_then(|u| u.as_str())
            .and_then(|u| u.split(['?', '#']).next())
            .and_then(|p| p.rsplit('.').next())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(suffix.as_str(), "mp4" | "webm" | "avi" | "flv" | "ts") {
            entry.insert("ext".to_string(), serde_json::json!(suffix));
        }
    }
    if entry.contains_key("fragments") {
        entry.insert("fragments".to_string(), serde_json::json!([]));
    }
}

/// Fill in fields the installed yt-dlp binary omits but the crate's model
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
                sanitize_format_entry(entry);
            }
        }
    }
}

/// Picker cap: one row per item is cheap, but a thousand-row dialog is
/// not a picker anymore. The item count shown notes the truncation.
pub(crate) const MAX_PLAYLIST_ITEMS: usize = 500;

/// True only when the collection held more items than the fetch cap:
/// lenient parsing can drop unusable entries too, so `total >
/// items.len()` alone is not evidence of truncation.
pub(crate) fn playlist_truncated(pl: &PlaylistInfo) -> bool {
    pl.total > pl.items.len() && pl.items.len() == MAX_PLAYLIST_ITEMS
}

/// Lenient i64 for extractor JSON (durations arrive as floats from some
/// extractors, e.g. fractional Instagram reel durations); floats
/// truncate toward zero, negatives included — callers clamp display.
fn json_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f.trunc() as i64))
}

/// Non-empty string field, borrowed.
fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Parse one flat-playlist entry into a queueable item. Entries are
/// usually stubs (`_type: "url"`), but fully-extracted video objects
/// pass through the same field reads. Nulls, non-objects and entries
/// without a usable page URL are dropped.
fn parse_playlist_item(entry: &serde_json::Value, position: usize) -> Option<PlaylistItem> {
    entry.as_object()?;
    let is_http = |u: &str| u.starts_with("http://") || u.starts_with("https://");
    // `webpage_url` is the canonical item page; bare `url` doubles as
    // one for extractors that only emit it (it is the video id on
    // YouTube, so the http check matters).
    let page_url = json_str(entry, "webpage_url")
        .filter(|u| is_http(u))
        .or_else(|| json_str(entry, "url").filter(|u| is_http(u)))?;
    let id = json_str(entry, "id")
        .map(str::to_string)
        .unwrap_or_else(|| format!("item-{}", position + 1));
    let title = json_str(entry, "title")
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    let index = entry
        .get("playlist_index")
        .and_then(json_i64)
        .and_then(|i| usize::try_from(i).ok())
        .filter(|&i| i > 0) // `playlist_index` is documented 1-based; 0 is bogus.
        .unwrap_or(position + 1);
    Some(PlaylistItem {
        index,
        id,
        title,
        page_url: page_url.to_string(),
        duration: entry.get("duration").and_then(json_i64),
    })
}

/// Parse playlist-shaped probe JSON (`_type: "playlist"` with an
/// `entries` array, as `--flat-playlist --dump-single-json` emits).
/// Returns `None` for single-video JSON so the caller falls through to
/// the video path.
fn parse_playlist_json(value: &serde_json::Value, url: &str) -> Option<PlaylistInfo> {
    let obj = value.as_object()?;
    let entries = obj.get("entries").and_then(|e| e.as_array())?;
    // A single video never carries `entries`; belt-and-braces in case an
    // extractor nests one anyway.
    if obj.get("_type").and_then(|t| t.as_str()) == Some("video") {
        return None;
    }
    let extractor_key = obj
        .get("extractor_key")
        .and_then(|k| k.as_str())
        .unwrap_or("");
    let total = entries.len();
    let items: Vec<PlaylistItem> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| parse_playlist_item(e, i))
        .take(MAX_PLAYLIST_ITEMS)
        .collect();
    Some(PlaylistInfo {
        id: json_str(value, "id").unwrap_or("").to_string(),
        title: json_str(value, "title").unwrap_or(url).to_string(),
        page_url: json_str(value, "webpage_url").unwrap_or(url).to_string(),
        kind: PlaylistKind::classify(url, extractor_key),
        total,
        items,
    })
}

/// Instagram's story extractor stamps every tray entry with the URL that
/// was pasted. For a single-story link (`/stories/<user>/<id>/`) that is
/// the *pasted story's* URL, not the entry's: a row queued from such an
/// entry would re-resolve — and download — the pasted story instead of
/// the picked one. Point story items at the owner's story tray instead;
/// the worker re-resolves the tray and selects the picked entry by id.
/// Highlights keep their URL: a highlight *is* the collection, and its
/// items are not addressable as live stories.
fn retarget_story_items(url: &str, playlist: &mut PlaylistInfo) {
    if playlist.kind != PlaylistKind::Stories {
        return;
    }
    let Some(tray) = story_tray_url(url) else {
        return;
    };
    for item in &mut playlist.items {
        item.page_url = tray.clone();
    }
}

/// Non-empty path segments of an Instagram page URL, if it parses as
/// http(s) on an Instagram host. Shared preamble for the story helpers
/// below; each keeps its own shape/charset validation. Pure.
fn instagram_path_segments(url: &str) -> Option<Vec<String>> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if !matches!(
        parsed.host_str(),
        Some("instagram.com") | Some("www.instagram.com")
    ) {
        return None;
    }
    Some(
        parsed
            .path_segments()?
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `https://www.instagram.com/stories/<user>/<story-id>/` → the owner's
/// story tray `https://www.instagram.com/stories/<user>/`. Returns `None`
/// for tray URLs (nothing to retarget), highlights, and anything that
/// does not parse as an Instagram story link.
fn story_tray_url(url: &str) -> Option<String> {
    let segments = instagram_path_segments(url)?;
    let [stories, user, story_id] = segments.as_slice() else {
        return None;
    };
    if stories != "stories" {
        return None;
    }
    if user.eq_ignore_ascii_case("highlights") {
        return None;
    }
    if !story_id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("https://www.instagram.com/stories/{user}/"))
}

/// Queue target for one playlist entry under worker expansion: a
/// directly-addressable page. Story segments resolve to their own pages
/// (same form the extractor matches); everything else keeps its listed
/// page. `None` for entries with no http page, or pointing back at the
/// probed collection itself — compared against both the row's normalized
/// URL and the extractor's canonical one, since yt-dlp canonicalizes
/// (`youtu.be`→`watch`, trailing slashes) past what intake normalized.
/// Highlights resolve to their own URL, so they expand to nothing and
/// keep today's collection error; a pathological nested playlist would
/// otherwise re-expand forever (accepted residual: real extractors
/// don't nest — entries are videos, and the 500-item cap bounds even
/// a hostile one per generation).
/// Pure for tests.
pub(crate) fn expand_child_target(
    parent_url: &str,
    playlist_url: &str,
    item: &PlaylistItem,
) -> Option<String> {
    if let Some(segment) = story_segment_url(parent_url, &item.id) {
        return Some(segment);
    }
    let page = item.page_url.as_str();
    if !is_http_url(page) || page == parent_url || page == playlist_url {
        return None;
    }
    Some(page.to_string())
}

/// Instagram story username from a tray (or single-story) URL:
/// `.../stories/<user>[/<id>/]`. `None` for highlights and non-story
/// links. Pure for tests.
fn story_tray_user(url: &str) -> Option<String> {
    let segments = instagram_path_segments(url)?;
    let [stories, user, ..] = segments.as_slice() else {
        return None;
    };
    if stories != "stories" {
        return None;
    }
    // Instagram usernames: 1–30 chars of letters, digits, periods and
    // underscores. Anything else is not a user tray — fail early to the
    // tray + entry-id fallback instead of queueing a late failure.
    if user.chars().count() > 30
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return None;
    }
    if user.eq_ignore_ascii_case("highlights") {
        return None;
    }
    Some((*user).to_string())
}

/// Instagram shortcode alphabet (`_id_to_pk` in yt-dlp's extractor):
/// standard base64 order with `-_`, no padding.
const INSTA_SHORTCODE_TABLE: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Numeric media id for an Instagram shortcode, mirroring yt-dlp's
/// `_id_to_pk` (trailing 28-char private-post suffix stripped first).
/// Operates on bytes throughout: the alphabet is ASCII-only, and a
/// byte-index cut on a `&str` could split a codepoint and panic on
/// untrusted extractor input. `None` on characters outside the
/// alphabet or overflow (falls back to tray + entry-id selection).
/// Pure for tests.
fn insta_shortcode_to_pk(shortcode: &str) -> Option<u64> {
    let bytes = shortcode.as_bytes();
    let code: &[u8] = if bytes.len() > 28 {
        &bytes[..bytes.len() - 28]
    } else {
        bytes
    };
    // Real shortcodes are ~11 chars; anything much longer is junk, not
    // a future format — fail closed to the tray fallback instead of
    // queueing a bogus segment page. (A longer valid format would only
    // lose the shortcut, never break: fallback is today's behavior.)
    if code.is_empty() || code.len() > 16 {
        return None;
    }
    let mut pk: u64 = 0;
    for &b in code {
        let digit = INSTA_SHORTCODE_TABLE.iter().position(|&t| t == b)? as u64;
        pk = pk.checked_mul(64)?.checked_add(digit)?;
    }
    // No real media id is zero (all-A input decodes to 0).
    if pk == 0 {
        return None;
    }
    Some(pk)
}

/// Directly-addressable page for one picked story segment:
/// `.../stories/<user>/<numeric-id>/`. Rows queued with this re-resolve
/// the segment itself as a single video — queuing the tray URL instead
/// downloads whatever the tray resolves to once per row (the first
/// segment, once per picked row: N copies of story one). Highlights
/// keep the tray (their items are not addressable as live stories), as
/// does anything unparseable (worker falls back to tray + entry-id
/// selection). Pure for tests.
pub(crate) fn story_segment_url(tray_url: &str, shortcode: &str) -> Option<String> {
    let user = story_tray_user(tray_url)?;
    let pk = insta_shortcode_to_pk(shortcode)?;
    Some(format!("https://www.instagram.com/stories/{user}/{pk}/"))
}

/// Fetch one page's raw `--dump-single-json` through a direct spawn
/// (same spawn/timeout/output semantics as the crate's extractors),
/// then parse leniently (see [`sanitize_video_json`]). Used instead of
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
    fetch_proxy: Option<&crate::download::ResolvedProxy>,
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
    fetch_proxy: Option<&crate::download::ResolvedProxy>,
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

/// Worker error when the page resolved playlist-shaped and no entry
/// could be selected: a routing bug when the row was never picked from
/// a playlist, an expired story when it was.
pub(crate) fn playlist_resolve_error(playlist_item_id: Option<&str>) -> VideoError {
    VideoError::fetch(gettext(if playlist_item_id.is_some() {
        "the story is no longer available"
    } else {
        "the link opened a collection, not a single video"
    }))
}

/// What one video worker attempt resolved to. The spawner needs more
/// than bytes-or-nothing: playlist-shaped pages expand into per-item
/// rows instead of failing.
#[derive(Debug)]
pub enum VideoOutcome {
    /// Bytes finished (or adopted) at dest.
    Finished(u64),
    /// The page resolved playlist-shaped with no picked entry: queue
    /// one row per item (see `expand_child_target`) instead of failing.
    Expand(PlaylistInfo),
    /// Stopped before finishing; the canceller owns the row state.
    Aborted,
}

/// What one extractor dump resolved to: a single video, or a
/// collection whose entries the picker (or worker expansion) consumes.
/// `parse_playlist_json` decides the shape; single videos never carry
/// `entries`.
#[derive(Debug)]
pub(crate) enum FetchedVideo {
    Single(Box<Video>),
    Playlist(PlaylistInfo),
}

/// The picked playlist entry, for rows queued from a picker whose page
/// re-resolves playlist-shaped (Instagram stories/highlights: every
/// entry carries the collection URL, so the row has no per-item page).
/// Matches on the entry id persisted at pick time; `None` when the row
/// was not picked from a playlist or the entry is gone.
fn pick_playlist_entry(
    value: &serde_json::Value,
    playlist_item_id: Option<&str>,
) -> Option<serde_json::Value> {
    let want = playlist_item_id?;
    value
        .get("entries")
        .and_then(|entries| entries.as_array())?
        .iter()
        .find(|entry| entry.get("id").and_then(|id| id.as_str()) == Some(want))
        .cloned()
}

/// Parse one video-shaped dump into the worker's [`Video`] model,
/// stamping formats with their video id.
fn parse_single_video(mut value: serde_json::Value) -> Result<Video, VideoError> {
    sanitize_video_json(&mut value);
    let mut video: Video = serde_json::from_value(value).map_err(VideoError::fetch)?;
    for format in &mut video.formats {
        format.video_id = Some(video.id.clone());
    }
    Ok(video)
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
    fetch_proxy: Option<crate::download::ResolvedProxy>,
) -> Result<ProbeResult, VideoError> {
    let handle = crate::download::tokio_rt().spawn(async move {
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

/// Best directly-fetchable video-only stream per height (codec rank,
/// then filesize). Plain HTTPS without DRM only; muxed and audio-only
/// streams never qualify. Pure.
fn best_direct_by_height(
    formats: &[Format],
    newest_first: bool,
) -> std::collections::HashMap<u32, &Format> {
    use std::collections::HashMap;
    let mut best: HashMap<u32, &Format> = HashMap::new();
    for f in formats {
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
                let cur_rank = codec_rank(
                    cur.codec_info.video_codec.as_deref().unwrap_or("none"),
                    newest_first,
                );
                let new_rank = codec_rank(vcodec, newest_first);
                (new_rank < cur_rank)
                    || (new_rank == cur_rank
                        && filesize_of(f).unwrap_or(0) > filesize_of(cur).unwrap_or(0))
            }
        };
        if replace {
            best.insert(h, f);
        }
    }
    best
}

/// Short transport/codec tag for a picker row: HLS variants show their
/// transport, not a codec that ffmpeg — not the engine — will consume.
/// Remote extractor strings are allowlisted to label-safe chars so bidi
/// overrides, newlines or oversized values can't spoof the dropdown.
/// Pure.
fn format_short_label(f: &Format) -> String {
    if f.protocol == Protocol::M3U8Native {
        return "HLS".to_string();
    }
    let raw = f.codec_info.video_codec.as_deref().unwrap_or("?");
    let clean: String = raw
        .split('.')
        .next()
        .unwrap_or("?")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '+')
        .take(16)
        .collect();
    if clean.is_empty() {
        "?".to_string()
    } else {
        clean
    }
}

/// Listable video-only formats for one video: best per height (codec
/// rank, then filesize), tallest first. Only directly fetchable streams qualify (plain HTTPS, no DRM);
/// HLS variants fill heights with no direct stream (the worker pulls
/// those via ffmpeg); muxed files stay on the automatic path, which
/// already adopts them. Audio-only formats never appear here.
pub fn video_format_options(video: &Video, newest_first: bool) -> Vec<VideoFormatOption> {
    let mut best = best_direct_by_height(&video.formats, newest_first);
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
            let short = format_short_label(f);
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

/// Codec priority modes for the `video-codec-priority` setting.
pub const CODEC_PRIORITY_NEWEST: &str = "newest";
pub const CODEC_PRIORITY_COMPATIBLE: &str = "compatible";
pub const CODEC_PRIORITY_VALUES: &[&str] = &[CODEC_PRIORITY_NEWEST, CODEC_PRIORITY_COMPATIBLE];

/// Translated combo labels, index-aligned with [`CODEC_PRIORITY_VALUES`].
pub fn codec_priority_labels() -> Vec<String> {
    vec![gettext("Newest first"), gettext("Most compatible")]
}

/// Combo index for a stored priority value. Unknown values fall back
/// to newest (the historical behavior).
pub fn codec_priority_index(value: &str) -> usize {
    combo_index(CODEC_PRIORITY_VALUES, value, 0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to newest.
pub fn codec_priority_value(index: usize) -> &'static str {
    combo_value(CODEC_PRIORITY_VALUES, index, CODEC_PRIORITY_NEWEST)
}

/// yt-dlp subtitle language codes offered in preferences, index-aligned
/// with [`subtitle_language_labels`]. `off` disables subtitle downloads.
pub(crate) const SUBTITLE_LANGUAGE_VALUES: &[&str] = &[
    "off", "en", "ar", "de", "es", "fr", "hi", "id", "it", "ja", "ko", "nl", "pl", "pt", "ru",
    "tr", "vi", "zh",
];

pub fn subtitle_language_labels() -> Vec<String> {
    vec![
        gettext("Off"),
        gettext("English"),
        gettext("Arabic"),
        gettext("German"),
        gettext("Spanish"),
        gettext("French"),
        gettext("Hindi"),
        gettext("Indonesian"),
        gettext("Italian"),
        gettext("Japanese"),
        gettext("Korean"),
        gettext("Dutch"),
        gettext("Polish"),
        gettext("Portuguese"),
        gettext("Russian"),
        gettext("Turkish"),
        gettext("Vietnamese"),
        gettext("Chinese"),
    ]
}

/// Combo index for a stored subtitle language code. Unknown or empty
/// values fall back to English (the default).
pub fn subtitle_language_index(value: &str) -> usize {
    combo_index(SUBTITLE_LANGUAGE_VALUES, value, 1)
}

/// Stored code for a combo index. Out-of-range indexes fall back to English.
pub fn subtitle_language_value(index: usize) -> &'static str {
    combo_value(SUBTITLE_LANGUAGE_VALUES, index, "en")
}

/// Active raw setting for one job: trimmed and lowercased, then
/// allowlisted against `values` minus `"off"`. `off`, empty and unknown
/// codes (hand-edited dconf) all resolve to `None`: a code yt-dlp would
/// only warn about is never requested, and the value reaching the CLI —
/// and any sidecar filename — always comes from the fixed list. Pure.
fn allowlisted_active(raw: &str, values: &[&str]) -> Option<String> {
    let norm = raw.trim().to_ascii_lowercase();
    values
        .iter()
        .find(|v| **v == norm && **v != "off")
        .map(|v| v.to_string())
}

/// Active subtitle language for one job: the raw setting trimmed and
/// lowercased, then allowlisted against [`SUBTITLE_LANGUAGE_VALUES`].
/// `off`, empty and unknown codes (hand-edited dconf) all resolve to
/// `None`: a code yt-dlp would only warn about is never requested, and
/// the value reaching `--sub-langs` — and the sidecar filename — always
/// comes from the fixed list.
pub(crate) fn subtitle_lang_active(raw: &str) -> Option<String> {
    allowlisted_active(raw, SUBTITLE_LANGUAGE_VALUES)
}

/// Offered subtitle languages that produce sidecars: the full list
/// minus the `off` marker (the prefs UI uses the full list). Single
/// source so writers and deleters cannot drift when codes change.
pub(crate) fn subtitle_content_languages() -> impl Iterator<Item = &'static str> {
    SUBTITLE_LANGUAGE_VALUES
        .iter()
        .copied()
        .filter(|l| *l != "off")
}

/// yt-dlp remux target containers offered in preferences, index-aligned
/// with [`remux_video_labels`]. `off` disables remuxing.
pub(crate) const REMUX_VIDEO_VALUES: &[&str] = &["off", "mp4", "mkv", "webm"];

pub fn remux_video_labels() -> Vec<String> {
    vec![
        gettext("Off"),
        // Container names are proper nouns: never translated.
        "MP4".to_string(),
        "MKV".to_string(),
        "WebM".to_string(),
    ]
}

/// Combo index for a stored remux target. Unknown or empty values fall
/// back to Off (the default).
pub fn remux_video_index(value: &str) -> usize {
    combo_index(REMUX_VIDEO_VALUES, value, 0)
}

/// Stored code for a combo index. Out-of-range indexes fall back to Off.
pub fn remux_video_value(index: usize) -> &'static str {
    combo_value(REMUX_VIDEO_VALUES, index, "off")
}

/// Active remux target for one job: the raw setting trimmed and
/// lowercased, then allowlisted against [`REMUX_VIDEO_VALUES`].
/// `off`, empty and unknown codes (hand-edited dconf) all resolve to
/// `None`: a code yt-dlp would only warn about is never requested, and
/// the value reaching `--remux-video` always comes from the fixed list.
pub(crate) fn remux_video_active(raw: &str) -> Option<String> {
    allowlisted_active(raw, REMUX_VIDEO_VALUES)
}

/// First search dir holding a complete ffmpeg toolchain (`ffmpeg` plus
/// `ffprobe`). yt-dlp resolves both tools from `--ffmpeg-location` and
/// never falls back to PATH for a missing sibling, so pointing it at a
/// dir with only `ffmpeg` (e.g. Grab's own user-lib install, which
/// shadows a perfectly good system install) breaks post-processing with
/// "ffprobe not found" even when the system ships both binaries.
fn toolchain_dir_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .find(|d| is_executable(&d.join("ffmpeg")) && is_executable(&d.join("ffprobe")))
        .cloned()
}

/// Directory form of a resolved tool binary for `--ffmpeg-location`
/// (yt-dlp wants the directory; Grab resolves the binary). Prefers a
/// dir with both ffmpeg and ffprobe; falls back to the binary's own dir
/// (the previous behavior) when no complete toolchain is on hand.
fn ffmpeg_location_dir(ffmpeg_bin: &Path) -> String {
    toolchain_dir_in(&tool_search_dirs())
        .or_else(|| ffmpeg_bin.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/usr/bin"))
        .to_string_lossy()
        .into_owned()
}

/// Best-effort subtitle sidecars — yt-dlp/Parabolic parity: exact
/// language with automatic-caption fallback, converted to SRT beside
/// the output. A missing language is only a warning upstream (verified
/// against yt-dlp 2026.08.19: `There are no subtitles for the
/// requested languages`, exit 0), and conversion points at Grab's
/// resolved ffmpeg (guaranteed present by `resolve_libraries`, which
/// refuses video attempts without it) — so subtitles can never sink a
/// download.
fn subtitle_cli_args(lang: &str) -> Vec<String> {
    vec![
        "--write-subs".to_string(),
        "--sub-langs".to_string(),
        lang.to_string(),
        "--write-auto-subs".to_string(),
        "--convert-subs".to_string(),
        "srt".to_string(),
    ]
}

/// Rank a codec string against an ordered table of prefix groups:
/// first matching group wins, anything else ranks past the table.
/// Powers both priority modes so the two orders can't drift apart
/// branch by branch. Pure.
fn codec_rank_in(vcodec: &str, table: &[&[&str]]) -> u8 {
    let c = vcodec.to_ascii_lowercase();
    table
        .iter()
        .position(|group| group.iter().any(|p| c.starts_with(p)))
        .map(|i| i as u8)
        .unwrap_or(table.len() as u8)
}

/// Newest-first codec rank, mirroring yt-dlp's `+vcodec:av01` sort:
/// AV1 wins ties at the same height, then VP9, HEVC, AVC1, anything
/// else. Older codecs are only dropped in favor of newer ones — never
/// at the cost of resolution, and never into an empty list.
const NEWEST_ORDER: &[&[&str]] = &[
    &["av01", "av1"],
    &["vp9"],
    &["hev1", "hvc1", "h265"],
    &["avc1", "h264"],
];

fn codec_rank_newest(vcodec: &str) -> u8 {
    codec_rank_in(vcodec, NEWEST_ORDER)
}

/// Compatibility-first rank for players without HEVC/AV1 decoders
/// (the common Linux gap): H.264 first, then VP9 (software-decoded
/// everywhere), HEVC, AV1, anything else.
const COMPATIBLE_ORDER: &[&[&str]] = &[
    &["avc1", "h264"],
    &["vp9"],
    &["hev1", "hvc1", "h265"],
    &["av01", "av1"],
];

fn codec_rank_compatible(vcodec: &str) -> u8 {
    codec_rank_in(vcodec, COMPATIBLE_ORDER)
}

/// Rank one codec under the stored priority mode.
pub(crate) fn codec_rank(vcodec: &str, newest_first: bool) -> u8 {
    if newest_first {
        codec_rank_newest(vcodec)
    } else {
        codec_rank_compatible(vcodec)
    }
}

/// Extractor codec preference matching the priority mode: the crate
/// falls back to all formats when the preferred codec is absent, so
/// this never fails a row by itself.
pub(crate) fn codec_preference(newest_first: bool) -> VideoCodecPreference {
    if newest_first {
        VideoCodecPreference::AV1
    } else {
        VideoCodecPreference::AVC1
    }
}
/// One HLS manifest variant selected by the planner: the exact
/// format id runners pin, plus its height for caps and logging.
#[derive(Debug, Clone)]
struct HlsSel {
    format_id: String,
    height: Option<u32>,
}

impl HlsSel {
    /// Build from an extractor format: manifest protocol, DRM-free,
    /// with a playlist URL. The URL itself is validated but not
    /// stored — runners re-resolve by id.
    fn from_format(f: &Format) -> Option<Self> {
        if f.protocol != Protocol::M3U8Native {
            return None;
        }
        if matches!(f.has_drm, Some(DrmStatus::Yes)) {
            return None;
        }
        f.download_info.url.clone().filter(|u| !u.is_empty())?;
        Some(Self {
            format_id: f.format_id.clone(),
            height: f.video_resolution.height.filter(|&h| h > 0),
        })
    }
}

/// Nearest stored quality bucket for an exact format height, so a
/// dropped dialog pin degrades to the picked height instead of the
/// global preference. Exact hits return themselves; anything between
/// buckets rounds to the closest, ties up; heights outside every
/// bucket clamp to the tallest/shortest. Every result is a recognized
/// [`VIDEO_QUALITY_VALUES`] entry (never "best").
pub fn quality_for_height(height: u32) -> &'static str {
    QUALITY_HEIGHTS
        .iter()
        .filter_map(|(v, h)| h.as_ref().map(|h| (*h, *v)))
        .min_by_key(|(b, _)| (b.abs_diff(height), std::cmp::Reverse(*b)))
        .map(|(_, v)| v)
        .unwrap_or("1080p")
}

/// Smallest height at or above the cap, else the tallest; `None`
/// takes the tallest. Shared by the muxed and HLS pickers over
/// pre-filtered (height, value) pairs — fetchability filtering stays
/// with the callers. Pure.
fn pick_at_or_above<T: Clone>(mut cands: Vec<(T, u32)>, want: Option<u32>) -> Option<T> {
    cands.sort_by_key(|(_, h)| *h);
    match want {
        Some(cap) => cands
            .iter()
            .find(|(_, h)| *h >= cap)
            .or_else(|| cands.last())
            .map(|(item, _)| item.clone()),
        None => cands.pop().map(|(item, _)| item),
    }
}

/// Best muxed (audio+video) file for a height cap: smallest height at
/// or above the cap, else the tallest available. Same semantics as
/// [`select_hls_format`]. Only directly fetchable files qualify, so a
/// DRM or link-less entry can never shadow a playable one — and the
/// quality cap survives: callers used to take the crate's
/// `best_audio_video_format`, which is first-in-extractor-order (lowest
/// first on x.com) regardless of the requested height.
fn select_muxed_format(formats: &[Format], want: Option<u32>) -> Option<StreamSel> {
    let cands: Vec<(StreamSel, u32)> = formats
        .iter()
        .filter(|f| f.format_type().is_audio_and_video())
        .filter_map(|f| {
            let h = f.video_resolution.height.filter(|&h| h > 0)?;
            StreamSel::from_format(f).ok().map(|sel| (sel, h))
        })
        .collect();
    pick_at_or_above(cands, want)
}

/// Height of one format id in fresh metadata, for comparing an
/// adopted single file against the HLS preset below.
fn format_height(formats: &[Format], id: &str) -> Option<u32> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(|f| f.video_resolution.height.filter(|&h| h > 0))
}
/// Default combo selection for a fresh resolve: index into `formats`
/// (tallest first) closest to the preference. `"best"` and empty
/// listings resolve to row 0; ties go taller, then earlier. Pure for
/// tests.
pub fn default_quality_index(formats: &[VideoFormatOption], quality: &str) -> usize {
    let want = match quality_height(quality) {
        None => return 0,
        Some(h) => h,
    };
    formats
        .iter()
        .enumerate()
        .min_by_key(|(i, opt)| (opt.height.abs_diff(want), std::cmp::Reverse(opt.height), *i))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// `--proxy` argv for one yt-dlp spawn. Empty when direct. Proxies are
/// unauthenticated (see `manual_proxy`): the URL never carries userinfo.
pub(crate) fn proxy_cli_args(proxy: Option<&crate::download::ResolvedProxy>) -> Vec<String> {
    match proxy.map(|p| p.cli_url.clone()) {
        Some(url) => vec!["--proxy".to_string(), url],
        None => Vec::new(),
    }
}

/// NO_PROXY env for one yt-dlp spawn. Set only when proxied; yt-dlp
/// honors it on a best-effort basis for the bypass list.
pub(crate) fn apply_proxy_env(
    cmd: &mut tokio::process::Command,
    proxy: Option<&crate::download::ResolvedProxy>,
) {
    if let Some(p) = proxy
        && !p.no_proxy_env.is_empty()
    {
        cmd.env("NO_PROXY", &p.no_proxy_env)
            .env("no_proxy", &p.no_proxy_env);
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

/// Canonical quality ladder: stored value to height cap (`None` = Best,
/// tallest available). Single source for the height cap and the
/// extractor selector — unknown values fall back to 1080p in both by
/// design (same fallback as the combo mapping). Pure.
const QUALITY_HEIGHTS: &[(&str, Option<u32>)] = &[
    ("best", None),
    ("2160p", Some(2160)),
    ("1440p", Some(1440)),
    ("1080p", Some(1080)),
    ("720p", Some(720)),
    ("480p", Some(480)),
];

/// Stored quality value to a height cap: `None` (Best) takes the
/// tallest variant available. Unknown values fall back to 1080p (same
/// fallback as the combo mapping and the extractor selector).
pub(crate) fn quality_height(value: &str) -> Option<u32> {
    QUALITY_HEIGHTS
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, h)| *h)
        .unwrap_or(Some(1080))
}

/// Best HLS variant for a height cap: smallest height at or above the
/// cap, else the tallest available. Mirrors the crate's
/// closest-at-or-above preset semantics.
fn select_hls_format(formats: &[Format], want: Option<u32>) -> Option<HlsSel> {
    // Extractor order is arbitrary: the shared picker sorts ascending
    // so the capped match is genuinely the smallest height at or above
    // it. Height-less variants sort as 0, matching the old inline order.
    let cands: Vec<(HlsSel, u32)> = formats
        .iter()
        .filter_map(HlsSel::from_format)
        .map(|s| {
            let h = s.height.unwrap_or(0);
            (s, h)
        })
        .collect();
    pick_at_or_above(cands, want)
}

/// Find one HLS variant by dialog-pinned id.
fn find_hls_format(formats: &[Format], id: &str) -> Option<HlsSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(HlsSel::from_format)
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

/// Planned streams for one attempt: direct splits, an adopted single
/// file, or an HLS variant. Pure over the extracted metadata, so unit
/// tests can pin the selection order without spawning tools.
struct StreamPlan {
    video_sel: Option<StreamSel>,
    audio_sel: Option<StreamSel>,
    hls_sel: Option<HlsSel>,
}

/// Resolve which streams an attempt fetches, in priority order:
///
/// 1. A dialog-pinned HLS id resolves first. [`find_usable_format`]
///    only accepts plain HTTPS, so without this the pin would be
///    dropped and then shadowed by the muxed adoption below (x.com
///    VODs: direct mp4s are muxed, so the combo lists HLS only).
/// 2. Direct splits: pinned HTTPS id, else the quality preset.
/// 3. Single-part adoption for a still-missing side: muxed files (at
///    the requested height, not first-in-extractor-order), then
///    Unclassified video containers (TikTok-style sparse extractors).
///    Gated on the missing side — not on audio absence — because those
///    pages also list a separate audio track, which used to skip both
///    fallbacks and keep only the music. Skipped once a pinned HLS
///    resolved, and when the request already holds both splits.
/// 4. The HLS preset stays a last resort for rows with no direct audio
///    (a muxed adoption above takes precedence when it found a file),
///    except when the preset is taller yet within the cap: Best match
///    means best across transports, not best direct file.
fn plan_streams(
    video: &Video,
    quality: &str,
    audio_only_request: bool,
    video_format_id: Option<&str>,
    newest_codecs: bool,
    item_id: u64,
) -> StreamPlan {
    use yt_dlp::VideoSelection as _;
    // Pins and presets resolve for audio-only rows too: with no direct
    // audio they feed the HLS extract path (or live capture) instead of
    // failing the row as unavailable.
    let pinned_hls: Option<HlsSel> =
        video_format_id.and_then(|id| find_hls_format(&video.formats, id));
    let mut video_sel: Option<StreamSel> = if audio_only_request {
        None
    } else if pinned_hls.is_some() {
        // Explicit HLS pick: no direct selection and no fallback
        // chatter — the HLS path below consumes the pin.
        None
    } else if let Some(pinned) = video_format_id {
        find_usable_format(&video.formats, pinned).or_else(|| {
            tracing::info!(
                item_id,
                pinned,
                "pinned video format gone, falling back to preset"
            );
            video
                .select_video_format(
                    selector_for_quality(quality),
                    codec_preference(newest_codecs),
                )
                .and_then(|f| StreamSel::from_format(f).ok())
        })
    } else {
        video
            .select_video_format(
                selector_for_quality(quality),
                codec_preference(newest_codecs),
            )
            .and_then(|f| StreamSel::from_format(f).ok())
    };
    let mut audio_sel: Option<StreamSel> =
        select_audio_original_first(&video.formats).and_then(|f| StreamSel::from_format(f).ok());
    // Muxed-only sources (one file, both tracks — archive.org, file
    // lockers): adopt the file directly instead of failing on the missing
    // split counterpart. A downloaded track beats a failed row; the
    // manifest records the effective single-part mode so retries agree.
    // Unclassified last resort (TikTok-style sparse extractors): both
    // codec fields missing leaves media typed Unknown — invisible to
    // every selector above. Only video-container extensions qualify,
    // so storyboards and manifests can never adopt here.
    let mut single_adopted = false;
    // Pre-adoption audio: restored if the HLS override below fires, so
    // a shadowed adoption never leaks its single-part mode into the
    // HLS path.
    let pre_audio_sel = audio_sel.clone();
    if pinned_hls.is_none()
        && (audio_sel.is_none() || video_sel.is_none())
        && (!audio_only_request || audio_sel.is_none())
    {
        let muxed = video_sel.take_if(|v| v.has_audio).or_else(|| {
            // Height-aware: a bare "first muxed file" pick is lowest
            // first on extractors that list ascending (x.com), ignoring
            // the requested quality entirely.
            select_muxed_format(&video.formats, quality_height(quality))
        });
        if let Some(m) = muxed {
            audio_sel = Some(m);
            single_adopted = true;
        }
    }
    if pinned_hls.is_none()
        && !audio_only_request
        && video_sel.is_none()
        && (audio_sel.is_none() || !single_adopted)
    {
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
        }
    }
    // HLS fallback (x.com VODs, live replays): nothing above is
    // directly fetchable, but manifest variants exist. A resolved pin
    // wins outright; otherwise the preset only runs when no direct
    // audio survived (a muxed adoption above takes precedence).
    // Best-overall override: when the adoption took a muxed file
    // shorter than an in-cap HLS variant (x.com direct files top out
    // below the tallest variant), the variant wins — Best match must
    // mean best across transports, not best direct file. Ties and
    // over-cap variants keep the direct file.
    let hls_preset = select_hls_format(&video.formats, quality_height(quality));
    let mut hls_wins = false;
    if !audio_only_request
        && pinned_hls.is_none()
        && single_adopted
        && let (Some(preset), Some(muxed_h)) = (
            hls_preset.as_ref(),
            audio_sel
                .as_ref()
                .and_then(|s| format_height(&video.formats, &s.format_id)),
        )
        && let Some(preset_h) = preset.height
        && preset_h > muxed_h
        && quality_height(quality).is_none_or(|cap| preset_h <= cap)
    {
        hls_wins = true;
        audio_sel = pre_audio_sel;
    }
    let hls_sel: Option<HlsSel> = pinned_hls.or_else(|| {
        if hls_wins || audio_sel.is_none() {
            hls_preset.clone()
        } else {
            None
        }
    });
    StreamPlan {
        video_sel,
        audio_sel,
        hls_sel,
    }
}

/// Map a stored quality value to the extractor selector. Unknown values
/// fall back to 1080p (same fallback as the combo mapping).
pub fn selector_for_quality(value: &str) -> VideoQuality {
    match quality_height(value) {
        None => VideoQuality::Best,
        Some(h) => VideoQuality::CustomHeight(h),
    }
}

/// One stream selected for download: owned values so the pipeline holds no
/// borrow on the extractor metadata across awaits.
#[derive(Debug, Clone)]
struct StreamSel {
    format_id: String,
    ext: String,
    url: String,
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
            size: filesize_of(f),
            has_audio: f
                .codec_info
                .audio_codec
                .as_deref()
                .is_some_and(|c| c != "none"),
        })
    }
}

/// Best direct audio track, original language first.
///
/// YouTube ships auto-dubbed audio as separate tracks (verified live:
/// dub tracks carry the dub `language`, "dubbed" in `format_note`, and
/// `language_preference` -1; the original carries 10). The crate's
/// `select_audio_format(Best, …)` ranks quality → bitrate → sample rate
/// → channels with no language awareness, so a higher-bitrate dub beats
/// the original. Rank `language_preference` (untagged → 0) above that
/// same order instead — matching upstream yt-dlp, whose default format
/// leads with `lang`. Extractors that don't tag score every track 0,
/// tying straight through to today's bitrate ranking unchanged. Only
/// directly fetchable tracks qualify (same gate as
/// [`StreamSel::from_format`]), so HLS/DRM audio still degrades to
/// absent and the HLS preset in [`plan_streams`] takes over. Pure for
/// tests.
fn select_audio_original_first(formats: &[Format]) -> Option<&Format> {
    formats
        .iter()
        .filter(|f| f.is_audio() && StreamSel::from_format(f).is_ok())
        .max_by(|a, b| {
            let (al, aq, ab, aa, ac) = audio_rank_key(a);
            let (bl, bq, bb, ba, bc) = audio_rank_key(b);
            al.cmp(&bl)
                .then_with(|| aq.total_cmp(&bq))
                .then_with(|| ab.total_cmp(&bb))
                .then_with(|| aa.cmp(&ba))
                .then_with(|| ac.cmp(&bc))
        })
}

/// Audio rank key in yt-dlp `lang`-first order (language, quality,
/// bitrate, sample rate, channels). Missing fields score neutral, so
/// untagged tracks fall back to bitrate instead of failing. The
/// comparator keeps `total_cmp` on the float lanes: NaN can never
/// appear (serde_json rejects non-finite numbers), but the ordering
/// stays total regardless. Pure.
fn audio_rank_key(f: &Format) -> (i64, f64, f64, i64, i64) {
    (
        f.language_preference.unwrap_or(0),
        f.quality_info.quality.map(|q| *q).unwrap_or(0.0),
        f.rates_info.audio_rate.map(|r| *r).unwrap_or(0.0),
        f.codec_info.asr.unwrap_or(0),
        f.codec_info.audio_channels.unwrap_or(0),
    )
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
    video_format_id: Option<String>,
    video_ext: String,
    audio_format_id: String,
    audio_ext: String,
    /// Final output size once the file has been renamed into place.
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
        video: Option<(&str, &str)>,
        audio: (&str, &str),
    ) -> bool {
        self.page_url == page_url
            && self.quality == quality
            && self.audio_format_id == audio.0
            && self.audio_ext == audio.1
            && match (video, &self.video_format_id) {
                (Some((id, ext)), Some(mid)) => id == mid && ext == self.video_ext,
                (None, None) => self.video_ext.is_empty(),
                _ => false,
            }
    }
}

/// Fixed part names inside a row's staging dir: retry-after-crash finds the
/// same paths the failed attempt wrote.
pub(crate) fn part_path(dir: &Path, kind: &str, ext: &str) -> PathBuf {
    dir.join(format!("{kind}.{ext}"))
}

/// Dest-dir part names (`<stem>.<kind>.<ext>` beside the finished file):
/// yt-dlp defaults — `.part` shells and fragments show up in the user's
/// folder while transferring, and yt-dlp's native `--continue` resumes
/// them in place. Deterministic across attempts (crash-resume finds the
/// same paths); unique per row via intake dedupe of the finished name.
///
/// This is a REAL filesystem path (single `%`, no template escapes):
/// callers feeding it to yt-dlp's `-o` must go through
/// [`ytdlp_output_template`], since yt-dlp parses `-o` as a template.
pub(crate) fn dest_part_path(dest: &Path, kind: &str, ext: &str) -> PathBuf {
    let dir = dest.parent().unwrap_or_else(|| Path::new(""));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    dir.join(format!("{stem}.{kind}.{ext}"))
}

/// Render a real filesystem path as a yt-dlp `-o` output template.
/// yt-dlp parses `-o` as a Python printf-style template, so a literal
/// `%` in the filename stem (percent-decoded titles, user-typed names
/// like `100%.mp4`) must be doubled (`%%`) or the template misparses
/// and the download fails. Genuine yt-dlp fields (`%(ext)s`, …​) are
/// left intact: only a `%` that does not start `%(name)s` is doubled.
/// yt-dlp renders `%%` back to a single `%`, so the on-disk name still
/// matches what `dest_part_path` and `is_grab_part` expect. (A user
/// title that itself contains `%(…)s` text passes through as a field:
/// inherent yt-dlp template ambiguity, pre-existing behavior.)
pub(crate) fn ytdlp_output_template(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '%' && !matches!(chars.clone().next(), Some('(')) {
            out.push('%');
        }
    }
    out
}

/// Grab-namespaced part infixes: the only names `clean_dest_parts` ever
/// touches. The finished file itself (`<stem>.<ext>`) never matches.
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live."];

/// Suffix of `file_name` past a `<stem>.` prefix, if it has one.
/// `strip_prefix` (not slicing past `starts_with`): panic-free even if a
/// future edit reorders the guards. Pure.
fn strip_stem_suffix<'a>(file_name: &'a str, stem: &str) -> Option<&'a str> {
    file_name
        .strip_prefix(stem)
        .and_then(|r| r.strip_prefix('.'))
}

fn is_grab_part(file_name: &str, stem: &str) -> bool {
    strip_stem_suffix(file_name, stem).is_some_and(|r| PART_KINDS.iter().any(|k| r.starts_with(k)))
}

/// File names directly inside `dir`: a best-effort snapshot for intake
/// reservation. Unreadable or missing dirs read as empty; per-entry IO
/// errors drop that entry (fail-open, same posture as the sweeper
/// below, so neither side can conjure a phantom delete).
pub(crate) fn dir_file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `stem` already hosts Grab-namespaced part files or subtitle
/// sidecars in a snapshotted listing (`<stem>.video.*` etc. plus
/// `<stem>.<lang>.srt` for offered languages, plus yt-dlp `.part`
/// shells of the part files). Intake treats such a stem as taken:
/// claiming it would let a later `clean_dest_parts` sweep, subtitle
/// collection, or row delete touch files Grab never wrote. Empty stems
/// never match. Directories reserve too (the sweeper only deletes
/// files): deliberately fail-closed, at most an extra ` (1)` in the
/// claimed name.
pub(crate) fn stem_reserved_in(names: &[String], stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    names.iter().any(|n| is_grab_part(n, stem)) || stem_has_subtitle_sidecar(names, stem)
}

/// Whether `stem` already hosts a subtitle sidecar for any offered
/// language (`<stem>.<lang>.srt`) in a snapshotted listing. Checked
/// alongside `stem_reserved_in` at intake so a pre-existing sidecar
/// reserves the stem too. Deliberately NOT folded into `is_grab_part`:
/// that matcher backs `clean_dest_parts`'s sweep, and sidecars must
/// survive row removal, not be swept with it.
fn stem_has_subtitle_sidecar(names: &[String], stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    names.iter().any(|n| {
        strip_stem_suffix(n, stem)
            .and_then(|r| r.strip_suffix(".srt"))
            .is_some_and(|lang| subtitle_content_languages().any(|l| l == lang))
    })
}

/// Delete a row's dest-dir part files (finished parts plus yt-dlp `.part`
/// shells). The finished file is never matched. Intake never claims a
/// stem that already hosts part-namespace files or subtitle sidecars
/// (see `stem_reserved_in`), so a match here is Grab's own output —
/// except for files that arrived mid-download, which no claim-time
/// check can cover.
pub fn clean_dest_parts(dest: &Path) {
    let (Some(dir), Some(stem)) = (dest.parent(), dest.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_grab_part(name, stem)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `<output-stem>.<lang>.srt` beside `output`: where yt-dlp drops a
/// `--write-subs` sidecar for a `-o` path, and where Grab keeps it
/// beside the finished file. The language is always allowlisted (no
/// dots, no separators), so this can never escape its directory — and
/// a finished `<stem>.<lang>.srt` can never match [`PART_KINDS`], so
/// `clean_dest_parts` structurally leaves collected sidecars alone
/// while still sweeping stale part-namespaced ones.
pub(crate) fn sidecar_path_for(output: &Path, lang: &str) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_string());
    output
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{stem}.{lang}.srt"))
}

/// Best-effort sidecar collection: move an exact sidecar file beside
/// the finished download. A missing source (the page published no
/// subtitles) is the normal nothing-to-do; any other failure only
/// traces — subtitles must never fail a download that succeeded.
fn collect_sidecar(src: &Path, dest: &Path, lang: &str) {
    if !src.exists() {
        return;
    }
    let dst = sidecar_path_for(dest, lang);
    // Never clobber: a foreign sidecar arriving mid-download (after the
    // intake snapshot) must survive. Ours stays beside the part file,
    // where row removal sweeps it. Collection runs at most once per row
    // (post-claim), so an existing dst is always foreign.
    if let Err(e) = crate::download::rename_noreplace(src, &dst) {
        tracing::warn!(
            src = %src.display(),
            dst = %dst.display(),
            error = %e,
            "subtitle sidecar left beside the part file"
        );
    }
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

/// Heuristic upper bound for a sane unified temp: the planned combined
/// total plus 10% for `filesize_approx` underestimates (the primary
/// wobble — the total sums extractor estimates, not measured bytes)
/// plus 8 MiB flat for fixed post-merge additions (`--embed-metadata`,
/// container overhead). The temp on disk is yt-dlp's merged output
/// while the total sums the planned part sizes, so an exact comparison
/// wipes valid crash-recovery temps whenever the estimate understates
/// reality, forcing full re-downloads.
///
/// Deliberately generous, and honest about it: the flat floor disables
/// garbage detection below ~8 MiB almost entirely, and anything under
/// this bound is trusted as final — yt-dlp treats an existing file at
/// or past its expected size as "already downloaded" (exit 0, verified
/// against yt-dlp 2026.08.19), and the post-run path claims non-empty
/// output without re-verifying size. The counterweight is staging
/// isolation: yt-dlp is the sole writer to the row's staging dir and
/// the manifest selection must match, so foreign bytes here mean
/// same-selection byte variance across attempts, not arbitrary garbage.
/// Past this bound, treat as garbage: wipe and start over. Pure
/// function (no I/O), unit-testable directly.
fn unified_temp_limit(total: u64) -> u64 {
    total
        .saturating_add(total / 10)
        .saturating_add(8 * 1024 * 1024)
}

/// What the next attempt should do, decided from the sidecar and disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Finished file already in place (adopt it).
    Finished,
    /// Temp incomplete or absent: spawn the same command and let yt-dlp
    /// resume its own `.part` shell (or download fresh when nothing is
    /// there). Safe because the manifest matched: same selection.
    Resume,
    /// (Re)download everything, wiping staging first.
    Fresh,
}

/// Inputs for [`resume_plan`], bundled so the signature stays small.
pub(crate) struct ResumeQuery<'a> {
    pub manifest: Option<&'a VideoManifest>,
    pub dest: &'a Path,
    pub staging: &'a Path,
    pub page_url: &'a str,
    pub quality: &'a str,
    pub video: Option<(&'a str, &'a str)>,
    pub audio: (&'a str, &'a str),
    /// Freshly selected combined total, for the over-long check. `None`
    /// means unknown: without a total, oversize is undetectable and any
    /// existing bytes are reusable.
    pub total: Option<u64>,
}

pub(crate) fn resume_plan(q: &ResumeQuery) -> ResumePlan {
    let Some(m) = q.manifest else {
        return ResumePlan::Fresh;
    };
    if !m.matches(q.page_url, q.quality, q.video, q.audio) {
        return ResumePlan::Fresh;
    }
    if let Some(final_bytes) = m.final_bytes
        && file_len(q.dest) == Some(final_bytes)
    {
        return ResumePlan::Finished;
    }
    // The unified temp (if any) must be sane: far past the total, treat
    // as garbage (nothing valid that far past it), and a sparse
    // full-size shell would resume as zeros. Either way, wipe and start
    // over. Anything else — partial temp, or nothing at all — spawns
    // the same command and lets yt-dlp resume or download fresh on its
    // own; note an in-margin temp is then trusted as final (see
    // `unified_temp_limit`), since yt-dlp skips files already at or
    // past the expected size. The limit carries headroom because the
    // temp is yt-dlp's merged output while the total sums extractor
    // estimates: estimate error, not container overhead, is the wobble
    // source, and an exact comparison wipes valid crash recovery
    // whenever the estimate understates reality.
    if let Some(temp) = discover_unified_output(q.staging, None) {
        let len = file_len(&temp);
        if q.total
            .is_some_and(|t| len.is_some_and(|n| n > unified_temp_limit(t)))
            || is_sparse_shell(&temp)
        {
            return ResumePlan::Fresh;
        }
    }
    ResumePlan::Resume
}

/// Inputs for one resolver-worker attempt. Built on the main thread in
/// `spawn_video`; everything the pipeline needs, nothing GTK.
#[derive(Debug, Clone)]
pub struct VideoJob {
    pub item_id: u64,
    pub page_url: String,
    /// yt-dlp id of the playlist entry this row was picked from, if any
    /// (see [`VideoChoices::playlist_item_id`]). The worker selects the
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
    pub proxy: Option<crate::download::ResolvedProxy>,
}

/// Progress reports are throttled to this many bytes between row updates:
/// per-chunk reports would churn the UI for no visible gain, but the bar
/// must still feel live on slow links (HIG: indeterminate-or-smooth,
/// never a frozen bar).
const PROGRESS_GRANULARITY: u64 = 16384;

/// Run one attempt: resolve → download parts → merge → rename into place.
/// Returns the final size, or `None` when aborted (the pauser/canceller
/// already set the row status; the caller sends nothing).
///
/// # Errors
/// Returns a display-ready [`VideoError`]; the caller reports it as Failed.
pub async fn run_video_download(
    mut job: VideoJob,
    mut abort: oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<VideoOutcome, VideoError> {
    use crate::download::EngineMsg;

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

/// Fallback `-f` spec when the selected id is unknown to yt-dlp's fresh
/// extract (ids can rotate between the dialog resolve and this
/// attempt): same height semantics as the planner, resolved inside the
/// binary. Audio legs stay audio (`ba/b` degrades to best-single only
/// when no audio track exists); adopted single files degrade to
/// best-single instead of drifting into a bare audio track.
fn part_fallback_spec(quality: &str, video_part: bool, prefer_audio: bool) -> String {
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

/// Whether a staging filename may be the unified download's claimed
/// output: under our template prefix, not a merge-fragment leftover,
/// and not a `.part` shell, subtitle sidecar, or yt-dlp metadata
/// dropping (same exclusion set as the HLS discoverer). Used for both
/// the `after_move` fast path and the scan fallback so they agree.
fn unified_candidate(file_name: &str) -> bool {
    let ext = Path::new(file_name).extension().and_then(|e| e.to_str());
    file_name.starts_with("grab-media.")
        && !is_ytdlp_fragment(file_name)
        && !matches!(ext, Some("part" | "srt" | "ytdl" | "temp" | "tmp" | "frag"))
}

/// yt-dlp's own merge temp names (`<stem>.f<id>.<ext>`) inside a `-o`
/// template dir: never the claimed output, even when a merge fails and
/// leaves them behind. Pure for tests.
fn is_ytdlp_fragment(file_name: &str) -> bool {
    let Some(dot_f) = file_name.find(".f") else {
        return false;
    };
    let after_f = &file_name[dot_f + 2..];
    let digits = after_f.len()
        - after_f
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    digits > 0 && after_f[digits..].starts_with('.')
}

/// Locate the unified download's output in staging: prefer yt-dlp's
/// `after_move:filepath` print (canonicalized, must stay in staging),
/// else scan for the `grab-media.*` temp. Both paths use
/// [`unified_candidate`], so merge-fragment leftovers, `.part` shells,
/// subtitle sidecars and metadata droppings are never claimed.
fn discover_unified_output(staging: &Path, after_move: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = after_move
        && let Ok(canonical) = std::fs::canonicalize(path)
        && canonical.starts_with(staging)
        && canonical.is_file()
        && let Some(name) = canonical.file_name().and_then(|n| n.to_str())
        && unified_candidate(name)
    {
        return Some(canonical);
    }
    std::fs::read_dir(staging)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(unified_candidate)
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
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
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
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
    match crate::download::rename_noreplace(&final_tmp, &job.dest) {
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
    proxy: Option<&crate::download::ResolvedProxy>,
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

/// yt-dlp `-f` spec for one HLS attempt over the page URL (yt-dlp
/// re-resolves and merges itself). `bv*` (any video, muxed included)
/// leads so direct muxed files win over lower splits; height caps
/// like the picker. A muxed pick may gain a redundant second audio
/// track via `+ba`, which players ignore — completeness beats purity.
fn hls_format_spec(quality: &str, pinned: Option<&str>) -> String {
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
fn live_remux_argv(ts_path: &Path, dest: &Path, audio_only: bool, with_bsf: bool) -> Vec<String> {
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

/// yt-dlp's exact stderr line when `--live-from-start` meets a stream
/// Pure decision for the live-edge fallback: a from-start attempt that
/// recorded nothing and wasn't stopped retries once from the live edge
/// instead of failing the row. Anything else — a mid-capture failure, a
/// stopped attempt, or the second attempt itself — keeps its outcome, so
/// partial recordings are never discarded and Stop is never overridden.
/// Pure for tests.
fn fallback_to_live_edge(
    is_live: bool,
    live_from_start: bool,
    aborted: bool,
    already_retried: bool,
) -> bool {
    is_live && live_from_start && !aborted && !already_retried
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
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
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
    match crate::download::rename_noreplace(&final_tmp, &job.dest) {
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

/// Machine-readable progress lines: `--progress-template` with stable
/// fields beats parsing human `[download]` prose (percent formats and
/// unit spellings drift between versions). `[Grab];`-prefixed so
/// `parse_ytdlp_after_move` (bare absolute paths only) never mistakes
/// one for a filepath.
pub(crate) const YTDLP_PROGRESS_TEMPLATE: &str = "[Grab];%(progress.status)s;%(progress.downloaded_bytes)s;%(progress.total_bytes)s;%(progress.total_bytes_estimate)s;%(progress.speed)s;%(progress.eta)s";

/// One parsed template line: absolute byte counts (never percents), so
/// callers accumulate instead of re-deriving. `total` already folds the
/// estimate fallback; `None` means unknown (live/unsized), not zero.
/// `speed`/`eta` are parsed and pinned by tests but not consumed — the
/// pump recomputes both from ticks — so they stay (they document the
/// line shape and cost nothing). `finished` marks a leg boundary:
/// yt-dlp prints one `status=finished` line per completed format, and
/// `downloaded_bytes` resets for the next leg.
#[derive(Debug, PartialEq)]
pub(crate) struct YtProgress {
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    pub speed: Option<f64>,
    pub eta: Option<u64>,
    pub finished: bool,
}

pub(crate) fn parse_ytdlp_template(line: &str) -> Option<YtProgress> {
    let rest = line.strip_prefix("[Grab];")?;
    let mut f = rest.split(';');
    let status = f.next()?;
    // "error" lines carry no usable counts; finished lines do.
    if status == "error" {
        return None;
    }
    let num = |s: Option<&str>| {
        s.filter(|v| *v != "NA")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let downloaded = num(f.next()).map(|v| v as u64);
    let total = num(f.next()).map(|v| v as u64);
    let estimate = num(f.next()).map(|v| v as u64);
    let speed = num(f.next());
    let eta = f
        .next()
        .filter(|v| *v != "NA" && *v != "Unknown")
        .and_then(|v| v.parse::<u64>().ok());
    Some(YtProgress {
        downloaded,
        total: total.or(estimate),
        speed,
        eta,
        finished: status == "finished",
    })
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

/// Whether a fresh total starts a new format leg (video→audio)
/// rather than HLS/DASH estimate wobble. Totals are re-estimated per
/// fragment while bytes climb monotonically, so wobble moves the total
/// alone; a new leg moves the total substantially (either direction —
/// the audio leg is usually much smaller, but need not be) AND resets
/// downloaded back near zero (legs download sequentially, so bytes are
/// high when the second leg starts). Unknown bytes count as reset: a
/// leg's first lines may carry no count yet. The first known total
/// always (re)inits. Pure for tests.
fn leg_changed(max_total: Option<u64>, max_dl: u64, total: u64, downloaded: Option<u64>) -> bool {
    if total == 0 {
        return false;
    }
    match max_total {
        None | Some(0) => true,
        Some(m) => {
            let total_moved = total < m / 2 || total > m.saturating_mul(2);
            total_moved && downloaded.is_none_or(|d| d <= max_dl / 2)
        }
    }
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

/// Trace yt-dlp's selected-format line (`[info] … Downloading N
/// format(s): …`) as it streams past: on success it names what
/// actually downloaded (audit our pick against yt-dlp's sort and
/// id aliasing); on failure the tail below still carries the error.
/// `pending` carries a line split across 4 KiB reads.
fn trace_format_lines(pending: &mut String, chunk: &[u8]) {
    pending.push_str(&String::from_utf8_lossy(chunk));
    while let Some(pos) = pending.find('\n') {
        let line: String = pending.drain(..=pos).collect();
        let line = line.trim_end();
        if is_format_selection_line(line) {
            tracing::info!("{line}");
        }
    }
}

/// Whether a yt-dlp stderr line announces the selected formats.
fn is_format_selection_line(line: &str) -> bool {
    line.contains("Downloading ") && line.contains("format(s)")
}

/// Last non-blank line of captured child output, for error detail.
/// `fallback` names the tool when the output carries nothing usable.
fn last_log_line(output: &str, fallback: &str) -> String {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(fallback)
        .trim()
        .to_string()
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

#[allow(clippy::too_many_arguments)]
async fn run_hls_ytdlp(
    youtube_bin: &Path,
    ffmpeg_bin: &Path,
    staging: &Path,
    job: &VideoJob,
    hls_format_id: &str,
    abort: oneshot::Receiver<()>,
    timeout: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
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
                        let len = crate::download::piece_len(t);
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
                    for idx in piece_marks(crate::download::piece_len(grid), &mut marked, leg_have)
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
    match crate::download::rename_noreplace(&final_tmp, &job.dest) {
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
