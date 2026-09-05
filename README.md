# Grab (`org.gnome.WgetFrontend`)

A GNOME download manager frontend for GNU wget — GTK 4 + libadwaita, written in Rust.

## Quick start (no install)

```bash
# needs: rust stable, gtk4-devel, libadwaita-devel, wget
cargo run --release -- https://example.com/file.iso
# or: echo 'https://example.com/a.iso' > urls.txt && cargo run -- urls.txt
```

`build.rs` compiles the GSettings schema into `target/` and points
`GSETTINGS_SCHEMA_DIR` at it, so plain `cargo run` just works. A
system-installed schema (or a pre-set `GSETTINGS_SCHEMA_DIR`) always wins.

## Debug

```bash
GTK_DEBUG=interactive cargo run        # GTK Inspector (Ctrl+Shift+D)
G_MESSAGES_DEBUG=all cargo run
G_DEBUG=fatal-criticals cargo run     # abort on criticals
cargo test                             # parser/argv unit tests
```

## Install for the current user (dash icon, no sudo)

```bash
cargo build && sh build-aux/install-user.sh  # then restart the app
```

## Install system-wide

```bash
# needs: meson, ninja, blueprint not required (UI is code-built)
meson setup build && ninja -C build && sudo ninja -C build install
```

## Flatpak

```bash
flatpak install --user flathub org.flatpak.Builder org.gnome.Sdk//48 org.gnome.Platform//48
flatpak-builder --user --install build build-aux/org.gnome.WgetFrontend.json
```

Notes: file picking uses `GtkFileDialog` (portal-safe); downloads default to
`xdg-download` (`--filesystem=xdg-download` in the manifest).

## Design rules (from `developing-gtk-apps`)

- Single-threaded UI: wget runs as `gio::Subprocess`, stderr is drained in a
  `glib::spawn_future_local` future. Never touch widgets off the main thread.
- Pause = `SIGSTOP`, resume = `SIGCONT`, resume-after-restart = `wget -c`.
- Actions live on `app.*`; accelerators: `Ctrl+N` new, `Ctrl+,` prefs, `Ctrl+Q` quit.
- No deprecated widgets: `AdwDialog`/`AdwAboutDialog`, `AdwSpinner`, `.dimmed` class.
