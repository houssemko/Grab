//! Queue persistence model: row status, the serde queue file
//! (versioned, backward compatible) and its stored items. Leaf
//! module (gettext + download_fetch bitmap + media types): the
//! manager persists/restores through the `download` facade.

use crate::download_fetch::SegmentState;
use gettextrs::gettext;
use gtk4::glib;

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

pub(crate) const QUEUE_VERSION: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredItem {
    pub(crate) url: String,
    pub(crate) dest_dir: String,
    pub(crate) filename: String,
    pub(crate) status: DownloadStatus,
    #[serde(default)]
    pub(crate) progress: f64,
    /// Completed 1 MB pieces for segmented resume across restarts (v2+).
    /// Absent on v1 files and for items that need no resume.
    #[serde(default)]
    pub(crate) segments: Option<SegmentState>,
    /// Intake file selection for multi-file torrents (v2+). The live map
    /// is in-memory only, so the selection is persisted here and
    /// re-staged on restore — otherwise a restart drops the filter and
    /// the resume downloads every file.
    #[serde(default)]
    pub(crate) selected_files: Option<Vec<usize>>,
    /// Recorded engine output folder for torrents (v2+). Absent on old
    /// files and for items that need no folder tracking.
    #[serde(default)]
    pub(crate) output_dir: Option<String>,
    /// Video-page source for yt-dlp items: only `Some(Page)` is ever
    /// written (plain downloads omit it, so old files stay clean and old
    /// app versions keep reading new ones).
    #[serde(default)]
    pub(crate) video_source: Option<crate::media_types::VideoSource>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredQueue {
    pub(crate) version: u32,
    pub(crate) items: Vec<StoredItem>,
}
