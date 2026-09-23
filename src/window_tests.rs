use super::*;
use crate::download::DownloadStatus;
use crate::media_types::PlaylistKind;
use crate::window_rows::should_pulse;

#[test]
fn should_pulse_covers_row_states() {
    use DownloadStatus::*;
    // Resolving: active with no fraction — the reported hang.
    assert!(should_pulse(Downloading, false, 0.0));
    // Determinate transfer renders fractions, never pulses.
    assert!(!should_pulse(Downloading, false, 0.5));
    assert!(!should_pulse(Downloading, false, 1.0));
    // Live captures pulse for the whole capture regardless of fraction.
    assert!(should_pulse(Downloading, true, 0.0));
    assert!(should_pulse(Downloading, true, 0.9));
    // Anything not actively downloading stays frozen: queued, paused,
    // and terminal rows must not animate.
    assert!(!should_pulse(Queued, false, 0.0));
    assert!(!should_pulse(Paused, false, 0.0));
    assert!(!should_pulse(Paused, true, 0.0));
    assert!(!should_pulse(Done, true, 0.0));
    assert!(!should_pulse(Failed, false, 0.0));
    assert!(!should_pulse(Cancelled, false, 0.0));
}

#[test]
fn should_pulse_locks_non_finite_and_negative_edges() {
    use DownloadStatus::*;
    // Negative fractions still read as "no fraction" — pulses.
    assert!(should_pulse(Downloading, false, -0.5));
    // NaN never satisfies `<= 0.0`, so it renders determinate (no pulse).
    assert!(!should_pulse(Downloading, false, f64::NAN));
    // Infinite progress is not "no fraction" — no pulse.
    assert!(!should_pulse(Downloading, false, f64::INFINITY));
    // Non-active rows never pulse regardless of fraction.
    assert!(!should_pulse(Queued, true, 0.0));
    assert!(!should_pulse(Done, false, f64::NAN));
}

#[test]
fn fmt_item_duration_covers_minute_hour_boundaries() {
    assert_eq!(fmt_item_duration(0), "0:00");
    assert_eq!(fmt_item_duration(59), "0:59");
    assert_eq!(fmt_item_duration(60), "1:00");
    assert_eq!(fmt_item_duration(61), "1:01");
    assert_eq!(fmt_item_duration(3599), "59:59");
    assert_eq!(fmt_item_duration(3600), "1:00:00");
    assert_eq!(fmt_item_duration(3661), "1:01:01");
}

#[test]
fn fmt_item_duration_clamps_negative_to_zero() {
    assert_eq!(fmt_item_duration(-1), "0:00");
    assert_eq!(fmt_item_duration(i64::MIN), "0:00");
}

#[test]
fn playlist_count_label_covers_kinds_singular_plural() {
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 1), "1 story");
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 3), "3 stories");
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 1),
        "1 highlight"
    );
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 2),
        "2 highlights"
    );
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 1), "1 item");
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 5), "5 items");
}

#[test]
fn playlist_count_label_zero_uses_plural() {
    assert_eq!(playlist_count_label(PlaylistKind::Stories, 0), "0 stories");
    assert_eq!(
        playlist_count_label(PlaylistKind::Highlights, 0),
        "0 highlights"
    );
    assert_eq!(playlist_count_label(PlaylistKind::Playlist, 0), "0 items");
}
