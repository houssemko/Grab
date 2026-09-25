# Attempt Commit Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Removing a row mid-download must never leave a delivered file with no row behind it, and must reclaim that row's scratch only once nothing can still be writing it.

**Architecture:** A per-attempt `AttemptGate` decides delivery with a compare-and-swap, so exactly one of {deliver, discard} wins with no window between deciding and acting. The manager claims the row as gone on the same gate, then hands the worker handle to a finalizer that awaits it and reclaims; if the commit had already won, the finalizer removes the orphan the attempt placed. Non-video rows keep abort-and-remove untouched.

**Tech Stack:** Rust 2024, tokio (oneshot, `JoinHandle`, `AbortHandle`), GTK4 / libadwaita (unaffected), `std::sync::atomic` for the gate.

**Spec:** `SPEC-attempt-commit-gate.md` (repo root, commit `a2bb035`). The plan argues from the spec; executors read both.

## Global Constraints

- Tests are **serial and mandatory**: `cargo test -- --test-threads=1`. Parallel aborts on environment races.
- All three gates green before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test -- --test-threads=1`, plus sentrux `check_rules`.
- **Every new test is verified non-vacuous** by removing its guard and observing the failure. The commit message records which mutations were run.
- **Never** use a permission bit as a failure trigger. CI runs as root in `container: fedora:44` where `CAP_DAC_OVERRIDE` makes a read-only mode a no-op. Use a structural trigger.
- **Never** clean a shared parent. `staging_dir(id).parent()` is the global staging root `/tmp/grab-video`.
- **Never** `remove_dir_all` a staging directory on a row that is not being removed — `final.<n>.<ext>` there may be the user's only copy of a recording.
- Linux-only process helpers (`GroupCleanup`, `still_running`, `read_pid`, `/proc` reads) must sit behind `#[cfg(target_os = "linux")]`, and any test using them must be gated too.
- A slice leaves `main` deployable. Independent `staff-code-reviewer` review before any push.

## Review Focus

Five input classes the spec implies that no single task's test naturally covers. Each gets a test in the owning task.

1. **Removal landing between the commit CAS and the rename syscall.** Expectation: no orphan file, ever. (Tasks 3, 4, 5)
2. **A row removed while a sibling row already holds the same destination.** Expectation: the finalizer must not delete the sibling's files. (Task 6)
3. **`abort` receiver dropped without a send** (manager shutting down mid-attempt). Expectation: fail closed — no delivery, no panic. (Task 4)
4. **Removal of a row whose attempt never started** (no gate created, e.g. a queued row). Expectation: `remove` must not panic on a missing gate and must still free its slot. (Task 2)
5. **Finalizer racing shutdown.** Expectation: the worker is stopped, not detached, and cleanup still runs exactly once. (Task 7)

---

### Task 1: The `AttemptGate` type and its state machine

The invariant lives in one small type. Nothing else can be correct until this is, so it goes first and gets the most thorough tests.

**Files:**
- Create: `src/attempt_gate.rs`
- Modify: `src/video.rs` (facade re-export), `src/main.rs` (module declaration)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct AttemptGate { /* … */ }` with `AttemptGate::new() -> Self`
  - `pub fn try_commit(&self) -> bool` — CAS `Active → Committing`; `false` means a discard already won
  - `pub fn discard(&self) -> bool` — CAS `Active → Discarded`; `true` means **this** caller won, `false` means the commit already had
  - `pub fn mark_delivered(&self)`
  - `pub fn was_delivered(&self) -> bool`
  - `pub fn is_discarded(&self) -> bool`
  - `impl AttemptGate { pub fn shared(&self) -> Arc<AttemptGate> }`

- [ ] **Step 1: Write the failing tests**

Create `src/attempt_gate.rs` with the tests first, and a sibling `#[cfg(test)] #[path = "attempt_gate_tests.rs"] mod tests;` at the bottom (matching how `video_staging.rs` wires `video_staging_tests`).

`src/attempt_gate_tests.rs`:

```rust
use crate::attempt_gate::AttemptGate;

#[test]
fn a_fresh_gate_allows_a_commit() {
    let gate = AttemptGate::new();
    assert!(gate.try_commit(), "nothing has claimed the attempt yet");
}

#[test]
fn a_gate_commits_only_once() {
    let gate = AttemptGate::new();
    assert!(gate.try_commit());
    assert!(!gate.try_commit(), "a second commit must not also win");
}

#[test]
fn a_discard_before_a_commit_blocks_the_commit() {
    // The window the whole design exists to close: a removal that lands
    // first must win outright, and the worker must then deliver nothing.
    let gate = AttemptGate::new();
    assert!(gate.discard(), "the first discard wins");
    assert!(!gate.try_commit(), "a discarded attempt must not deliver");
    assert!(gate.is_discarded());
}

#[test]
fn a_commit_before_a_discard_blocks_the_discard() {
    // The mirror, and the reason the manager needs `was_delivered`: the
    // commit wins, so the attempt *will* place a file, and the finalizer
    // has to know to remove it.
    let gate = AttemptGate::new();
    assert!(gate.try_commit());
    assert!(!gate.discard(), "the commit already had it");
    assert!(!gate.is_discarded());
}

#[test]
fn delivery_is_recorded_only_once_marked() {
    let gate = AttemptGate::new();
    assert!(!gate.was_delivered());
    gate.mark_delivered();
    assert!(gate.was_delivered());
}

#[test]
fn a_shared_gate_is_the_same_gate() {
    // The manager and the worker must arbitrate on one object, not copies.
    let gate = AttemptGate::new();
    let shared = gate.shared();
    assert!(shared.discard());
    assert!(!gate.try_commit(), "the clone did not observe the discard");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -- --test-threads=1 attempt_gate`
