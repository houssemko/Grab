# Implementation Plan: Grab Health Follow-up

## Overview
Sentrux scan of `grab` (44 files, 37k lines, quality 4708, bottleneck modularity 2947) shows clean layering (0 above-diagonal) but 3 oversized modules, thin window tests, and no architectural guardrails. Baseline is green: `cargo fmt --check` clean, `clippy --all-targets -- -D warnings` clean, `cargo test -- --test-threads=1` 454 passed (462 after this PR). This plan locks the good layering, closes the real test gap, and stages decomposition without a big-bang refactor.

## Architecture Decisions
- Keep single-binary GTK4/libadwaita layout from CONTRIBUTING.md; no new crates.
- Sentrux `dsm.size=14` covers the import graph core; `video.rs`/`download.rs`/`window.rs` are the hotspots (5.3k / 4.5k / 3.1k lines, all >1000-line inspection signal).
- Sentrux `test_gaps` (35 untested, score 0.10) undercounts Rust `#[cfg(test)]` inline + `*_tests.rs` modules (454 real tests). Fix the gap that is real: `window_tests.rs` has 1 test for 3131 lines.
- Threading rule stays: tokio runtime -> `EngineMsg` channel -> `glib::spawn_future_local` on main thread.
- Refactors split from behavior changes per code-review skill; each task leaves tree green.

## Task List

### Phase 1: Foundation
- [ ] Task 1: Lock layering with .sentrux/rules.toml
- [ ] Task 2: Reconcile Sentrux coverage vs cargo test reality

### Checkpoint: Foundation
- [ ] `check_rules` passes, `rescan` quality >= 4708
- [ ] Coverage note recorded, no suite regression

### Phase 2: Core
- [ ] Task 3: Window pure-helper tests (TDD)
- [ ] Task 4: Oversized-module split RFC (plan-only, no code moves)

### Checkpoint: Core
- [ ] `cargo fmt --check`, `clippy --all-targets -- -D warnings`, `cargo test -- --test-threads=1` all green
- [ ] `session_end` shows no degradation vs 4708 baseline

### Phase 3: Polish
- [ ] Task 5: Dead-code + docs sweep (`#[expect(dead_code)]` on video mod, README/CONTRIBUTING drift)

### Checkpoint: Complete
- [ ] All acceptance criteria met, review-ready

## Risks and Mitigations
| Risk | Impact | Mitigation |
|------|--------|------------|
| GTK tests need serial main-loop (`--test-threads=1`) | Med | Keep serial command in every task verification |
| Splitting 3 large files risks churn | High | Task 4 is RFC-only; moves happen as stacked S-tasks after approval |
| Sentrux Rust heuristics stay noisy | Low | Document real counts; use rules.toml only for layering, not coverage gates |
| Single-author bus factor (43/44 solo, 0.97) | Med | Small slices + docs, no heroics |

## Open Questions
- Which layering to enforce in rules.toml? Proposal: `main -> application -> window -> download/video/torrent/settings/preferences/cookies` with no back-edges (matches 0 above-diagonal).
- Scope Task 3 to pure fns only (`playlist_count_label`, `fmt_item_duration`, `default_name_for`)? GTK-widget helpers stay out.
- Accept file-split RFC as follow-up stack, not this batch?

## Task 1-2 Completion Notes (2026-09-23)

**Correction:** DSM `above_diagonal=0` does NOT mean acyclic. `check_rules(max_cycles=0)`
fails with 2 circular deps among `cookies/download/torrent/video/install_help/window`.
Runtime edges confirm at least `download<->video` (`VideoSource`/`combo_*` vs
`run_video_download(.. EngineMsg ..)`) and `download<->torrent`
(magnet/archive vs `EngineMsg`/`tokio_rt`). `rules.toml` locks `max_cycles=2`
to prevent new cycles; Task 4 RFC targets reduction toward 0.
`check_rules` now passes. Rescan quality 4773 (up from 4708 baseline).

**Coverage reconciliation (Task 2):** `cargo test -- --list` = 454 tests (462 with this PR):
video 252, download 109, torrent 24, window 1, application 1.
Sentrux `test_gaps` reports 39 source / 5 test files / 4 tested / 35 untested /
score 0.10 because it counts all 44 scanned files (data/, po/, build-aux,
meson, etc.) as sources while Rust tests live inline (`#[cfg(test)]`) +
`*_tests.rs` (which the Rust plugin does recognize). Real gaps: `window.rs`
3131 lines / 1 test, `application.rs` 454 / 1, and zero-test modules
(`cookies.rs` 229, `settings.rs` 184, `preferences.rs` 993, `install_*`).
Task 3 targets window pure helpers first.

