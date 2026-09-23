//! Quality ladder: stored values, height caps, picker labels and
//! filename defaults. Leaf module (media types + yt_dlp selector +
//! gettext): preferences, dialog and planner consume these through
//! the `video` facade.

use crate::video_types::VideoFormatOption;
use gettextrs::gettext;
use yt_dlp::model::selector::VideoQuality;

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

/// Translated ComboRow labels, index-aligned with [`VIDEO_QUALITY_VALUES`](crate::media_types::VIDEO_QUALITY_VALUES).
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

/// Nearest stored quality bucket for an exact format height, so a
/// dropped dialog pin degrades to the picked height instead of the
/// global preference. Exact hits return themselves; anything between
/// buckets rounds to the closest, ties up; heights outside every
/// bucket clamp to the tallest/shortest. Every result is a recognized
/// [`VIDEO_QUALITY_VALUES`](crate::media_types::VIDEO_QUALITY_VALUES) entry (never "best").
pub fn quality_for_height(height: u32) -> &'static str {
    QUALITY_HEIGHTS
        .iter()
        .filter_map(|(v, h)| h.as_ref().map(|h| (*h, *v)))
        .min_by_key(|(b, _)| (b.abs_diff(height), std::cmp::Reverse(*b)))
        .map(|(_, v)| v)
        .unwrap_or("1080p")
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

/// Map a stored quality value to the extractor selector. Unknown values
/// fall back to 1080p (same fallback as the combo mapping).
pub fn selector_for_quality(value: &str) -> VideoQuality {
    match quality_height(value) {
        None => VideoQuality::Best,
        Some(h) => VideoQuality::CustomHeight(h),
    }
}
