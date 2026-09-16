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
//!   [`install_libraries`]. Callers detect the gap with
//!   [`resolve_libraries`] and offer an install action (AdwBanner).
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
use yt_dlp::model::format::{Format, HttpHeaders, Protocol};
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
        Self::MissingLibraries(gettext("Video downloads need the yt-dlp support tools"))
    }
    fn fetch(e: impl std::fmt::Display) -> Self {
        Self::Fetch(
            gettext("Couldn't read the video page: {detail}").replace("{detail}", &e.to_string()),
        )
    }
    fn install(e: impl std::fmt::Display) -> Self {
        Self::Message(
            gettext("Couldn't install the video support tools: {detail}")
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
        Self::Message(gettext("No suitable formats found for this video"))
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
    /// The exact [`crate::download::DEST_EXISTS`] sentence, so the pump's
    /// foreign-file requeue path picks a fresh name and retries the merge.
    fn exists() -> Self {
        Self::Message(crate::download::DEST_EXISTS.to_string())
    }
}

/// Extraction result, kept deliberately small: the queue row needs the
/// title/thumbnail/duration, and the *page URL* for expiry-safe re-resolve.
#[derive(Clone, Debug)]
pub struct VideoInfo {
    /// Extractor video id (not persisted; informational).
    pub id: String,
    pub title: String,
    pub thumbnail: Option<String>,
    /// Duration in seconds.
    pub duration: Option<i64>,
    /// Preformatted duration from the extractor (e.g. "41:21").
    pub duration_string: Option<String>,
    /// Canonical page URL — the identity persisted across restarts.
    pub page_url: String,
    /// Unix time after which every resolved format URL is stale, derived
    /// from the youngest `available_at` across formats.
    pub expires_at: Option<i64>,
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
            thumbnail: v.thumbnail.clone(),
            duration: v.duration,
            duration_string: v.duration_string.clone(),
            page_url,
            expires_at,
        }
    }
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

/// Candidate directories for the tools, in priority order: the Flatpak
/// bundle, Grab's user library dir, then PATH (which inside Flatpak
/// includes the runtime's /usr/bin where ffmpeg ships).
fn tool_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/app/bin"), user_lib_dir()];
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