Expected: compile failure — `cannot find crate::attempt_gate`.

- [ ] **Step 3: Write the minimal implementation**

`src/attempt_gate.rs`:

```rust
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

use std::sync::Arc;
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
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(ACTIVE),
            delivered: AtomicBool::new(false),
        }
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

    /// A shareable handle to the same gate, for the manager/worker pair.
    pub fn shared(&self) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(self.state.load(Ordering::Acquire)),
            delivered: AtomicBool::new(self.delivered.load(Ordering::Acquire)),
        })
    }
}

#[cfg(test)]
#[path = "attempt_gate_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -- --test-threads=1 attempt_gate`
Expected: PASS, 6 tests.

- [ ] **Step 5: Wire the module in**

In `src/main.rs`, beside the other `mod` declarations, add `mod attempt_gate;`.
In `src/video.rs`, add the facade re-export:
```rust
/// Facade: the per-attempt delivery decision lives in
/// [`attempt_gate`](crate::attempt_gate) now.
pub use crate::attempt_gate::AttemptGate;
```

- [ ] **Step 6: Verify the mutations**

Three mutations, each must fail a test:
- `try_commit` uses `load` + `store` instead of `compare_exchange` → `a_discard_before_a_commit_blocks_the_commit` fails.
- `discard` always returns `true` → `a_commit_before_a_discard_blocks_the_discard` fails.
- `shared` returns a fresh `Self::new()` → `a_shared_gate_is_the_same_gate` fails.

Run: `cargo test -- --test-threads=1 attempt_gate` after each mutation, then restore.

- [ ] **Step 7: Commit**

```bash
git add src/attempt_gate.rs src/attempt_gate_tests.rs src/main.rs src/video.rs
git commit -m "feat: add the per-attempt commit gate

The delivery decision for one attempt, decided by CAS so exactly one of
{deliver, discard} wins with no window between deciding and acting. Every
later task depends on this type being right, so it lands alone with its
own state-machine tests."
```

---

### Task 2: The manager creates a gate per video attempt

**Files:**
- Modify: `src/download.rs` (the video spawn path around line 1377)
- Test: `src/download_tests.rs`

**Interfaces:**
- Consumes: `AttemptGate::new()`, `AttemptGate::shared()` from Task 1.
- Produces: `DownloadManager::gates: RefCell<HashMap<u64, Arc<AttemptGate>>>`, populated at video spawn and cleared by `remove`/`unremove`. Later tasks read it via `fn gate_for(&self, id: u64) -> Option<Arc<AttemptGate>>`.

- [ ] **Step 1: Write the failing test**

Append to `src/download_tests.rs`:

```rust
#[test]
fn a_video_attempt_gets_a_gate_and_a_plain_row_does_not() {
    // The gate is the only thing that can arbitrate delivery, so a video
    // attempt must have one and a plain row must not: a plain row is
    // aborted outright and never arbitrates anything.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("gate-spawn");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);

    let plain = DownloadItem::new(700_001, "https://example.com/a.bin", "a.bin", "/tmp/dl");
    manager.store().append(&plain);
    manager.epoch.borrow_mut().insert(plain.id(), 1);
    assert!(
        manager.gate_for(plain.id()).is_none(),
        "a plain row must not be given a gate to arbitrate"
    );

    let item = manager
        .restore_existing(&stored_row(
            "https://example.com/v.mp4",
            "/tmp/dl",
            "v.mp4",
            DownloadStatus::Paused,
        ))
        .expect("video row");
    manager
        .video_sources
        .borrow_mut()
        .insert(item.id(), crate::media_types::VideoSource::Page {
            page_url: "https://example.com/v.mp4".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            video_format_id: None,
            playlist_item_id: None,
        });
    assert!(
        manager.gate_for(item.id()).is_some(),
        "a video row with no gate cannot arbitrate delivery, so a removal \\
         could not stop it placing a file"
    );
    let _ = std::fs::remove_file(&_qf);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -- --test-threads=1 a_video_attempt_gets_a_gate`
Expected: compile failure — no method `gate_for`.

- [ ] **Step 3: Implement**

In `src/download.rs`, add beside the other per-row maps:

```rust
    /// Delivery decision per video attempt, by row. Created when the
    /// attempt is spawned so the manager and the worker arbitrate on one
    /// object; see `attempt_gate`.
    gates: RefCell<HashMap<u64, std::sync::Arc<crate::attempt_gate::AttemptGate>>>,
```

Initialise it in the constructor next to `video_abort`, and add the accessor:

```rust
    /// The delivery gate for `id`, if this row has a video attempt.
    pub(crate) fn gate_for(&self, id: u64) -> Option<std::sync::Arc<crate::attempt_gate::AttemptGate>> {
        self.gates.borrow().get(&id).cloned()
    }
```

In the video spawn path, immediately after the abort channel is created, create the gate and record it:

```rust
        let gate = crate::attempt_gate::AttemptGate::new().shared();
        self.gates.borrow_mut().insert(id, std::sync::Arc::clone(&gate));
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -- --test-threads=1 a_video_attempt_gets_a_gate`
Expected: PASS.

- [ ] **Step 5: Verify the mutation**

Do not insert into `gates` at spawn → the test fails on the second assertion.

- [ ] **Step 6: Run the full gates**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add src/download.rs src/download_tests.rs
git commit -m "feat: give each video attempt a delivery gate

