# Spec: the attempt commit gate

Fixes [#179](https://github.com/houssemko/Grab/issues/179): removing a row
mid-live-capture must not deliver a file, and must reclaim the row's scratch
only once nothing can still be writing it.

This is the fourth attempt at the issue. The first three were rejected in
review, each time for a deeper reason, and those findings are folded in here
rather than rediscovered. See "Why the earlier attempts failed" at the end.

Status: designed, not implemented.

## The invariant

For one download attempt with destination `D`, a row removal `R`, and a
delivery step (renaming the attempt's finished file to `D`):

> It must never be the case that `R` happens-before the delivery **and** the
> delivered file survives.

Exactly one of {deliver, discard} wins, and the decision is atomic. Every
other requirement — scratch reclamation, not regressing plain downloads,
shutdown safety, destination reservation — follows from getting that one
decision right.

The naive fix is to check whether the row still exists immediately before the
rename. That is check-then-act and cannot work:

1. The worker checks, sees the row, and is descheduled.
2. `remove` runs, deletes the row, starts its finalizer.
3. The worker resumes and renames successfully.
4. The finalizer sweeps staging and dest-side *parts* — and deliberately
   never touches a finished file, because that is a user's real download.

The orphan survives. Adding a second check only moves the window.

## The gate

A per-attempt handle created when a video attempt is spawned and passed into
the runner next to the existing stop receiver.

```rust
/// Decides whether one attempt may deliver its result.
pub struct AttemptGate {
    state: AtomicU8,
    delivered: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attempt { Active = 0, Committing = 1, Discarded = 2 }

impl AttemptGate {
    /// Claim the right to deliver. `false` means a discard already won.
    fn try_commit(&self) -> bool {
        self.state
            .compare_exchange(Active, Committing, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Claim the row as gone. `true` means the commit had already won.
    fn discard(&self) -> bool {
        matches!(
            self.state.compare_exchange(Active, Discarded, Ordering::AcqRel, Ordering::Acquire),
            Ok(Active)
        )
    }

    fn was_delivered(&self) -> bool { self.delivered.load(Ordering::Acquire) }
}
```

Whichever CAS wins is the linearization point. There is no interval between
"decide" and "act" that a competing decision can slip into, because the
decision *is* the state transition.

### Why not let the manager arbitrate

The alternative is manager-mediated delivery: the worker never renames, it
asks the manager to commit and the manager decides. That is trivially correct
— one arbiter cannot disagree with itself — but it puts a main-loop round-trip
in front of every delivery and makes the manager hold a file across it,
coupling the video runner to `DownloadManager` for a problem the worker can
solve locally. The gate keeps the worker self-contained and puts the invariant
in one small, heavily-testable type.

## Delivery in all three legs

`run_live_ytdlp`, `run_unified_ytdlp` and `run_hls_ytdlp` each wrap their
existing rename:

```rust
if !gate.try_commit() {
    // A removal won. Reclaim this attempt's own scratch and deliver nothing.
    sweep_live_capture(/* … */, Staging::Sweep, Exit::Discarded).await;
    return Ok(None);
}
match rename_noreplace(&final_tmp, &dest) {
    Ok(()) => gate.mark_delivered(),
    /* … existing arms … */
}
```

All three legs need it. Routing every video row through the cooperative
`Stop::Discard` path while only the live leg checks before committing is how
the earlier attempt left an equivalent hole in the VOD and HLS paths.

The stop receiver stays as the *prompt* — it wakes the worker so it stops
promptly — but it is no longer the authority. The gate is.

### Receiver states

`try_recv()` has three outcomes and all three are handled explicitly:

| Outcome | Meaning | Action |
|---|---|---|
| `Empty` | no stop requested | carry on |
| `Ok(Preserve)` | user stop | finalize and deliver |
| `Ok(Discard)` | row removed | `gate.discard()` |
| `Closed` | the manager is gone | **fail closed** — do not deliver |

An earlier attempt matched only `Ok(Discard)` and so treated a closed
receiver as permission to proceed. With no sender left, nothing authorises the
delivery, so `Closed` must stop it.

## Removal

```rust
pub fn remove(&self, id: u64) {
    self.cancel_inner(id, true, Stop::Discard);
    self.epoch.borrow_mut().remove(&id);
    if self.video_sources.borrow().contains_key(&id) {
        let dest = …;
        self.reservations.lock().insert(dest.clone());
        self.finish_discard(id, dest, gate);
    }
    …
}
```

`finish_discard` moves the attempt's `JoinHandle` into a finalizer that:

1. `gate.discard()` — claim the row as gone before anything else.
2. `await`s the worker.
3. `clean_staging(staging_dir(id))`.
4. `clean_dest_parts(dest)`.
5. **If `gate.was_delivered()`**, remove `dest` itself. The commit won the
   race, so the file is this attempt's and the row is gone; leaving it would
   be exactly the orphan the invariant forbids.
6. Drop the destination reservation.

Step 5 is why `clean_dest_parts` keeps its "never touch the finished file"
rule. That rule is correct for every other caller, where the finished file is
a real download; here the caller knows the file is an orphan.

### Non-video rows are untouched

`Stop::Discard` is gated on `video_sources.contains_key(&id)`. A plain HTTP or
torrent row has no gate, no sender and never reaches this path, so it keeps
the original abort-and-remove. An earlier attempt made removal cooperative
for every row, so a plain row's engine kept writing after its row was gone and
the completed handle leaked a concurrency slot, because the pump tail
early-returns on the epoch entry `remove` drops.

## Shutdown

A finalizer *owns* the worker handle, and dropping a `JoinHandle` detaches the
task rather than aborting it. So aborting the finalizer during shutdown leaves
yt-dlp running with no supervisor — precisely what #180 was written to
prevent, and a regression an earlier attempt introduced.

```rust
struct PendingDiscard {
    /// Aborts the *worker* directly, so it does not depend on the finalizer
    /// surviving.
    worker_abort: tokio::task::AbortHandle,
    finalizer: tokio::task::JoinHandle<()>,
}
discards: RefCell<HashMap<u64, PendingDiscard>>,
```

`shutdown` aborts every `worker_abort` first, then awaits the finalizers,
which now observe a finished worker and run their cleanup.

Completed finalizers are pruned on `start_next` via `JoinHandle::is_finished`,
so the registry does not grow by one entry per removed row for the life of
the session.

## Destination reservation

Deferring the dest-side sweep is not enough on its own. Between `remove`
returning and the finalizer running, a new row can claim the same filename,
and the finalizer's stem-wide `clean_dest_parts` would delete the *new* row's
part files.

Inferring the reservation from part files on disk — the approach an earlier
attempt took — has two holes:

- The live worker deletes its shell before recreating it, so there is a window
  where the stem looks free.
- `unremove` bypasses intake dedupe entirely and starts a row with the same
  filename immediately.

So the reservation is manager-owned and explicit:

```rust
reservations: Arc<Mutex<HashSet<PathBuf>>>,   // canonical destinations
```

Inserted when `remove` starts a finalizer, cleared by the finalizer when it
finishes. **Both** intake and `unremove`/spawn consult it, alongside the
existing filesystem-derived `stem_reserved_in` check.

## Group quiescence

Awaiting the task is necessary but not sufficient, and the gap is real:
`reap_child` waits only the direct child, while the group guards *signal*
SIGKILL and return. A descendant can therefore still be running when the task
returns, and this repository's own `LiveScratchGuard` documentation records
that a dying recorder recreates its state file after an unlink.

So the worker waits for its process group to actually be gone before
returning on the discard paths, bounded and with the result treated as
meaningful rather than assumed:

```rust
/// Wait until no process remains in `pgid`, bounded. `false` means the group
/// did not quiesce, which the caller must treat as "a writer may remain".
fn await_group_quiescence(pgid: i32, timeout: Duration) -> bool
```

If it does not quiesce in time, the finalizer still sweeps — there is nothing
better to do — but the outcome is reported rather than assumed, so the
behaviour is observable instead of silently load-bearing.

## Testing

Serial, in the existing sibling `*_tests.rs` files. Every test is verified
non-vacuous by removing its guard and observing the failure. Four rounds of
review found that the *fixtures* were as much the problem as the assertions,
so the fixture rules are part of the spec.

### What each test must prove

| Test | Proves |
|---|---|
| `a_removal_losing_the_commit_race_delivers_nothing` | Discard before `try_commit` → no delivery. |
| `a_removal_that_loses_the_race_still_reclaims_the_delivered_file` | Commit won → the finalizer removes the orphan. |
| `a_removal_wins_the_commit_race` | The interleaving is deterministic via a barrier, not a sleep. |
| the same three for the unified and HLS legs | All three commit paths, not just live. |
| `removing_a_plain_row_still_aborts_its_task_and_frees_the_slot` | The non-video regression, with a `Drop` flag inside the task. |
| `removing_a_row_immediately_undoes_cleanly` | Undo during pending cleanup does not lose either row's files. |
| `a_discard_lands_while_a_remux_is_in_flight` | The mid-remux window, ordered by a release marker. |
| `shutdown_during_a_pending_discard_stops_the_worker` | The worker's `AbortHandle` fires; no detached yt-dlp. |
| `a_closed_receiver_stops_delivery` | `Closed` fails closed, at both observation points. |
| `a_discard_waits_for_the_recorder_group_to_quiesce` | The descendant is non-runnable before the sweep. |

### Fixture rules, each from a review finding

- **Never suppress a write with `.ok()` in a stand-in.** Record whether the
  late write actually succeeded and assert it. An earlier ordering test wrote
  into staging without recreating the parent; an inline sweep had already
  removed it, the write failed, `.ok()` hid it, and the test passed against the
  exact bug it claimed to catch.
- **A stand-in must recreate the directories before its late write**, because
  that is what a dying recorder does, and it is the only thing that
  distinguishes an inline sweep from a deferred one.
- **Order a fake's markers so a waiter cannot observe a half-built fixture.**
  The recorder fake published the leader pid before spawning its descendant,
  so a stop could land between the two and the descendant assertion failed for
  the wrong reason. Publish the descendant first, the leader last.
- **Order with a release marker, never a sleep.** The mid-remux test used a 2s
  ffmpeg sleep; a delayed sender could miss it. The fake must wait for a
  marker created *after* the discard is sent.
- **Prove cancellation with a `Drop` flag inside the task, plus a start
  barrier.** Asserting that a flag was *not* set after a short wait passes even
  when the task was detached rather than aborted — the same hole that made the
  first attempt's `abort()` untestable.
- **Gate Linux-only process tests.** `GroupCleanup`, `still_running` and
  `read_pid` exist only on Linux; an ungated test using them breaks the build
  elsewhere.
- **Never use a permission bit as a failure trigger.** CI runs as root in
  `container: fedora:44`, where `CAP_DAC_OVERRIDE` makes a read-only mode a
  no-op. Use a structural trigger.
- **Never clean a shared parent.** `staging_dir(id).parent()` is the global
  staging root.

## What this does not claim

- **A live capture can still be stopped before its liveness is known.**
  `live_rows` is populated by an async `LiveDetected` message, so a stop in
  that window takes the non-live path and discards the capture. That is a
  separate defect with a shared root cause, and it stays open.
- **A retained recording is not surfaced to the user.** It sits in staging.
- **A group that refuses to quiesce is still swept.** The wait is bounded and
  reports its outcome; it does not escalate to refusing the reclaim.
- **Undo restores the row, not the recording.**

## Boundaries

**Always**
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test -- --test-threads=1`, green before every commit, plus
  sentrux `check_rules`.
- Every new test verified non-vacuous, including its fixture.
- A slice leaves `main` deployable.

**Ask first**
- Any change to `PART_KINDS` — it is a deletion allowlist, so each entry is a
  class of user-visible file Grab may remove.
- Any change to `staging_dir`'s keying (see the #178 spec).
- Deleting a file at a row's destination. Step 5 above is the *only* sanctioned
  exception, and only for a row that is being removed.

**Never**
- Check-then-act around delivery. Use the gate.
- Abort a finalizer in place of aborting its worker.
- Sweep a row's scratch from inside the task that owns it.
- Let a new row or an Undo claim a destination with a pending reservation.

## Why the earlier attempts failed

Recorded so the next reader does not repeat them.

1. **Attempt 1** sent the stop-and-keep signal *before* aborting, and treated
   `abort()` as synchronous. It also shipped tests that stayed green when
   `abort()` was deleted.
2. **Attempt 2** made removal cooperative for every row, regressing plain
   downloads. Its ordering test was vacuous — the same `.ok()` hole. Discard
   arriving after the recorder wait was never read, so a mid-remux removal
   still delivered. The finalizer bypassed the shutdown registry.
3. **Attempt 3** added a pre-rename `try_recv()`, which is check-then-act with
   a window; deferred cleanup narrowed the orphan-delivery race without closing
   it. Its shutdown path detached nested workers, regressing #180. The
   filesystem-inferred stem reservation missed the pre-recreation window and
   Undo entirely.

The common thread: each attempt added a *check* where the problem needed a
*decision*.
