# Handoff: engine-cycle refactor (for the Grab dev conversation)

Say: "continue the engine-cycle refactor from tasks/handoff-refactor.md".

## State
- Branch `refactor/break-engine-cycles`, stacked on `test/window-helper-coverage-and-rules`
  (PR #175, unmerged — do NOT merge #175's branch into this work; rebase when #175 lands).
- Tree clean. Only `tasks/` is untracked (this file, `plan.md`, `todo.md`).
- Sentrux `session_start` baselined at quality **4774** (call `session_end` when done).
- NO code changed yet in this phase — exploration only. Next action is Slice A below.

## Goal (approved with "go")
1. Break the 2 reported cycles toward `max_cycles = 0`, tighten `.sentrux/rules.toml`.
2. Then split the god files per the RFC in `tasks/plan.md` (Task 4 section).

## Cycle inventory (verified, runtime edges — `src/*.rs`, non-doc)
- SCC-1 `{download, video, torrent, cookies}`:
  - `video → download`: `tokio_rt` (video.rs:1011,1028,1143,1160,2200), `EngineMsg`
    (3551,4186,4589,5116 + `use` at 3553,4188,4591,5118), `ResolvedProxy`
    (2024,2101,2198,2712,2723,3533 + `proxy_cli_args` at 2712), `rename_noreplace`
    (3306,4261,4764,5254), `piece_len` (5171,5192), `filename_from_url` (112),
    `DEST_EXISTS` (511). (`normalize_url` at video.rs:262 is a DOC comment only.)
  - `download → video`: `VideoSource` ×13, `VideoJob`/`VideoOutcome`/`VideoChoices`,
    `PlaylistInfo` ×2, `combo_index`/`combo_value` ×4, plus `resolve_libraries`,
    `run_video_download`, `classify`-family and staging helpers (stay one-way, fine).
  - `torrent → download`: `EngineMsg`, `dedupe/sane/shorten_filename`
    (torrent.rs:27,130,205), `tokio_rt`, `ResolvedProxy` (+ `torrent_socks_url()`
    method, defined download.rs:564 — pure string op, moves with the struct).
  - `cookies → download`: `lock_recover` (cookies.rs:195,208; def download.rs:809,
    pure mutex-poison recovery). `cookies → video`: `staging_root`, `kill_tree`,
    `cookies_browser_spec` (one-way, stays).
  - `download → cookies`: `jar_for_browser`, `cookie_header_for` (one-way, stays).
- SCC-2 `{window, install_help}`: `window → install_help::show` (window.rs:2306),
  `install_help → window::close_on_click` (install_help.rs:57; def window.rs:72,
  also used window.rs:1467,2506). Break by moving `close_on_click` to leaf.
- One-way (leave alone): `settings → video` (`CODEC_PRIORITY_NEWEST`,
  `subtitle_lang_active`), `download → settings`, `window → download`
  (`DownloadManager` API — unavoidable, UI drives engine), `window → install_progress::run`.

## Slice plan (each: new leaf + move defs + update ALL paths incl. tests + verify + commit)
- **A — `media_types.rs`**: `PlaylistKind/Item/Info`, `VideoSource`, `VideoChoices`,
  `VIDEO_QUALITY_VALUES`, `default_video_quality`, `quality_index/value`,
  `combo_index/value`. Update: download.rs, window.rs, preferences.rs:613-614,
  download_tests.rs (`VideoChoices` ×7). NO `pub use` re-export (review: no shims);
  video_tests.rs bare names need explicit `use crate::media_types::…`.
- **B — `engine_msg.rs`**: `EngineMsg` + `DEST_EXISTS` const. Update: download.rs
  (def→import), video.rs, torrent.rs (`progress_msg`, import at :27), test files.
- **C — `file_names.rs`**: `filename_from_url`, `dedupe/shorten/sane_filename`
  (+`split_stem_ext`, `restrict_filename_ascii`/`fold_ascii_part`,
  `name_stem` helpers), `rename_noreplace(+_sys)`, `piece_len(+PIECE_* consts)`,
  `fmt_bytes` (download.rs:1556 — check body first). NOT `normalize_url`
  (calls `torrent::{is_magnet,parse_magnet,is_torrent_url,archive_path_for_url}`,
  can never be leaf — stays; its only video link is a doc comment).
- **D — leaves**: `runtime.rs` (`tokio_rt` + `lock_recover`), `net_types.rs`
  (`ResolvedProxy` struct with `pub(crate)` fields + `torrent_socks_url`;
  `http_client_for`/`system_proxy`/`manual_proxy` impls stay in download.rs),
  `ui_util.rs` (`close_on_click`, gtk/adw only).
- Then: `check_rules` → expect 0 cycles → set `max_cycles = 0` → rescan →
  god-file splits (adapted RFC: facades WITH re-exports are safe there since
  remaining edges are one-way).

## Verification per slice
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test -- --test-threads=1` (focused `cargo test <mod>` during dev, full at checkpoints).
Serial tests are mandatory (CONTRIBUTING.md). Sentrux `check_rules` after each slice.