One gate per spawned video attempt, shared between the manager and the
worker. Plain rows get none: they are aborted outright and never
arbitrate anything, so a gate there would be dead weight implying a
decision path that does not exist."
```

---

### Task 3: The live leg commits through the gate

**Files:**
- Modify: `src/video_runner.rs` (`run_live_ytdlp` signature and its rename, plus the `try_recv` block added by `174b78a`)
- Test: `src/video_tests.rs`

**Interfaces:**
- Consumes: `AttemptGate` (Task 1).
- Produces: `run_live_ytdlp(…, gate: &Arc<AttemptGate>, …)` — the `gate` parameter sits directly after `job`, matching where the other runners will take it. `run_video_download` forwards it.

- [ ] **Step 1: Write the failing tests**

Append to `src/video_tests.rs`:

```rust
#[test]
fn a_discard_before_the_commit_delivers_nothing() {
    // The live leg must consult the gate, not just the stop signal: the
    // signal is only a prompt, and a prompt can arrive after the one
    // place the worker stops looking.
    let dir = std::env::temp_dir().join(format!("grab-gate-discard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    assert!(gate.discard(), "the removal claims the row before the worker starts");

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &gate,
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(None)), "a discarded attempt must not deliver: {res:?}");
    assert!(!dest.exists(), "a discarded capture was delivered");
    assert!(!gate.was_delivered(), "the gate must not record a delivery that never happened");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_commit_before_the_discard_delivers_and_is_recorded() {
    // The mirror: when the commit wins, the file *is* placed, and the gate
    // says so — which is what tells the finalizer there is an orphan.
    let dir = std::env::temp_dir().join(format!("grab-gate-commit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &gate,
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(Some(_))), "an un-discarded attempt must deliver: {res:?}");
    assert!(dest.exists(), "nothing was placed");
    assert!(gate.was_delivered(), "the gate must record the delivery");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -- --test-threads=1 grab-gate`
Expected: compile failure — `run_live_ytdlp` takes 8 arguments, 9 supplied.

- [ ] **Step 3: Thread the gate and commit through it**

In `src/video_runner.rs`:

1. Add `gate: &std::sync::Arc<AttemptGate>` to `run_live_ytdlp` immediately after `job: &VideoJob`.
2. Add the same parameter to `run_video_download`, and forward it at the `run_live_ytdlp` call site.
3. Replace the `try_recv` block introduced by `174b78a` — which is check-then-act — with a gate commit immediately before the rename:

```rust
    // The linearization point. `try_commit` is a CAS, so there is no window
    // between deciding and acting for a concurrent removal to slip into:
    // either this wins and the row is still here, or the removal already
    // won and there is nothing to deliver.
    if !gate.try_commit() {
        sweep_live_capture(
            &out,
            &part,
            &state,
            staging,
            Some(&final_tmp),
            Staging::Sweep,
            Exit::Discarded,
        )
        .await;
        return Ok(None);
    }
    match crate::file_names::rename_noreplace(&final_tmp, &job.dest) {
        Ok(()) => {
            gate.mark_delivered();
        }
```

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo test -- --test-threads=1 grab-gate`
Expected: PASS, 2 tests.

- [ ] **Step 5: Verify the mutations**

- Delete the `if !gate.try_commit()` block → `a_discard_before_the_commit_delivers_nothing` fails.
- Delete `gate.mark_delivered()` → `a_commit_before_the_discard_delivers_and_is_recorded` fails.

- [ ] **Step 6: Update the existing call sites in tests**

Every existing `run_live_ytdlp(` call in `src/video_tests.rs` needs a `&AttemptGate::new()` argument after `&job`. Add the import:

```rust
use crate::attempt_gate::AttemptGate;
```

- [ ] **Step 7: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/video_runner.rs src/video_tests.rs
git commit -m "fix: the live leg commits through the gate, not a channel poll

Replaces the pre-rename try_recv added by 174b78a. That was
check-then-act: a removal could land between the poll and the rename
and the orphan survived. The gate CASes the decision instead, so there
is no interval to slip into, and a successful rename is recorded so the
finalizer can tell an orphan from a real download."
```

---

### Task 4: The VOD and HLS legs commit through the same gate

`174b78a` routed every video row through the cooperative `Stop::Discard` path but only taught the live leg to check before committing, leaving the same hole in the other two.

**Files:**
- Modify: `src/video_runner.rs` (`run_unified_ytdlp`, `run_hls_ytdlp`, `run_video_download`)
- Test: `src/video_tests.rs`

**Interfaces:**
- Consumes: `AttemptGate` (Task 1), already threaded through `run_video_download` by Task 3.
- Produces: `run_unified_ytdlp(…, gate: &Arc<AttemptGate>, …)` and `run_hls_ytdlp(…, gate: &Arc<AttemptGate>, …)`, both placed immediately after `job`.

- [ ] **Step 1: Write the failing tests**

Append to `src/video_tests.rs`:

```rust
#[test]
fn a_unified_leg_honours_a_discard_before_its_commit() {
    let dir = std::env::temp_dir().join(format!("grab-gate-unified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    assert!(gate.discard());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_unified_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &gate,
        "v123+a456/bv*+ba/b",
        Some("mp4"),
        Some(7),
        &mut stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(None)), "a discarded VOD attempt must not deliver: {res:?}");
    assert!(!dest.exists(), "a discarded VOD attempt was delivered");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_hls_leg_honours_a_discard_before_its_commit() {
    let dir = std::env::temp_dir().join(format!("grab-gate-hls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = fake_ytdlp_hls(&dir);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut job = direct_test_job();
    job.dest = dir.join("v.mkv");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    assert!(gate.discard());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let res = crate::runtime::tokio_rt().block_on(run_hls_ytdlp(
        &fake,
        std::path::Path::new("/usr/bin/ffmpeg"),
        &staging,
        &job,
        &gate,
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(matches!(res, Ok(None)), "a discarded HLS attempt must not deliver: {res:?}");
    assert!(!dest.exists(), "a discarded HLS attempt was delivered");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_closed_stop_receiver_stops_delivery_rather_than_authorising_it() {
    // `Empty` means "no stop yet" and carrying on is right. `Closed` means
    // no sender remains to authorise anything, so it must fail closed —
    // an earlier attempt matched only `Ok(Discard)` and so read a closed
    // receiver as permission to deliver.
    let dir = std::env::temp_dir().join(format!("grab-gate-closed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_yt = fake_ytdlp_live(&dir, false);
    let fake_ff = fake_ffmpeg_copy(&dir);
    let staging = dir.join("staging");
    let mut job = live_test_job();
    job.dest = dir.join("v.mp4");
    let dest = job.dest.clone();

    let gate = AttemptGate::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    // Sender dropped immediately: the receiver observes Closed.
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel::<StopIntent>();
    let res = crate::runtime::tokio_rt().block_on(run_live_ytdlp(
        &fake_yt,
        &fake_ff,
        &staging,
        &job,
        &gate,
        "h1080",
        stop_rx,
        std::time::Duration::from_secs(30),
        tx,
    ));
    assert!(
        !gate.was_delivered(),
        "a closed receiver authorised a delivery: nothing was left to permit it"
    );
    assert!(!dest.exists(), "a closed receiver delivered a file");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -- --test-threads=1 grab-gate`
Expected: the two leg tests fail to compile (arity), and once compiled, the closed-receiver test fails on delivery.

- [ ] **Step 3: Thread the gate and commit through both legs**

Add `gate: &std::sync::Arc<AttemptGate>` immediately after `job` in both `run_unified_ytdlp` and `run_hls_ytdlp`, forward it from `run_video_download`, and wrap each rename exactly as Task 3 does — `if !gate.try_commit() { … return Ok(None); }` before, `gate.mark_delivered();` in the `Ok(())` arm.

- [ ] **Step 4: Handle `Closed` explicitly**

In the live leg, where the stop is consumed, match all three receiver outcomes rather than only `Ok(Discard)`:

```rust
    match abort.try_recv() {
        Ok(StopIntent::Preserve) => {}
        Ok(StopIntent::Discard) | Err(oneshot::error::TryRecvError::Closed) => {
            if !gate.try_commit() || !gate.discard() {
                // A discard already owns the decision, or we just claimed
                // it: either way there is nothing left to deliver.
            }
            sweep_live_capture(
                &out, &part, &state, staging, Some(&final_tmp),
                Staging::Sweep, Exit::Discarded,
            )
            .await;
            return Ok(None);
        }
        Err(oneshot::error::TryRecvError::Empty) => {}
    }
```

- [ ] **Step 5: Update existing call sites**

Every `run_unified_ytdlp(` and `run_hls_ytdlp(` call in `src/video_tests.rs` needs a `&AttemptGate::new()` argument after `&job`.

- [ ] **Step 6: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/video_runner.rs src/video_tests.rs
git commit -m "fix: the VOD and HLS legs commit through the gate too

174b78a routed every video row through the cooperative discard path but
only taught the live leg to check before committing, so a removal during
a VOD or HLS download still delivered. A closed stop receiver now also
stops delivery rather than reading as permission: with no sender left,
nothing authorises placing a file."
```

---

### Task 5: `remove` claims the gate and hands the worker to a finalizer

**Files:**
- Modify: `src/download.rs` (`remove`, `cancel_inner`, `finish_discard`)
- Test: `src/download_tests.rs`

**Interfaces:**
- Consumes: `AttemptGate::discard`, `was_delivered` (Task 1), `gate_for` (Task 2).
- Produces: `fn finish_discard(&self, id: u64, dest: PathBuf, gate: Arc<AttemptGate>)`.

- [ ] **Step 1: Write the failing test**

Append to `src/download_tests.rs`:

```rust
#[test]
fn a_removal_claims_the_gate_before_the_worker_can_commit() {
    // The decision must be claimed at `remove` time, not discovered later:
    // a worker that has not reached its rename yet must find the row gone.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-claims-gate");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 924_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-gateclaim-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let item = DownloadItem::new(id, "https://x.com/u/status/1", "v.mp4", &dest_dir.to_string_lossy());
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = AttemptGate::new().shared();
    manager.gates.borrow_mut().insert(id, std::sync::Arc::clone(&gate));
    let part = dest_dir.join("v.live.mp4.part");
    std::fs::write(&part, b"recorded").unwrap();

    manager.remove(id);

    assert!(
        gate.is_discarded(),
        "remove returned without claiming the gate, so a worker that had not \\
         reached its rename could still deliver"
    );
    assert!(!gate.was_delivered());
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn a_finalizer_removes_the_orphan_a_lost_commit_left_behind() {
    // When the commit wins, the file *is* placed, and the row is gone, so
    // the finalizer has to remove it. This is the only sanctioned exception
    // to `clean_dest_parts` never touching a finished file.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("remove-orphan");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 925_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-orphan-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest = dest_dir.join("v.mp4");
    let item = DownloadItem::new(id, "https://x.com/u/status/1", "v.mp4", &dest_dir.to_string_lossy());
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = AttemptGate::new().shared();
    manager.gates.borrow_mut().insert(id, std::sync::Arc::clone(&gate));
    // The commit already won, so the attempt will place the file.
    assert!(gate.try_commit());
    let handle = crate::runtime::tokio_rt().spawn(async move {
        // Stand in for a worker that was already inside its rename.
        std::fs::write(&dest, b"orphan").ok();
        gate.mark_delivered();
    });
    manager.running.borrow_mut().insert(id, handle);

    manager.remove(id);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while dest.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !dest.exists(),
        "the commit won, so the attempt placed a file, and the row is gone: \\
         the orphan outlived it"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -- --test-threads=1 gate`
Expected: both fail — `remove` does not claim the gate and does not remove the orphan.

- [ ] **Step 3: Implement**

In `src/download.rs`, replace `finish_discard` with:

```rust
    /// Reclaim a removed video row's scratch, but only once its worker has
    /// actually stopped.
    ///
    /// Claiming the gate first is what makes this safe to defer: a worker
    /// that has not reached its rename yet finds the row gone and delivers
    /// nothing, and one that had already committed has placed a file that
    /// step 5 below removes.
    fn finish_discard(
        &self,
        id: u64,
        dest: std::path::PathBuf,
        gate: std::sync::Arc<crate::attempt_gate::AttemptGate>,
    ) {
        let _ = gate.discard();
        let staging = crate::video::staging_dir(id);
        let reclaim = move || {
            crate::video::clean_staging(&staging);
            crate::video::clean_dest_parts(&dest);
            if gate.was_delivered() {
                // The commit won the race, so this file is the attempt's own
                // orphan and the row is gone. The one sanctioned exception
                // to never deleting a finished file.
                let _ = std::fs::remove_file(&dest);
            }
        };
        match self.running.borrow_mut().remove(&id) {
            Some(handle) => {
                let tracked = crate::runtime::tokio_rt().spawn(async move {
                    let _ = handle.await;
                    reclaim();
                });
                self.discards.borrow_mut().insert(id, tracked);
            }
            None => reclaim(),
        }
    }
```

In `remove`, replace the video cleanup block with:

```rust
        if let Some(gate) = self.gate_for(id)
            && self.video_sources.borrow().contains_key(&id)
        {
            let dest = self
                .find(id)
                .map(|i| std::path::PathBuf::from(i.file_path()))
                .unwrap_or_default();
            self.gates.borrow_mut().remove(&id);
            self.finish_discard(id, dest, gate);
        }
```

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo test -- --test-threads=1 gate`
Expected: PASS, 2 tests.

- [ ] **Step 5: Verify the mutations**

- Drop the `let _ = gate.discard();` line → `a_removal_claims_the_gate_before_the_worker_can_commit` fails.
- Drop the `if gate.was_delivered()` block → `a_finalizer_removes_the_orphan_a_lost_commit_left_behind` fails.

- [ ] **Step 6: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/download.rs src/download_tests.rs
git commit -m "fix: remove claims the gate, and the finalizer reclaims the orphan

The gate is claimed at remove time, so a worker that has not reached its
rename finds the row gone and delivers nothing. A worker that had
already committed has placed a file the row no longer owns, so the
finalizer removes it after awaiting the worker -- the one sanctioned
exception to clean_dest_parts never touching a finished file."
```

---

### Task 6: Reserve the destination for the whole teardown

Deferring the dest sweep leaves a window: a new row can claim the filename, and the finalizer's stem-wide sweep then deletes the new row's files. Inferring the reservation from part files misses the pre-recreation window, and `unremove` bypasses intake dedupe entirely.

**Files:**
- Modify: `src/download.rs` (reservation set, intake reservation check, `unremove`)
- Test: `src/download_tests.rs`

**Interfaces:**
- Consumes: `gate_for` (Task 2).
- Produces: `reservations: Arc<std::sync::Mutex<HashSet<PathBuf>>>`, with `fn reserve_dest(&self, dest: &Path)`, `fn release_dest(&self, dest: &Path)`, `fn dest_reserved(&self, dest: &Path) -> bool`.

- [ ] **Step 1: Write the failing tests**

Append to `src/download_tests.rs`:

```rust
#[test]
fn a_pending_discard_reserves_the_destination_against_intake() {
    // Between remove and its finalizer, the stem must not be claimable:
    // otherwise the finalizer's stem-wide sweep deletes the *new* row's
    // part files.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-intake");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    assert!(!manager.dest_reserved(&dest));
    manager.reserve_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "intake could claim a destination whose row is still tearing down"
    );
    manager.release_dest(&dest);
    assert!(!manager.dest_reserved(&dest), "the reservation outlived the cleanup");
}

#[test]
fn an_undo_does_not_reclaim_a_destination_with_a_pending_discard() {
    // Undo bypasses intake dedupe and starts a row with the same filename
    // immediately, so it has to consult the reservation too.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("reserve-undo");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let dest = std::path::PathBuf::from("/tmp/dl/v.mp4");
    manager.reserve_dest(&dest);
    assert!(
        manager.dest_reserved(&dest),
        "Undo was allowed to reclaim a destination with a pending discard"
    );
    let _ = std::fs::remove_dir_all("/tmp/dl");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -- --test-threads=1 reserve`
Expected: compile failure — no `dest_reserved`.

- [ ] **Step 3: Implement the set and its accessors**

In `src/download.rs`, add the field beside `gates`:

```rust
    /// Destinations with a discard in flight, by canonical path. Consulted
    /// by intake *and* `unremove`: the filesystem-derived `stem_reserved_in`
    /// check cannot see the window between a worker deleting its shell and
    /// recreating it, and `unremove` bypasses intake entirely.
    reservations: Arc<std::sync::Mutex<HashSet<std::path::PathBuf>>>,
```

Initialise it in the constructor and add:

```rust
    /// Whether `dest` has a row removal still tearing down.
    pub(crate) fn dest_reserved(&self, dest: &std::path::Path) -> bool {
        self.reservations
            .lock()
            .map(|set| set.contains(dest))
            .unwrap_or(false)
    }

    fn reserve_dest(&self, dest: &std::path::Path) {
        if let Ok(mut set) = self.reservations.lock() {
            set.insert(dest.to_path_buf());
        }
    }

    fn release_dest(&self, dest: &std::path::Path) {
        if let Ok(mut set) = self.reservations.lock() {
            set.remove(dest);
        }
    }
```

In `remove`, reserve before starting the finalizer, and have the finalizer release afterwards:

```rust
            self.reserve_dest(&dest);
```

and inside `finish_discard`'s `reclaim` closure, after the orphan removal:

```rust
            self.release_dest(&dest);
```

`reclaim` must therefore capture an `Arc` clone of the reservation set rather than borrowing `self`, since it runs on the tokio runtime.

- [ ] **Step 4: Consult it from intake and `unremove`**

In the intake name-claim path, after the existing `stem_reserved_in` check, add `&& !self.dest_reserved(&candidate)`. In `unremove`, before starting the restored row, skip the start while `self.dest_reserved(&dest)` is true, so the restored row requeues instead of colliding.

- [ ] **Step 5: Run them to verify they pass**

Run: `cargo test -- --test-threads=1 reserve`
Expected: PASS, 2 tests.

- [ ] **Step 6: Verify the mutation**

Remove the `release_dest` call → `a_pending_discard_reserves_the_destination_against_intake` fails on its last assertion (the reservation never clears).

- [ ] **Step 7: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/download.rs src/download_tests.rs
git commit -m "fix: reserve the destination for a row's whole teardown

Deferring the dest sweep opened a window: a new row could claim the
filename and the finalizer's stem-wide sweep would delete the new row's
parts. Inferring the reservation from part files misses the gap between a
worker deleting its shell and recreating it, and unremove bypasses intake
dedupe entirely, so the reservation is manager-owned and both paths
consult it."
```

---

### Task 7: Shutdown stops the worker rather than detaching it

A finalizer *owns* the worker handle, and dropping a `JoinHandle` detaches the task rather than aborting it. Aborting the finalizer therefore leaves yt-dlp running with no supervisor — the exact regression #180 fixed, introduced by `174b78a`.

**Files:**
- Modify: `src/download.rs` (`discards`, `finish_discard`, `shutdown`, `start_next`)
- Test: `src/download_tests.rs`

**Interfaces:**
- Consumes: `finish_discard` (Task 5).
- Produces:
  ```rust
  struct PendingDiscard {
      /// Aborts the worker directly, so stopping it does not depend on the
      /// finalizer surviving.
      worker_abort: tokio::task::AbortHandle,
      finalizer: tokio::task::JoinHandle<()>,
  }
  discards: RefCell<HashMap<u64, PendingDiscard>>,
  ```

- [ ] **Step 1: Write the failing test**

Append to `src/download_tests.rs`, gated for Linux alongside the other process tests:

```rust
#[cfg(target_os = "linux")]
#[test]
fn shutdown_during_a_pending_discard_stops_the_worker_rather_than_detaching_it() {
    // The finalizer owns the worker handle. Aborting the finalizer drops
    // that handle, which *detaches* the worker -- yt-dlp would keep running
    // with no supervisor, which is the regression #180 fixed.
    let (_q, _l) = test_locks();
    let _qf = test_queue_file("shutdown-discard");
    let settings = test_settings();
    let manager = DownloadManager::new(gio::ListStore::new::<DownloadItem>(), settings);
    let id = 926_000 + std::process::id() as u64;
    let dest_dir = std::env::temp_dir().join(format!("grab-shutdisc-{id}"));
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).unwrap();
    let item = DownloadItem::new(id, "https://x.com/u/status/1", "v.mp4", &dest_dir.to_string_lossy());
    manager.store().append(&item);
    manager.video_sources.borrow_mut().insert(
        id,
        crate::media_types::VideoSource::Page {
            page_url: "https://x.com/u/status/1".to_string(),
            media_url: None,
            expires_at: None,
            quality: "1080p".to_string(),
            audio_only: false,
            video_format_id: None,
            playlist_item_id: None,
        },
    );
    manager.epoch.borrow_mut().insert(id, 1);
    let gate = AttemptGate::new().shared();
    manager.gates.borrow_mut().insert(id, std::sync::Arc::clone(&gate));

    // A worker that runs long and records that it was dropped.
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let flag = std::sync::Arc::clone(&dropped);
    let handle = crate::runtime::tokio_rt().spawn(async move {
        let _flag = DropFlag(flag);
        std::future::pending::<()>().await;
    });
    let worker_abort = handle.abort_handle();
    manager.running.borrow_mut().insert(id, handle);
    let dest = dest_dir.join("v.mp4");
    let staging = crate::video::staging_dir(id);
    let reclaim_gate = std::sync::Arc::clone(&gate);
    let tracked = crate::runtime::tokio_rt().spawn(async move {
        let _ = manager.running.borrow().get(&id).map(|_| ());
        // Mirror finish_discard: wait for the worker, then reclaim.
        let _ = worker_abort;
        let _ = reclaim_gate.discard();
        crate::video::clean_staging(&staging);
        crate::video::clean_dest_parts(&dest);
    });
    manager.discards.borrow_mut().insert(
        id,
        crate::download::PendingDiscard { worker_abort, finalizer: tracked },
    );

    manager.shutdown();

    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "shutdown left the worker running: the finalizer's handle was dropped, \\
         which detaches the task instead of aborting it"
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -- --test-threads=1 shutdown_during_a_pending_discard`
Expected: compile failure — no `PendingDiscard` type.

- [ ] **Step 3: Implement**

In `src/download.rs`, define the record and change the registry:

```rust
/// A removal whose worker is still tearing down.
struct PendingDiscard {
    /// Aborts the *worker* directly. Aborting the finalizer instead would
    /// drop this handle, and dropping a `JoinHandle` detaches the task
    /// rather than aborting it -- leaving yt-dlp unsupervised.
    worker_abort: tokio::task::AbortHandle,
    finalizer: tokio::task::JoinHandle<()>,
}
```

Change `discards` to `RefCell<HashMap<u64, PendingDiscard>>`, and in `finish_discard` record both handles:

```rust
            Some(handle) => {
                let worker_abort = handle.abort_handle();
                let finalizer = crate::runtime::tokio_rt().spawn(async move {
                    let _ = handle.await;
                    reclaim();
                });
                self.discards.borrow_mut().insert(id, PendingDiscard { worker_abort, finalizer });
            }
```

In `shutdown`, stop the workers first, then await the finalizers so cleanup still runs:

```rust
        let finals: Vec<PendingDiscard> = self.discards.borrow_mut().drain().map(|(_, p)| p).collect();
        for pending in &finals {
            pending.worker_abort.abort();
        }
        // … alongside the normal worker handles, as before …
        tokio_rt().block_on(async {
            for pending in finals {
                let _ = pending.finalizer.await;
            }
        });
```

Prune completed finalizers in `start_next` so the registry does not grow by one entry per removed row for the life of the session:

```rust
        self.discards
            .borrow_mut()
            .retain(|_, pending| !pending.finalizer.is_finished());
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -- --test-threads=1 shutdown_during_a_pending_discard`
Expected: PASS.

- [ ] **Step 5: Verify the mutation**

Remove the `pending.worker_abort.abort()` loop from `shutdown` → the test fails on the `dropped` assertion.

- [ ] **Step 6: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/download.rs src/download_tests.rs
git commit -m "fix: shutdown stops a discarded worker instead of detaching it

A finalizer owns its worker's JoinHandle, and dropping one detaches the
task rather than aborting it, so aborting the finalizer left yt-dlp
running with no supervisor -- the regression #180 fixed. The worker's
AbortHandle is now retained alongside the finalizer and fired directly,
then the finalizer is awaited so its cleanup still runs. Completed
finalizers are pruned on start_next."
```

---

### Task 8: Wait for the recorder's process group before reclaiming

`reap_child` waits only the direct child, while the group guards *signal* SIGKILL and return. A descendant can still be running when the task returns, and this repo's own `LiveScratchGuard` docs record that a dying recorder recreates its state file after an unlink.

**Files:**
- Modify: `src/video_spawn.rs` (the quiescence helper), `src/video_runner.rs` (call it on the discard path)
- Test: `src/video_spawn.rs`'s sibling test file, or `src/video_tests.rs`

**Interfaces:**
- Consumes: `ProcessGroupGuard`'s pgid accessor.
- Produces: `pub(crate) fn await_group_quiescence(pgid: i32, timeout: Duration) -> bool` — `false` means the group did not quiesce, which the caller must treat as "a writer may remain".

- [ ] **Step 1: Write the failing test**

Append to `src/video_tests.rs`, Linux-gated:

```rust
#[cfg(target_os = "linux")]
#[test]
fn a_group_that_refuses_to_quiesce_is_reported_rather_than_assumed() {
    // A descendant that ignores its group kill must be *reported*, not
    // silently treated as gone: the caller sweeps anyway (there is nothing
    // better to do) but the outcome is observable rather than assumed.
    let dir = std::env::temp_dir().join(format!("grab-quiesce-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("still-writing");
    // A process in its own group that writes, then sleeps well past the wait.
    let script = dir.join("stubborn");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'x' >> '{}'\nsleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut child = std::process::Command::new(&script)
        .process_group(0)
        .spawn()
        .expect("stubborn descendant");
    let pgid = child.id() as i32;
    std::thread::sleep(std::time::Duration::from_millis(100));

    let quiesced = await_group_quiescence(pgid, std::time::Duration::from_millis(300));
    assert!(
        !quiesced,
        "a live process group was reported as quiesced, so a reclaim would run \\
         under a writer that never stopped"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -- --test-threads=1 refuses_to_quiesce`
Expected: compile failure — no `await_group_quiescence`.

- [ ] **Step 3: Implement**

In `src/video_spawn.rs`:

```rust
/// Wait until no process remains in `pgid`, bounded.
///
/// `reap_child` waits only the direct child, and the group guards *signal*
/// SIGKILL and return, so task completion is not by itself proof that no
/// writer remains. `false` means the group did not quiesce in time, and the
/// caller must treat a writer as possibly still present.
#[cfg(target_os = "linux")]
pub(crate) fn await_group_quiescence(pgid: i32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // SAFETY: signal 0 performs error checking only; a constant signal.
        let alive = unsafe { libc::killpg(pgid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn await_group_quiescence(_pgid: i32, _timeout: std::time::Duration) -> bool {
    // No process-group introspection available; the direct-child reap is
    // the strongest guarantee this platform offers.
    true
}
```

In `src/video_runner.rs`, on the discard path in `run_live_ytdlp`, after `reap_child` and before returning, wait for the group and log if it does not quiesce:

```rust
        if !crate::video_spawn::await_group_quiescence(group.pgid(), timeout) {
            tracing::warn!(
                "recorder process group did not quiesce; reclaiming anyway with a \\
                 writer possibly still present"
            );
        }
```

This needs `pgid()` on `ProcessGroupGuard`; add it if absent:

```rust
    /// The process group id this guard signals.
    pub(crate) fn pgid(&self) -> i32 {
        self.pgid
    }
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -- --test-threads=1 refuses_to_quiesce`
Expected: PASS.

- [ ] **Step 5: Verify the mutation**

Return `true` unconditionally → the test fails on its `!quiesced` assertion.

- [ ] **Step 6: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/video_spawn.rs src/video_runner.rs src/video_tests.rs
git commit -m "fix: wait for the recorder's process group before reclaiming

reap_child waits only the direct child, and the group guards signal
SIGKILL and return, so task completion is not proof that no writer
remains -- this repo's own LiveScratchGuard docs record a dying recorder
recreating its state file after an unlink. The discard path now waits
for the group, bounded, and reports rather than assumes when it does not
quiesce."
```

---

### Task 9: Close the fixture holes the reviews found, and re-verify non-vacuity

Every test in this plan is only as good as its fixture, and three rounds of review found fixture bugs that made passing tests meaningless.

**Files:**
- Modify: `src/download_tests.rs`, `src/video_tests.rs`
- Test: the same

**Interfaces:**
- Consumes: everything above.
- Produces: no new API; this task hardens the existing tests.

- [ ] **Step 1: Prove cancellation with a `Drop` flag, not an absence**

Find the plain-row abort test added by `174b78a` and replace its "flag not set after 200ms" oracle, which passes even when the task was detached rather than aborted. Give the task a `Drop` flag and a start barrier:

```rust
    // A Drop flag inside the task, not an absence checked after a sleep:
    // asserting a flag was *not* set passes just as well when the task was
    // detached as when it was aborted.
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
```

Assert the drop flag becomes true after `remove`, and delete the `handle.abort()` line to confirm the test fails.

- [ ] **Step 2: Order the recorder fixture's markers**

`fake_ytdlp_live_abortable` publishes the leader pid *before* spawning its descendant, so a stop can land between the two and the descendant assertion then fails for the wrong reason. Reorder it: spawn the descendant and write `recorder-child-pid` first, publish `recorder-pid` last. Have every waiter watch for the *leader* marker, which now implies the descendant exists.

- [ ] **Step 3: Replace the mid-remux sleep with a release marker**

The mid-remux test in `174b78a` relies on a 2-second ffmpeg sleep, which a delayed sender can miss. Replace it with a fake ffmpeg that writes a "started" marker and then blocks until a "release" marker appears; the test creates the release marker only *after* `stop_tx.send(Discard)` has returned.

- [ ] **Step 4: Stop suppressing late-write errors**

The stand-in worker in `remove_tells_a_live_worker_to_discard_and_waits_for_it_to_stop` writes with `.ok()`. Record whether each write actually succeeded and assert both did, so a fixture that silently failed to write cannot pass.

- [ ] **Step 5: Gate the Linux-only tests**

Add `#[cfg(target_os = "linux")]` to every test using `GroupCleanup`, `still_running` or `read_pid`. Verify with:

Run: `cargo check --all-targets --target x86_64-unknown-linux-gnu`
Expected: clean. (If the target is unavailable locally, confirm by inspection that each such test carries the attribute.)

- [ ] **Step 6: Re-run every mutation from this plan**

For each guard listed in the earlier tasks' Step 5/6, remove it and confirm the named test fails. Record the list in the commit message.

- [ ] **Step 7: Full gates, then commit**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test -- --test-threads=1`

```bash
git add src/download_tests.rs src/video_tests.rs
git commit -m "test: close the fixture holes that made these tests meaningless

Three rounds of review found fixtures, not assertions, as the recurring
problem: an absence-checked-after-a-sleep oracle that passes for a
detached task as readily as an aborted one, a recorder fake publishing
its leader pid before the descendant it later asserts on, a two-second
sleep standing in for an ordering guarantee, and late writes whose errors
were swallowed. Each is replaced with something that fails for the right
reason."
```

---

## Self-Review

**Spec coverage.** Every section of `SPEC-attempt-commit-gate.md` maps to a task: the gate and CAS (1), manager gate creation (2), live-leg commit (3), VOD/HLS commit and receiver states (4), removal claiming the gate and the orphan finalizer (5), destination reservation (6), shutdown (7), group quiescence (8), and the fixture rules plus the two still-open claims (9). The "Ask first" boundaries are carried in Global Constraints; "Never" items map to Tasks 3, 5, 6 and 7.

**Review Focus coverage.** (1) CAS-then-rename — Tasks 3, 4, 5. (2) sibling row at the same destination — Task 6. (3) dropped sender — Task 4's closed-receiver test. (4) removal of a row with no gate — Task 2 asserts a plain row has none, and Task 5's `gate_for` returns `None` so `remove` skips the cooperative path. (5) finalizer racing shutdown — Task 7.

**Type consistency.** `AttemptGate::new`, `shared`, `try_commit`, `discard`, `mark_delivered`, `was_delivered`, `is_discarded` are defined once in Task 1 and used unchanged thereafter. `gate_for` is defined in Task 2 and consumed by Tasks 5 and 6. `PendingDiscard` is defined in Task 7 and only constructed there. `finish_discard`'s signature is fixed in Task 5 and not changed by Task 7.

**Placeholders.** None; every step carries the code or the exact command.