## Task 3 Completion Notes (2026-09-23)

Added 5 tests to `src/window_tests.rs` (no impl change): `should_pulse` NaN/neg/inf
edges, `fmt_item_duration` boundaries + negative clamp, `playlist_count_label`
per-kind singular/plural + zero-plural. Suite 387 -> 392 passed, fmt/clippy clean.

## Task 4 RFC: Oversized-Module Split (plan-only, no code moved)

**Non-goals:** no behavior change, no path churn beyond re-exports, no GTK-test
broadening, no new crates.

**Cycle-breaking first (unblocks splits):** `download<->video` and
`download<->torrent` share one shape: engine owns `EngineMsg` + `tokio_rt`,
peers borrow the type back. Smallest fix: new leaf `src/engine_msg.rs`
(`EngineMsg`, status-report constructors, no crate deps); `download`, `video`
(`run_video_download` sender param), `torrent` depend downward on it.
`download`'s use of `video::VideoSource` (persisted identity) moves to a leaf
`src/media_types.rs` (`VideoSource`, no deps). `torrent` archive/magnet pure
helpers used by `download` intake move to `src/torrent_names.rs` (no deps).
Each is an S-task; re-run `check_rules` after each, target `max_cycles 2 -> 0`,
then tighten `rules.toml`.

**Stacked S-slices after cycles hit 0 (each: move + re-export + suite green):**
1. `video_types.rs`: `VideoSource/VideoInfo/ProbeResult/Playlist*` + `classify`.
2. `video_quality.rs`: `VIDEO_QUALITY_VALUES`, `quality_index/value`, `combo_*`,
   `quality_for_height`, `default_video_filename`.
3. `video_probe.rs`: `fetch_video_infos`, `preview_fresh`, `page_host`,
   `is_video_page/is_http_url/is_direct_file_url`, `drive_direct_url`.
4. `video_tools.rs`: `resolve_libraries`, `ensure_tool_versions`,
   `user_lib_dir`, `distro_packages`, version floor.
5. `video_worker.rs`: `run_video_download`, staging dirs, progress granularity.
   `video.rs` becomes facade re-exports (or `video/mod.rs` later — human call).
6. `download_names.rs`: `sane_filename/shorten_filename/dedupe_filename`,
   `normalize_url`, content-disposition parsing.
7. `download_net.rs`: proxy resolve/pool, `cookie_header_for` call sites,
   `publish_rate_limit`.
8. `download_queue.rs`: `DownloadManager` queue ops + `StoredQueue` persistence.
9. `download_engine.rs`: engine loop, segmented resume, pause/resume/cancel/retry.
10. `window_rows.rs`: `RowWidgets/LiveRow`, `build_row/refresh_row/upgrade_row`,
    `should_pulse` (tests move with it).
11. `window_dialogs.rs`: `show_add_dialog`, video steps, rename dialog.
12. `window_pick.rs`: playlist + torrent pickers, `playlist_count_label`,
    `fmt_item_duration` (tests move with them).

**Acceptance for each slice:** `cargo fmt --check`, `clippy --all-targets --
-D warnings`, `cargo test -- --test-threads=1` green; `check_rules` passes;
`rescan` quality does not drop vs pre-slice.

## Task 5 Notes: Dead-code + docs sweep (2026-09-23)

- Dead code: exactly one site, `src/main.rs:14` `#[expect(dead_code)]` on
  `mod video` (expiry helpers kept for a future no-re-resolve fast path).
  Proposing NO deletion — expectation still holds (clippy `-D warnings` green).
- Docs drift: `CONTRIBUTING.md` architecture table lists 5 files
  (`main/application/window/download/preferences`) but omits `cookies`,
  `settings`, `torrent`, `video`, `install_help`, `install_progress` and the
  `*_tests.rs` modules. Filed as follow-up (docs-only change, not in this batch).
- No other `TODO/FIXME/allow(dead_code)` hits in `src/`.

Tasks tracked in `tasks/todo.md`.
