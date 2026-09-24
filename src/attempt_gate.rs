//! Decides whether one download attempt may deliver its result.
//!
//! Leaf module (std only). The invariant this file exists to protect: for
//! one attempt with a destination, a row removal, and a delivery step, it
//! must never be the case that the removal happens-before the delivery and
//! the delivered file survives.
//!
//! That is enforced with a compare-and-swap rather than a check, because a
//! check-then-act leaves a window between deciding and acting that the other
//! party can slip into. Whichever CAS wins *is* the linearization point.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// States an attempt can be in. `Committing` means delivery has been claimed
/// and is now irreversible; `Discarded` means the row is gone.
const ACTIVE: u8 = 0;
const COMMITTING: u8 = 1;
const DISCARDED: u8 = 2;

/// The delivery decision for one attempt. Cloneable and thread-safe: the
/// manager holds one handle and the worker holds another.
#[derive(Debug)]
pub struct AttemptGate {
    state: AtomicU8,
    delivered: AtomicBool,
}

impl AttemptGate {
    /// A gate in the `Active` state, ready to be shared.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            state: AtomicU8::new(ACTIVE),
            delivered: AtomicBool::new(false),
        })
    }

    /// Claim the right to deliver. `false` means a discard already won and
    /// the caller must not place a file.
    pub fn try_commit(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Claim the row as gone. `true` means the caller won the decision;
    /// `false` means the commit had already claimed delivery.
    pub fn discard(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, DISCARDED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Record that the attempt actually placed its file. Read by the
    /// finalizer to decide whether there is an orphan to remove.
    pub fn mark_delivered(&self) {
        self.delivered.store(true, Ordering::Release);
    }

    /// Whether this attempt placed a file at the destination.
    pub fn was_delivered(&self) -> bool {
        self.delivered.load(Ordering::Acquire)
    }

    /// Whether the row was claimed as gone before any commit.
    pub fn is_discarded(&self) -> bool {
        self.state.load(Ordering::Acquire) == DISCARDED
    }
}

#[cfg(test)]
#[path = "attempt_gate_tests.rs"]
mod tests;
