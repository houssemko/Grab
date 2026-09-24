//! Decides whether one download attempt may deliver its result.
//!
//! Leaf module (std only). This type arbitrates the delivery decision for
//! one attempt; it does not remove a row or clean up a file. A caller may
//! attempt delivery only after `try_commit()` returns `true`, must call
//! `mark_delivered()` only after a successful delivery, and must read
//! `was_delivered()` only after the worker has completed. The latter may
//! legitimately read `false` before then.
//!
//! The decision is enforced with a compare-and-swap rather than a check,
//! because a check-then-act leaves a window between deciding and acting that
//! the other party can slip into. Whichever CAS wins *is* the linearization
//! point.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// States an attempt can be in. `Committing` means delivery has been claimed
/// and the decision is now irreversible; `Discarded` means the discard
/// decision has been claimed. The caller performs the external action.
const ACTIVE: u8 = 0;
const COMMITTING: u8 = 1;
const DISCARDED: u8 = 2;

/// The delivery decision for one attempt. The manager and worker share it
/// through cloneable `Arc` handles.
#[derive(Debug)]
pub struct AttemptGate {
    state: AtomicU8,
    delivered: AtomicBool,
}

impl AttemptGate {
    /// Creates a gate in the `ACTIVE` state and returns its shareable handle.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            state: AtomicU8::new(ACTIVE),
            delivered: AtomicBool::new(false),
        })
    }

    /// Attempts to claim the right to deliver.
    ///
    /// Returns `true` only when this caller wins the `ACTIVE` to
    /// `COMMITTING` transition. If it returns `false`, this caller must not
    /// deliver; another commit or discard already won.
    #[must_use]
    pub fn try_commit(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Attempts to claim the discard decision for the row.
    ///
    /// Returns `true` only when this caller wins the `ACTIVE` to
    /// `DISCARDED` transition. If it returns `false`, this call did not
    /// claim discard; another commit or discard already won. Use
    /// `is_discarded()` when that distinction matters.
    #[must_use]
    pub fn discard(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, DISCARDED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Records that the worker successfully placed the file. Call only
    /// after a successful delivery.
    pub fn mark_delivered(&self) {
        self.delivered.store(true, Ordering::Release);
    }

    /// Whether the worker has recorded a successful delivery.
    ///
    /// This may legitimately be `false` before the worker completes; read
    /// it only after the worker has completed.
    pub fn was_delivered(&self) -> bool {
        self.delivered.load(Ordering::Acquire)
    }

    /// Whether the current decision state is `DISCARDED`.
    pub fn is_discarded(&self) -> bool {
        self.state.load(Ordering::Acquire) == DISCARDED
    }
}

#[cfg(test)]
#[path = "attempt_gate_tests.rs"]
mod tests;
