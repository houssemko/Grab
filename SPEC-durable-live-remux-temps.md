# Spec: Durable Live Remux Temps

Covers the durability half of GitHub issue
[#178](https://github.com/houssemko/Grab/issues/178). The second half —
[#179](https://github.com/houssemko/Grab/issues/179), what happens to a
recording whose row is removed — is **not** in this spec; see "Not here".

Status: implemented. This document records what shipped and, just as
importantly, what it does **not** claim.

## Objective

A live capture can finish recording and then fail to place the result at
its destination. The completed recording is then the user's only copy of
that capture — and today a Retry destroys it.

Before: every attempt remuxed into the same `staging/<id>/final.<ext>`,
and the terminal cleanup wiped the whole staging directory. So a retry
either overwrote the previous attempt's finished recording, or swept it
away. A user who could not save a capture because of a permissions
problem would lose it by pressing Retry.

After: each attempt claims its own remux temp, and cleanup removes
exactly that file.

## Why not park the recording beside the destination

The first draft of this spec parked the un-claimable recording in the
user's folder, as `<stem>.parked.<ext>`. **A direct `renameat2(RENAME_NOREPLACE)`
probe showed that mechanism is unreachable**, which is why it was dropped
rather than implemented.

Every rename failure that is not `EEXIST` is a property of the source or
destination *directory*, not of the target *name*:

| rename failure | errno | would a park beside dest succeed? |
|---|---|---|
| dest is an existing dir / dangling symlink | `EEXIST` | different branch (requeue) |
| dest dir not writable | `EACCES` | no — same directory |
| src dir not writable | `EACCES` | no — same directory |
| dest parent is a regular file | `ENOTDIR` | no — same directory |
| dest name exceeds `NAME_MAX` | `ENAMETOOLONG` | no — see below |

And the name-length case cannot be rescued either: the park name is always
exactly **7 characters longer** than the dest name (`.parked.` inserted
before the extension), so anything that trips `NAME_MAX` for the dest
trips it harder for the park.

A park would therefore have fallen back to the status quo in every
reachable case, while claiming to provide durability. Relocating the file
is the wrong lever; making the cleanup precise is the right one.

## Design

### Attempt-scoped, exclusively claimed temps

`reserve_remux_temp(staging, ext)` in `src/video_staging.rs` claims
`final.<n>.<ext>` and returns it, or fails.

- **The counter** is the durability mechanism: an attempt cannot touch an
  earlier attempt's temp because the names differ.
- **The lease** (`<temp>.lease`, created with `create_new`) is what makes
  the claim *exclusive*. Scanning for a free name and then writing it is
  check-then-use — two overlapping attempts can both see slot 1 free, and
  the loser's cleanup would unlink the winner's completed recording.
  `create_new` makes the claim atomic.
- **The lease is a separate file** because the temp itself cannot be
  pre-created: ffmpeg runs without `-y` and refuses to overwrite an
  existing output.
- **It fails closed.** An unreadable staging directory, or an exhausted
  `1..=9999` range, returns an error rather than a guess. Handing back an
  occupied slot would let the caller's cleanup delete a real recording.
  The runner treats that exactly like the pre-existing remux-failure
  path: keep the raw shell, report the error.

### Precise cleanup

`sweep_live_capture` takes `final_tmp: Option<&Path>`:

- the sweeping exits remove exactly that temp and release its lease;
- then `tokio::fs::remove_dir(staging)` — **non-recursive**, so it only
  succeeds when the sweep left the directory empty. A sibling temp is
  never collateral;
- `Staging::Keep` (the unexpected-rename exit) removes nothing.

The per-attempt reset at the top of the attempt loop is unchanged: it
only touches the dest-side `out`/`part`/`state`, never staging.

## What this does and does not claim

**Does:** within one process, on the live route, a completed remux that
could not be placed survives any number of later attempts on that row.

**Does not:**

- **Across a restart.** `staging_dir(item_id)` is keyed by a
  session-local row id, and a restored row gets a new one, so a retry
  after a restart scans a different directory. The old recording is
  orphaned rather than adopted — and, worse, a later row that receives
  the same id can have `clean_staging` delete it. This is pre-existing
  (`staging_dir` has always been keyed this way) but this change makes
  the contents worth preserving, so it needs a durable staging key.
- **Across a route change.** `run_hls_ytdlp` and `run_unified_ytdlp`
  also call `remove_dir_all(staging)`. A retry that resolves through
  HLS or VOD can still sweep a live attempt's retained remux.
- **When the row is removed mid-finalization.** `DownloadManager::remove`
  still exempts `live_rows` from staging cleanup (#179).
- **Retention is not bounded by anything but row deletion.** A crash
  during a remux can leave a partial `final.<n>.<ext>`, and a successful
  retry preserves all prior siblings. They are reclaimed when the row is
  dropped (`clean_staging`), not before.
- **Findability.** The recording is preserved under
  `<temp>/grab-video/<id>/`. It is not surfaced to the user and the row
  does not report its path. Making it findable needs a product answer,
  not a mechanism — and the obvious mechanism (move it beside the
  destination) is the one shown above to be unreachable.

## Not here

- **#179.** `remove` should abort a live worker rather than signal it,
  which makes its scratch reclaimable inline with no finalizer to race.
  That is a separate change to `DownloadManager`; it is what removes the
  deferred/tombstoned cleanup and its generation, path-reuse and
  stranded-request races.
- **A durable staging key.** Needs either a persisted row identity (a
  `StoredItem` change plus a `QUEUE_VERSION` bump and migration) or a
  startup reconciliation pass. Both are real work.
- **Route-aware cleanup.** Making the HLS and unified paths able to
  distinguish their own temp from a live attempt's.
- **Bounded retention.** Reclaiming superseded siblings on a later
  successful attempt, or at startup.
- **Findability.** See above.

## Commands

```bash
cargo fmt --check                                  # must be clean
cargo clippy --all-targets -- -D warnings          # must be zero warnings
cargo test -- --test-threads=1                     # serial; parallel aborts on env races
```

Architecture guard (via the sentrux tool, not the shell):
`scan({ path: "/home/houssem/Projects/grab" })` then `check_rules()`.

At time of writing: 410 tests green, `max_cycles = 0`, quality_signal 6255.

## Testing Strategy

Serial, TDD, in the existing sibling `*_tests.rs` files. Every test is
verified non-vacuous by removing its guard and observing the failure.

| Test | Proves |
|---|---|
| `a_successful_retry_leaves_the_previous_attempts_remux_alone` | Attempt 1 parks nothing but keeps its remux; a later successful attempt delivers its own and leaves attempt 1 byte-identical. |
| `a_sweep_never_removes_another_attempts_remux` | An attempt that fails before it even remuxes does not take a pre-existing completed remux with it. |
| `a_remux_slot_is_claimed_exactly_once` | A second claim cannot land on the first slot, an occupied temp is skipped, and the lease exists. |
| `a_sweep_removes_only_its_own_temp` | The sweep removes the temp it was handed, leaves a sibling, and reclaims the directory only once empty. |
| `rename_failed_exit_keeps_media_and_staging_but_sweeps_state` | `Staging::Keep` leaves both its own and a sibling temp. |
| `live_capture_remux_failure_sweeps_state_but_keeps_recording` | A failed ffmux that leaves a *partial* temp has it removed, while the raw recording survives. |
| `live_capture_adopts_part_and_remuxes` | A delivered capture leaves no staging behind. |

### Test hygiene

- **Never `remove_dir_all` a shared parent.** `staging_dir(id).parent()`
  is the global staging root; cleaning that wipes every other row's
  staging, including a concurrently running Grab. Clean the exact
  `staging_dir(id)` and the test's own directories only.
- **Never trigger a failure with a permission bit.** CI runs this suite
  as root in `container: fedora:44`, where `CAP_DAC_OVERRIDE` makes a
  read-only mode a no-op. A `chmod`-based fixture passes unprivileged and
  silently takes the happy path in CI. Use a structural trigger — the
  existing `fake_ffmpeg_breaking_dest_dir` replaces the destination
  directory with a regular file, so the final rename fails with
  `ENOTDIR` for every caller.
- A process-liveness oracle must distinguish *dead* from *zombie*: read
  `/proc/<pid>/stat`, not `kill(pid, 0)`. See `still_running`.

## Boundaries

**Always**
- The three gates above, green before every commit, and `check_rules`.
- Every new test verified non-vacuous by removing its guard.
- A slice leaves `main` deployable.

**Ask first**
- Any new crate or dependency.
- Any change to `PART_KINDS` — it is a deletion allowlist, so each entry
  is a class of user-visible file Grab may remove.
- Any change to `staging_dir`'s keying (see "Across a restart").

**Never**
- Delete a delivered file at `dest` — only scratch.
- Make staging cleanup recursive again.
- Return a remux slot that is not exclusively claimed.
- Let Stop, pause or cancel discard a recording. (Stop delivers the
  partial; that is unchanged.)
