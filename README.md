# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita, downloads over HTTP(S).

## Install

Get `Grab.flatpak` from the [latest release](https://github.com/houssemko/Grab/releases):

```bash
flatpak install --user Grab.flatpak
```

Then open Grab from the app grid. The GNOME 50 runtime comes from Flathub on its own.

## Notes for packagers

Flatpak-only: Grab ships as a Flatpak bundle and offers no system-wide install. To build it (needs Builder and the GNOME 50 SDK):

```bash
flatpak-builder --user --install build build-aux/io.github.houssemko.Grab.json
```

After changing dependencies, run `python3 build-aux/gen-cargo-sources.py` first. The Flatpak build is offline.
