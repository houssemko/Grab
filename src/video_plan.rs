//! Stream planning: which video/audio/HLS streams an attempt
//! fetches, in priority order. Leaf module (video_types +
//! video_quality + yt_dlp model): the runner consumes the plan,
//! tests cover the pickers directly.

use crate::video_quality::{quality_height, selector_for_quality};
use crate::video_tools::VideoError;
use crate::video_types::{HlsSel, codec_preference, filesize_of};
use yt_dlp::model::format::{Extension, Format, FormatType, Protocol};
use yt_dlp::model::{DrmStatus, Video};

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

/// Best HLS variant for a height cap: smallest height at or above the
/// cap, else the tallest available. Mirrors the crate's
/// closest-at-or-above preset semantics.
pub(crate) fn select_hls_format(formats: &[Format], want: Option<u32>) -> Option<HlsSel> {
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
pub(crate) fn find_hls_format(formats: &[Format], id: &str) -> Option<HlsSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(HlsSel::from_format)
}

/// Find one format by id, accepting only what the pipeline can fetch.
/// `None` covers unknown ids and HLS/DRM/missing-URL formats alike: the
/// caller falls back to the quality preset.
pub(crate) fn find_usable_format(formats: &[Format], id: &str) -> Option<StreamSel> {
    formats
        .iter()
        .find(|f| f.format_id == id)
        .and_then(|f| StreamSel::from_format(f).ok())
}

/// Planned streams for one attempt: direct splits, an adopted single
/// file, or an HLS variant. Pure over the extracted metadata, so unit
/// tests can pin the selection order without spawning tools.
pub(crate) struct StreamPlan {
    pub(crate) video_sel: Option<StreamSel>,
    pub(crate) audio_sel: Option<StreamSel>,
    pub(crate) hls_sel: Option<HlsSel>,
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
pub(crate) fn plan_streams(
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

/// One stream selected for download: owned values so the pipeline holds no
/// borrow on the extractor metadata across awaits.
#[derive(Debug, Clone)]
pub(crate) struct StreamSel {
    pub(crate) format_id: String,
    pub(crate) ext: String,
    /// Direct fetch URL. Written at plan time; the runners address
    /// splits by format id today, so only tests read this.
    #[allow(dead_code)]
    pub(crate) url: String,
    pub(crate) size: Option<u64>,
    /// Whether the stream carries an audio track (muxed files do).
    pub(crate) has_audio: bool,
}

impl StreamSel {
    /// Build from an extractor format, accepting only what the pipeline
    /// can actually fetch: plain-HTTPS, DRM-free streams with a URL.
    /// Anything else (HLS manifests, encrypted formats) is a clean
    /// unavailable error — never playlist bytes merged as media, never
    /// encrypted garbage saved as a finished file.
    pub(crate) fn from_format(f: &Format) -> Result<Self, VideoError> {
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
pub(crate) fn select_audio_original_first(formats: &[Format]) -> Option<&Format> {
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
