//! Media identity types: what a to-be-downloaded resource is, not how it
//! downloads. Leaf module (no crate deps) breaking the
//! `download ↔ video` import cycle: the queue persists these, the dialog
//! picks them, the worker resolves them.

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
    pub(crate) fn classify(page_url: &str, extractor_key: &str) -> Self {
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
/// [`VideoInfo`](crate::video::VideoInfo): each queued row re-resolves its own item page at
/// download time, so the picker only needs identity + label.
#[derive(Clone, Debug)]
pub struct PlaylistItem {
    /// 1-based position (`playlist_index` when the extractor reports it).
    /// Part of the extractor data contract (asserted in tests), not read
    /// by the picker, which addresses entries by position.
    #[allow(dead_code)]
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
    /// Extractor collection id. Part of the probe data contract; the
    /// picker addresses entries by position instead.
    #[allow(dead_code)]
    pub id: String,
    pub title: String,
    /// The collection URL that was probed.
    pub page_url: String,
    pub kind: PlaylistKind,
    /// Entries reported by the extractor, before the picker cap.
    pub total: usize,
    pub items: Vec<PlaylistItem>,
}
