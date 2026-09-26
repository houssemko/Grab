//! Quality ladder: stored values, height caps, picker labels and filename
//! defaults. Leaf module (media types + yt_dlp selector + gettext).

use crate::video_types::VideoFormatOption;
use gettextrs::gettext;
use yt_dlp::model::selector::VideoQuality;

/// Default file name for a resolved video: the title and the container the
/// worker will produce.
///
/// Deliberately diverges from yt-dlp's `%(title)s [%(id)s].%(ext)s`: the id
/// rides in the file's `comment` tag instead, and Grab builds the name here
/// (not via `-o`) because dedupe, rename claims and resume all need the final
/// name up front. The extension is the remux target, lowercased here so the
/// contract holds whatever the caller passes; audio-only ignores remux.
pub fn default_video_filename(title: &str, audio_only: bool, remux_ext: Option<&str>) -> String {
    let ext = if audio_only {
        "m4a"
    } else {
        remux_ext.unwrap_or("mp4")
    };
    format!("{title}.{ext}")
}

/// Combo row labels, index-aligned with
/// [`VIDEO_QUALITY_VALUES`](crate::media_types::VIDEO_QUALITY_VALUES) and
/// shared by Preferences and the New Download dialog.
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

/// Nearest stored quality bucket for an exact format height, so a dropped
/// dialog pin degrades to the picked height instead of the global preference.
/// Ties go up, out-of-range heights clamp. Never returns "best".
pub fn quality_for_height(height: u32) -> &'static str {
    QUALITY_HEIGHTS
        .iter()
        .filter_map(|(v, h)| h.as_ref().map(|h| (*h, *v)))
        .min_by_key(|(b, _)| (b.abs_diff(height), std::cmp::Reverse(*b)))
        .map(|(_, v)| v)
        .unwrap_or("1080p")
}

/// Default combo selection for a fresh resolve: index into `formats`
/// (tallest first) closest to the preference. `"best"` and empty listings
/// resolve to row 0; ties go taller, then earlier.
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
/// tallest available). Single source for the height cap and the extractor
/// selector — unknown values fall back to 1080p in both by design.
const QUALITY_HEIGHTS: &[(&str, Option<u32>)] = &[
    ("best", None),
    ("2160p", Some(2160)),
    ("1440p", Some(1440)),
    ("1080p", Some(1080)),
    ("720p", Some(720)),
    ("480p", Some(480)),
];

/// Stored quality value to a height cap: `None` (Best) takes the tallest
/// variant available. Unknown values fall back to 1080p.
pub(crate) fn quality_height(value: &str) -> Option<u32> {
    QUALITY_HEIGHTS
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, h)| *h)
        .unwrap_or(Some(1080))
}

/// Map a stored quality value to the extractor selector.
pub fn selector_for_quality(value: &str) -> VideoQuality {
    match quality_height(value) {
        None => VideoQuality::Best,
        Some(h) => VideoQuality::CustomHeight(h),
    }
}
