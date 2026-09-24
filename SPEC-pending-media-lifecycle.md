# Spec: Pending Media Lifecycle

One piece covering GitHub issues [#178](https://github.com/houssemko/Grab/issues/178)
and [#179](https://github.com/houssemko/Grab/issues/179). They are the same
subsystem: **#178** is "where does un-claimable media live", **#179** is "what
happens to a recording whose row is removed".

Status: **awaiting approval**. No code until this is agreed.

## Objective

A live capture can produce a finished recording that Grab cannot place at its
destination, and a row can be removed while its worker is still recording.

- A recording that could not be placed is **kept, visibly, next to where the
  user wanted it**, and survives Retry, a second failure, and a restart. It is
  never overwritten.
- **Removing** a row discards its recording. **Stopping** a row keeps what it
  recorded. That distinction is the whole of #179.

Success looks like:

- A completed remux that could not be renamed is parked as a reserved
  part-namespace file beside the destination, visible in the user's folder, and
  it survives Retry, a *second* independent rename failure, and an app restart.
- A live row removed mid-capture leaves no dest-side scratch and no staging dir
  once the abort lands, and frees its concurrency slot.
- Nothing deletes a file already delivered at `dest`.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | **Removal disregards the recording; stopping keeps it.** `remove` aborts a live worker instead of signalling it. | Makes removal destructive and stop non-destructive, which is the honest reading of the two verbs. It also removes the entire deferred-cleanup mechanism: an aborted worker writes nothing, so `remove` can clean inline with no race. |
| D2 | **A park is a reserved part-namespace file beside the destination**: `<stem>.parked.<ext>`, built by `dest_part_path(&dest, "parked", ext)`. | Reuses three existing primitives instead of a new subsystem: `PART_KINDS` makes `stem_reserved_in` reserve the stem at intake, makes `clean_dest_parts` reclaim it with the row, and guarantees the finished file never matches. It is in the user's folder, so the recording is findable — and, critically, visible to the intake reservation snapshot, which a park under the temp root would not be. |
| D3 | **No park store, no UUID, no temp dir, no reconcile pass.** | The first draft had a `park/<uuid>/` store under the temp root with a filesystem index and age-based expiry. D2 removes the need for all of it: the park is an ordinary reserved file, so intake reservation and row-delete reclamation are already the lifecycle. There is no new state to reconcile after a crash — the file is simply there. |
| D4 | **Park names are numbered via the existing `dedupe_filename`**: `v.parked.mp4`, `v.parked (2).mp4`, … | A row can fail to place its recording more than once, and each failure is an independent completed recording. Numbering means a second park can never collide with the first, so no-clobber does not depend on `rename_noreplace` alone — which matters, because that helper falls back to a plain `std::fs::rename` (and therefore *does* clobber) where hard links are unsupported. |
| D5 | **A successful Retry drops every park for that stem.** | The retry delivered a file, so its parks are duplicates of the same capture. Dropping all is the only unambiguous rule; picking one would leave the others stranded. |
| D6 | **Placement: `video_staging` only; no new module.** | `video_staging` is already a leaf owning `staging_root`, `clean_dest_parts`, `dest_part_path` and `PART_KINDS`, and already depends on the `file_names` leaf. Keeps `max_cycles = 0` in `.sentrux/rules.toml` green. |
| D7 | **Scope is the live path only.** | `run_unified_ytdlp` and `run_hls_ytdlp` also perform a final `rename_noreplace` and can fail unexpectedly, leaving completed output in ordinary staging. Covering them is the same mechanism but a second integration, and is filed as a follow-up rather than silently claimed here. |
| D8 | **Undo restores the row, not the recording.** | `RemovedSnapshot` carries progress and segment bitmaps, never media. Since removal discards the recording (D1), an Undo after a remove restores a row whose parked copy is already gone. That is a direct consequence of the user's decision, stated here so it is not mistaken for an oversight. |

## Design

### One new infix

```rust
// src/video_staging.rs
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live.", "parked."];
```

That single edit is most of the mechanism. `is_grab_part` is a prefix match over
this list, so adding `parked.` simultaneously:

- makes `stem_reserved_in` report the stem as taken, so **intake cannot claim
  it** — the path-reuse race from the first draft is closed by construction;
- makes `clean_dest_parts` sweep `<stem>.parked.*` when the row is removed;
- leaves the finished `<stem>.<ext>` unmatched, because no infix is a prefix of a
  real extension.

### Parking (src/video_runner.rs)

In `run_live_ytdlp`, the arm that handles an unexpected final-rename failure
(where `sweep_live_capture(..., Staging::Keep, Exit::RenameFailed)` runs today):

1. Choose a free park name for this stem: `dedupe_filename("<stem>.parked.<ext>",
   |n| dest_dir.join(n).exists())`, which yields `v.parked.mp4` then
   `v.parked (2).mp4` and so on. Numbering is what makes two independent parks
   coexist without either clobbering the other (D4).
2. `rename_noreplace(&final_tmp, &parked)`.
3. On success the recording is out of staging's reach; `Staging::Keep` has
   nothing left to protect, and the row fails with the rename error as before.
4. On failure (cross-device staging, permissions on the destination) fall through
   to today's behaviour exactly: leave `final_tmp` in staging, keep the raw
   shell. Nothing is destroyed either way.

**The fallback is not durable, and that is pre-existing, not a regression.** If
the park cannot be created, the recording stays in `staging/<id>/final.<ext>`,
which a later Retry can sweep. That is exactly the behaviour on `main` today.
Fixing it needs a durable store, which D3 deliberately declines; the durable
case is the one where parking succeeds.

The error surfaced to the row names the parked path, so the failure is
actionable instead of mysterious. Translatable (`gettext`) like every other
user-visible string.

### Removal (src/download.rs)

`remove` currently skips its cleanup for live rows, because the worker is only
signalled and is still finalizing. Under D1 that reasoning no longer applies to
*removal*:

1. `remove` aborts the live worker's task handle, exactly as it already does for
   non-live rows, and drops the `running` slot. The guards shipped in
   `5836599`/`494bf23` make the abort safe: `ProcessGroupGuard` kills the process
   group, `LiveScratchGuard` removes the `.ytdl` state.
2. `remove` then sweeps staging and dest parts inline, as it does for every
   other video row — including any `<stem>.parked.*` file, which `PART_KINDS`
   now sweeps for free.
3. Stop, pause and cancel are **unchanged**: they still only signal, so the
   worker adopts the partial and delivers it.

**Abort is asynchronous, so the sweep can overlap a still-dying recorder.** That
is safe here, and the reason matters: the recording is being *discarded*, so
deleting a name a dying process still has open is harmless. Unlink gives the
writer a valid fd on an orphaned inode; it can waste a little I/O until it
exits, and the inode is then freed. Nothing can read a name that no longer
resolves, and no completed file can be corrupted, because nothing is reading
it. The same argument covers `unremove` racing the dying worker: a new attempt's
preflight or per-attempt reset may unlink `foo.live.*` out from under it, which
only discards bytes already being discarded.

**One race remains, and it is benign.** An abort can land after the worker has
already completed `rename_noreplace` to `dest`. In that window a complete file
appears in the user's folder. We do not delete it, and should not — a delivered
file is never removed by `remove`, for any row type.

Note also that `live_rows` can be *false* while a live worker is already running
(dialog-less rows start `is_live = false` and flip it via an async `LiveDetected`
message). `remove` therefore may take the non-live path for a genuinely live
capture. That is fine under the argument above, and it is why this spec needs no
"is it really live" gate.

### Dropping a park on delivery

On a successful capture, `sweep_live_capture(Exit::Delivered)` already removes
the dest-side media. Extend it to remove every `<stem>.parked*` file for that
stem (D5), so a retry that succeeds leaves nothing behind. A failure path must
leave the parks alone.

Because parks live in the part namespace, **every existing path that already
calls `clean_dest_parts` reclaims them with no new code** — `remove`,
`clear_finished`, and `drop_finished_row` among them. Implementation must
*verify* each of those call sites rather than assume it; if one does not call
`clean_dest_parts`, it needs the call added.

## Commands

Unchanged from `CONTRIBUTING.md`; all three gate every slice.

```bash
cargo fmt --check                                  # must be clean
cargo clippy --all-targets -- -D warnings          # must be zero warnings
cargo test -- --test-threads=1                     # serial; parallel aborts on env races
```

Plus the architecture guard, since this touches module boundaries (via the
sentrux tool, not the shell): `scan({ path: "/home/houssem/Projects/grab" })`
then `check_rules()`.

Baseline at time of writing: 408 tests green, quality_signal 6253,
`max_cycles = 0`.

## Project Structure

```
src/video_staging.rs   PART_KINDS gains "parked."; park-name + reclaim helpers
src/video_runner.rs    park on rename failure; drop parks on delivery
src/download.rs        remove aborts live workers; inline sweep; unremove guard
src/video_runner_tests.rs  private-item policy tests (existing sibling file)
src/video_tests.rs     live-capture integration tests
src/download_tests.rs  row-lifecycle tests
SPEC-pending-media-lifecycle.md   this document
```

No new crate, no new module, no new test file, no queue-schema change.

## Code Style

Follow the surrounding code, which is unusually deliberate about *why*:

```rust
/// Grab-namespaced part infixes: the only names `clean_dest_parts` ever
/// touches. The finished file itself (`<stem>.<ext>`) never matches.
const PART_KINDS: &[&str] = &["video.", "audio.", "hls.", "live."];
```

- Doc comments state the reason and the failure mode, not the mechanism.
- Comments explain what their absence would cost, and say so when a decision was
  deliberate ("bounded on purpose", "reverted because…").
- Production code uses fully-qualified `crate::` paths; `*_tests.rs` uses
  explicit `use`.
- `#[allow(dead_code)]` requires a written reason.
- No new `unsafe` beyond the existing `killpg` in `video_spawn.rs`.

## Testing Strategy

Serial suite, TDD, in the existing sibling test files. Every test must fail when
its specific guard is removed — verified by mutation, not assumed.

| Test | Proves |
|---|---|
| `parked.` is a part infix | `is_grab_part("v.parked.mp4", "v")`, and still not for the finished `v.mp4`. |
| A park reserves the stem | Intake dedupes away from a stem that hosts a parked file. |
| Row removal reclaims a park | `clean_dest_parts` removes `<stem>.parked*`. |
| Every row-drop path reclaims parks | `remove`, `clear_finished` and `drop_finished_row` each leave no park behind — verified per call site, not assumed. |
| A rename failure parks the remux | The remux lands at `<stem>.parked.<ext>` with its bytes, and nothing is left in staging. |
| Two failures produce two parks | Attempt 1 parks `v.parked.mp4`; attempt 2 parks `v.parked (2).mp4`; both retain their own bytes. |
| The park fallback destroys nothing | When the park cannot be created, `final.<ext>` stays in staging and the raw shell survives — today's behaviour, pinned so a future change cannot make it worse. |
| A park survives a Retry | Attempt 1 parks; attempt 2 runs to completion; the park still exists byte-identical. |
| A successful Retry drops every park | All `<stem>.parked*` files are gone after delivery. |
| A failed Retry keeps the parks | Remux failure leaves them untouched. |
| Removal discards an in-flight recording | Abort a live capture, remove the row, assert no `.live.*`, no `.parked*`, no staging dir. |
| Removal before `LiveDetected` | Removing a dialog-less row mid-capture still discards cleanly, covering the `live_rows == false` window. |
| Stop keeps the recording | The existing adopt-partial path still delivers; unchanged by this spec. |
| Undo does not resurrect media | `unremove` restores the row; the discarded park stays gone (D8). |
| Undo cannot claim a parked stem | The `unremove` guard rejects a stem with an outstanding park. |
| Removal frees the slot | After removal the row is absent from `running`. |
| A parked file is never adopted as output | `unified_candidate`, `discover_ytdlp_output` and `is_ytdlp_fragment` all reject `<stem>.parked*`. |
| A pre-existing symlink at the park path | Parking does not follow it: `rename` replaces the link, it does not write through it. |

Test hygiene, learned the hard way during this work:

- **Never `remove_dir_all` a shared parent.** `staging_dir(id).parent()` is the
  global `<staging_root>`; cleaning that wipes every other row's staging,
  including a concurrently running Grab. Clean the exact `staging_dir(id)` and
  the test's own dest dir only.
- Per-test unique temp paths, cleaned on failure too (RAII), not just at the end
  of a passing test.
- A process-liveness oracle must distinguish *dead* from *zombie*: check
  `/proc/<pid>/stat` state, not `kill(pid, 0)`. See `still_running` in
  `src/video_tests.rs`.

## Boundaries

**Always**
- Serial tests, and the three gates above, green before every commit.
- `check_rules` green — this change touches module boundaries.
- Every new test verified non-vacuous by removing its guard and observing the
  failure.
- A slice leaves `main` deployable; no half-implemented lifecycle on `main`.

**Ask first**
- Any new crate or dependency (this spec assumes none).
- Widening `PART_KINDS` beyond `parked.` — the list is a deletion allowlist, so
  every entry is a class of file Grab may remove.
- Changing what Stop does (it must keep delivering the partial).
- Any change to `StoredItem` or `QUEUE_VERSION`.

**Never**
- Delete a delivered file at `dest` — only scratch.
- Overwrite an existing park. Numbering (D4) is what guarantees this, not
  `rename_noreplace`: that helper falls back to a plain `std::fs::rename` where
  hard links are unsupported, and that fallback *does* replace the destination.
- Let Stop, pause or cancel discard a recording.
- Re-introduce a deferred/tombstoned cleanup: with removal aborting the worker,
  there is no finalizer to race, and a tombstone reintroduces the generation,
  path-reuse and stranded-request races that got the first attempt reverted.
- Follow a symlink when parking. `rename` replaces a symlink at the target path
  rather than writing through it, and `clean_dest_parts` already skips
  non-files — but keep it that way deliberately, since the destination folder is
  user-writable.

## Success Criteria

- [ ] #178: a remux that could not be renamed is parked visibly beside the
      destination, survives Retry, a *second* rename failure (as a separate
      numbered park), and a restart, and is never overwritten.
- [ ] #179: removing a live row discards its recording and leaves no scratch;
      stopping keeps it. The concurrency slot is freed.
- [ ] Intake cannot claim a stem with an outstanding park, and neither can
      `unremove`.
- [ ] Every row-drop path reclaims parks, not just `remove`.
- [ ] No user-visible output-discovery path can adopt a `.parked.*` file.
- [ ] Each test in the table above fails when its guard is removed.
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test -- --test-threads=1` (408 + new) green; `check_rules` passes
      with `max_cycles = 0`.

## Out of Scope

Recorded so these are not mistaken for oversights:

- **The VOD equivalent.** `run_unified_ytdlp` and `run_hls_ytdlp` also perform a
  final `rename_noreplace` and can leave completed output in staging on an
  unexpected failure. Same mechanism, separate integration (D7).
- **Crash debris.** A process kill mid-remux, or a crash between `remove` and
  the abort landing, can leave `staging/<id>/final.*` or dest-side `live.*` with
  no row and no park. Pre-existing, unrelated to the two issues, and not fixed
  here.
- **The unparkable fallback.** When a park cannot be created, the recording's
  durability is today's behaviour. A durable store is what D3 declines.
- **A user-facing recovery UI.** There is none. The parked file is a visible
  file; presenting it in the app is a separate product decision.

## Risks

| Risk | Mitigation |
|---|---|
| A `.parked.*` file is mistaken for a candidate download output | **Checked, and clear.** No discovery path can match it: `discover_ytdlp_output` keys on a `<stem>.hls.` prefix; `unified_candidate` requires a `grab-media.` prefix; `is_ytdlp_fragment` needs a `.f<digits>.` segment; and `container_truth_name` is handed a specific discovered path rather than scanning. Still worth a test pinning it, so a future pattern change cannot start adopting parked files. |
| The parked file is never cleaned up if the user ignores the failure | It is a visible file in their Downloads, reclaimable with the row. Bounded and discoverable — the point of D2. |
| Removing the row loses a recording the user wanted | That is D1, decided deliberately: Stop is the non-destructive verb. Undo restores the row but not the media (D8). |
| `rename_noreplace` fails across filesystems | staging and the destination can be on different filesystems (staging is often tmpfs). The fallback keeps today's behaviour; the recording is never lost, only not parked. |
| `rename_noreplace`'s no-clobber guarantee is weaker than it looks | It falls back to a plain rename where hard links are unsupported, and that *does* replace. This is why D4 uses numbered names via `dedupe_filename`, so two parks cannot target the same path at all rather than relying on the helper. |
| `PART_KINDS` grows a footgun entry | Each entry is a class of user-visible file Grab may delete. Enumerated in Boundaries as "ask first". |
| Numbered parks accumulate if a row fails to place repeatedly | Each failure is a distinct completed recording, so keeping them is correct. They are all reclaimed together when the row is delivered or dropped. |

## Open Questions

None blocking. One item is implementation detail rather than a decision:

- The row's failure message should name the parked path, so a permissions
  failure is actionable instead of mysterious. Exact wording is up to
  implementation, but it must be translatable (`gettext`) like every other
  user-visible string.
