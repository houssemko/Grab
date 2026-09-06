# Grab

A GNOME download manager frontend for GNU wget — GTK 4 + libadwaita, written in Rust.

## Install (alpha)

Download `Grab.flatpak` from the [latest release](https://github.com/houssemko/Grab/releases), then:

```bash
flatpak install --user Grab.flatpak   # pulls the GNOME 50 runtime from Flathub
flatpak run io.github.houssemko.Grab  # or open Grab from the app grid
```

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

## Install for the current user

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
flatpak install --user flathub org.flatpak.Builder org.gnome.Sdk//50 org.gnome.Platform//50
flatpak-builder --user --install build build-aux/io.github.houssemko.Grab.json
```

After any dependency change, regenerate the vendored cargo sources first:
`python3 build-aux/gen-cargo-sources.py` (the Flatpak builds offline).

Notes: file picking uses `GtkFileDialog` (portal-safe); downloads default to
`xdg-download` (`--filesystem=xdg-download` in the manifest).

