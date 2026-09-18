use super::*;
use crate::download::DownloadStatus;

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
