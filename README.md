# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita, downloads with wget.

## Install

Get `Grab.flatpak` from the [latest release](https://github.com/houssemko/Grab/releases):

```bash
flatpak install --user Grab.flatpak
```

Then open Grab from the app grid. The GNOME 50 runtime comes from Flathub on its own.

## Try it without installing

You need Rust, GTK 4, libadwaita and wget:

```bash
cargo run -- https://example.com/file.iso
```

## Debug

```bash
GTK_DEBUG=interactive cargo run
G_MESSAGES_DEBUG=all cargo run
cargo test
```

## Notes for packagers

System-wide install (needs meson and ninja):

```bash
meson setup build && ninja -C build && sudo ninja -C build install
```

Flatpak (needs Builder and the GNOME 50 SDK):

```bash
flatpak-builder --user --install build build-aux/io.github.houssemko.Grab.json
```

After changing dependencies, run `python3 build-aux/gen-cargo-sources.py` first. The Flatpak build is offline.
