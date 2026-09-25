## Task 1: Lock layering with .sentrux/rules.toml — DONE (2026-09-23)

**Result:** `max_cycles=0` FAILED (2 cycles among cookies/download/torrent/video/install_help/window); locked at `max_cycles=2`, `check_rules` passes, rescan 4773.

**Description:** Create `.sentrux/rules.toml` encoding the current clean layering so future back-edges fail `check_rules`.

**Acceptance criteria:**
- [ ] `.sentrux/rules.toml` exists and `check_rules` passes
- [ ] `rescan` quality_signal >= 4708 baseline

**Verification:**
- [ ] Tests pass: `cargo test -- --test-threads=1` (no regression, unchanged code)
- [ ] Build succeeds: `cargo clippy --all-targets -- -D warnings`
- [ ] Manual check: `check_rules` output clean via Sentrux

**Dependencies:** None

**Files likely touched:**
- `.sentrux/rules.toml`

**Estimated scope:** Small: 1-2 files

## Task 2: Reconcile Sentrux coverage vs cargo reality — DONE (2026-09-23)

**Result:** 387 real tests (video 252, download 109, torrent 24, window 1, application 1) vs Sentrux 35/0.10 (counts non-Rust files). Real gaps: window, application, cookies, settings, preferences, install_*. Note in plan.md.

**Description:** Document why Sentrux reports 35 untested / 0.10 while cargo runs 387 tests; record true per-module counts and decide if Sentrux needs `*_tests.rs` mapping or just a note.

**Acceptance criteria:**
- [ ] Note in `tasks/plan.md` or docs with `cargo test -- --list` counts vs Sentrux `test_gaps` counts
- [ ] No code change unless mapping config is trivial and safe

**Verification:**
- [ ] Tests pass: `cargo test -- --test-threads=1` count recorded
- [ ] Build succeeds: `cargo fmt --check`
- [ ] Manual check: `test_gaps` re-run recorded

**Dependencies:** Task 1

**Files likely touched:**
- `tasks/plan.md`
- `.sentrux/rules.toml` (only if coverage mapping exists)

**Estimated scope:** Small: 1-2 files

## Checkpoint: After Tasks 1-2
- [ ] All tests pass
- [ ] Application builds without errors
- [ ] `session_end` diff vs 4708 shows no degradation
- [ ] Review with human before proceeding

## Task 3: Window pure-helper tests (TDD) — DONE (2026-09-23)

**Result:** 5 new tests in `src/window_tests.rs`, suite 387 -> 392 green, fmt/clippy clean.

**Description:** RED-GREEN: add unit tests for pure window helpers (`playlist_count_label`, `fmt_item_duration`, plus `default_name_for` edge cases). No GTK widget construction in tests.

**Acceptance criteria:**
- [ ] RED: new tests fail before impl change (or pass only because helpers already correct — then assert edge cases that currently lack coverage)
- [ ] GREEN: `cargo test window -- --test-threads=1` passes, total suite 387+N passes
- [ ] No new clippy/fmt warnings

**Verification:**
- [ ] Tests pass: `cargo test -- --test-threads=1`
- [ ] Build succeeds: `cargo clippy --all-targets -- -D warnings`
- [ ] Manual check: `window_tests.rs` covers queued/paused/terminal + pure helpers

**Dependencies:** Tasks 1-2

**Files likely touched:**
- `src/window.rs`
- `src/window_tests.rs`

**Estimated scope:** Small: 1-2 files

## Task 4: Oversized-module split RFC (plan-only) — DONE (2026-09-23)

**Result:** RFC in `tasks/plan.md` (cycle-break first: engine_msg/media_types/torrent_names leaves; then 12 S-slices). No code moved; awaits approval.

**Description:** Produce a decomposition RFC for `video.rs` (5327), `download.rs` (4535), `window.rs` (3131): proposed submodules, move map, dependency direction, stacked S-task order. No code moves in this task.

**Acceptance criteria:**
- [ ] RFC lists target modules, what moves where, and why layering stays clean (0 above-diagonal)
- [ ] Each proposed move is S-size (1-2 files, one concept)
- [ ] Explicit non-goals (no behavior change)

**Verification:**
- [ ] Tests pass: unchanged suite still `cargo test -- --test-threads=1`
- [ ] Build succeeds: `cargo clippy --all-targets -- -D warnings`
- [ ] Manual check: human approves RFC before any split

**Dependencies:** Task 3

**Files likely touched:**
- `tasks/plan.md` (append RFC)

**Estimated scope:** Medium: 3-5 files read, 1 doc written

## Checkpoint: After Tasks 3-4
- [ ] End-to-end flow works
- [ ] All tests pass, builds clean
- [ ] Review with human before splits

## Task 5: Dead-code + docs sweep — DONE (2026-09-23)

**Result:** 1 dead-code site (`src/main.rs:14`, keep); CONTRIBUTING table drift filed as docs follow-up; no deletions.

**Description:** Review `#[expect(dead_code)]` on `mod video`, stale comments, and README/CONTRIBUTING drift. Ask before deleting anything.

**Acceptance criteria:**
- [ ] List of dead-code candidates presented, none deleted without approval
- [ ] Docs drift fixed or filed as follow-up

**Verification:**
- [ ] Tests pass: `cargo test -- --test-threads=1`
- [ ] Build succeeds: `cargo fmt --check`
- [ ] Manual check: dead-code list in review comment

**Dependencies:** Task 4

**Files likely touched:**
- `src/main.rs`
- `README.md`
- `CONTRIBUTING.md`

**Estimated scope:** Small: 1-2 files

## Task 6: Engine-cycle refactor (slices A-D) — DONE (2026-09-23)

**Result:** Both import cycles broken; `check_rules` passes at `max_cycles = 0`
(rules.toml tightened + comment updated). Quality 4719 -> 6059 across the
phase. Branch `refactor/break-engine-cycles`: f8814ec (A), ccd789f (B),
c94c9be (C), 9c3e029 (D). Suite 392 green throughout.

**Slices:** A `media_types.rs` (VideoSource/Choices/Playlist*/quality/combo;
`PlaylistKind::classify` bumped to `pub(crate)`; 2 latent dead-code warnings
surfaced + documented with reasoned allows); B `engine_msg.rs` (EngineMsg +
DEST_EXISTS); C `file_names.rs` (filename primitives, atomic rename, piece
sizing, fmt_bytes; `normalize_url` stays — torrent-coupled); D `runtime.rs`
(tokio_rt, lock_recover), `net_types.rs` (ResolvedProxy struct+method, impls
stay), `ui_util.rs` (close_on_click, needed adw prelude mirror).
Conventions: fully-qualified `crate::` paths in prod code (repo style),
explicit `use` only in `*_tests.rs`; no `pub use` shims.

**Next:** Task 4 RFC god-file splits (12 S-slices) — facades WITH re-exports
are safe there since remaining edges are one-way. Awaiting go.