/// Install yt-dlp + ffmpeg into the user library dir (tarball/dev builds).
/// Runs on Grab's Tokio runtime regardless of the calling thread; await
/// from a spawned task — never block the GTK thread on it.
pub async fn install_libraries() -> Result<Libraries, VideoError> {
    let dir = user_lib_dir();
    let handle = crate::download::tokio_rt().spawn(async move {
        let installer = LibraryInstaller::new(dir);
        let (youtube, ffmpeg) = tokio::join!(
            installer.install_youtube(None),
            installer.install_ffmpeg(None)
        );
        Ok::<_, yt_dlp::error::Error>(Libraries::new(youtube?, ffmpeg?))
    });
    match handle.await {
        Ok(Ok(libs)) => Ok(libs),
        Ok(Err(e)) => Err(VideoError::install(&e)),
        Err(e) => Err(VideoError::runtime(&e)),
    }
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

/// Extract metadata for one video page. The media URLs inside the returned
/// [`VideoInfo`] are only passed on to the download step; the *page URL* is
/// what survives restarts.
pub async fn fetch_video_infos(libs: Libraries, url: String) -> Result<VideoInfo, VideoError> {
    let handle = crate::download::tokio_rt().spawn(async move {
        let out = staging_root();
        std::fs::create_dir_all(&out).map_err(VideoError::staging)?;
        let downloader = Downloader::builder(libs, out)
            .build()
            .await
            .map_err(VideoError::fetch)?;
        let video = downloader
            .fetch_video_infos(&url)
            .await
            .map_err(VideoError::fetch)?;
        Ok::<_, VideoError>(VideoInfo::from(&video, &url))
    });
    match handle.await {
        Ok(r) => r,
        Err(e) => Err(VideoError::runtime(&e)),
    }
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
    /// forces a re-download instead of a corrupt merge.
    fn parts_present(&self, dir: &Path) -> bool {
        let audio_ok =
            file_len(&part_path(dir, "audio", &self.audio_ext)) == Some(self.audio_bytes);
        let video_ok = match &self.video_format_id {
            Some(_) => {
                file_len(&part_path(dir, "video", &self.video_ext)) == Some(self.video_bytes)
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

/// What the next attempt should do, decided from the sidecar and disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Finished file already in place (adopt it).
    Finished,
    /// Parts verified on disk (merge only).
    CombineOnly,
    /// (Re)download everything.
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
        ResumePlan::CombineOnly
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
    abort: oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::download::EngineMsg>,
) -> Result<Option<u64>, VideoError> {
    use crate::download::EngineMsg;
    use yt_dlp::VideoSelection as _;

    let staging = staging_dir(job.item_id);
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(VideoError::staging)?;
    let libs = resolve_libraries()?;
    // The downloader timeout covers extractor calls AND the ffmpeg merge:
    // a full-length merge on a slow CPU dwarfs any network timeout, so
    // never go below the crate default (the user's setting extends it).
    let timeout = Duration::from_secs(job.timeout_secs.max(300));
    let mut builder = Downloader::builder(libs, staging.clone()).with_timeout(timeout);
    if !job.user_agent.is_empty() {
        builder = builder.with_user_agent(job.user_agent.clone());
    }
    let downloader = builder.build().await.map_err(VideoError::fetch)?;
    let phase = |text: String| {
        tx.send(EngineMsg::Phase(text)).ok();
    };

    // Resolve (with retries): the extractor cache already drops expired
    // format URLs, so a cached hit is safe to download from.
    phase(gettext("Resolving video…"));
    let mut video: Option<Video> = None;
    for attempt in 0..job.tries.max(1) {
        match downloader.fetch_video_infos(&job.page_url).await {
            Ok(v) => {
                video = Some(v);
                break;
            }
            Err(e) if attempt + 1 < job.tries.max(1) => {
                tracing::debug!("video resolve failed, retrying: {e}");
                tokio::time::sleep(Duration::from_secs(u64::from(attempt) + 1)).await;
            }
            Err(e) => return Err(VideoError::fetch(&e)),
        }
    }
    let Some(video) = video else {
        return Err(VideoError::fetch("empty response"));
    };

    // Select streams: AVC1 default for compat (VP9/AV1 need no merge but
    // play back on fewer targets), best audio. Rejections (HLS/DRM/missing
    // URL) degrade candidates to absent here; the plan below decides
    // between split, single-file and audio-only from what's fetchable.
    let mut video_sel: Option<StreamSel> = (!job.audio_only)
        .then(|| {
            video.select_video_format(
                selector_for_quality(&job.quality),
                VideoCodecPreference::AVC1,
            )
        })
        .flatten()
        .and_then(|f| StreamSel::from_format(f).ok());
    let mut audio_sel: Option<StreamSel> = video
        .select_audio_format(AudioQuality::Best, AudioCodecPreference::Any)
        .and_then(|f| StreamSel::from_format(f).ok());
    // Muxed-only sources (one file, both tracks — archive.org, file
    // lockers): adopt the file directly instead of failing on the missing
    // split counterpart. A downloaded track beats a failed row; the
    // manifest records the effective single-part mode so retries agree.
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
    let Some(audio_sel) = audio_sel else {
        return Err(VideoError::unavailable());
    };

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
    };
    let plan = resume_plan(&query);
    match plan {
        ResumePlan::Finished => return Ok(file_len(&job.dest)),
        ResumePlan::Fresh => {
            // Wipe the staging dir, not just known names: a previous
            // attempt's detached writers (pause winning the abort race) may
            // still hold the old inodes, so unlink first — they write
            // nowhere visible afterwards.
            let _ = tokio::fs::remove_dir_all(&staging).await;
            tokio::fs::create_dir_all(&staging)
                .await
                .map_err(VideoError::staging)?;
        }
        ResumePlan::CombineOnly => {}
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
        phase(gettext("Merging…"));
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
    match crate::download::rename_noreplace(&final_tmp, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VideoError::exists());
        }
        Err(e) => return Err(VideoError::staging(&e)),
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
