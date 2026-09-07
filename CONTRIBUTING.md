# Contributing to Grab

## Architecture

Single-binary GTK 4 / libadwaita app (`src/`):

| File | Role |
|------|------|
| `main.rs` | Entry point, GSettings schema dir bootstrap |
| `application.rs` | Lifecycle (`startup`/`activate`/`open`/`shutdown`), actions, accels, About/Shortcuts dialogs |
| `window.rs` | Main window (header bar, `ViewStack` empty/list, failure `Banner`), New Download dialog |
| `download.rs` | `DownloadManager`: queue, tokio engine (`EngineMsg` channel), pause/resume/cancel/retry, atomic JSON persistence |
| `preferences.rs` | `AdwPreferencesDialog` bound to GSettings |

Threading rule: network I/O runs on a dedicated tokio runtime and only sends
`EngineMsg` over a channel; every GTK touch happens in `glib::spawn_future_local`
(main thread). GSettings keys live in `data/io.github.houssemko.Grab.gschema.xml`.

## Commands

```bash
cargo fmt --check          # must be clean
cargo clippy --all-targets -- -D warnings   # must be zero warnings
cargo test                 # 21 tests, incl. live-HTTP pause/resume/cancel
cargo run -- https://example.com/file.iso
```

CI (`.github/workflows/ci.yml`) runs all three on push/PR.

## UI changes

Follow the GNOME HIG: `AdwApplicationWindow` + `HeaderBar`, `AdwPreferencesDialog`
for settings, symbolic icons with tooltips *and* accessible labels, `AdwToast`
(+ Undo) for reversible actions, `AdwBanner` for persistent states. See
the HIG: <https://developer.gnome.org/hig/>.

## Releases (maintainers, Flatpak-only)

```bash
# 1. Bump version in Cargo.toml + metainfo, commit, push, tag (e.g. v0.2.1-alpha.5)
# 2. Regen vendored sources only if Cargo.lock gained/lost crates:
python3 build-aux/gen-cargo-sources.py
# 3. Build + install + smoke-test:
flatpak run org.flatpak.Builder --force-clean --user --install \
  build build-aux/io.github.houssemko.Grab.json
flatpak run io.github.houssemko.Grab --help
# 4. Bundle + publish (pre-release, matching previous notes style):
flatpak build-bundle ~/.local/share/flatpak/repo /tmp/Grab.flatpak io.github.houssemko.Grab
gh release create <tag> /tmp/Grab.flatpak --title "Grab <tag>" --prerelease --notes "..."
```
