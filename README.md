# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita.

## Install

Get `Grab.flatpak` from the [latest release](https://github.com/houssemko/Grab/releases):

```bash
flatpak install --user Grab.flatpak
```

## Notes for packagers

Flatpak-only.

```bash
flatpak-builder --user --install build build-aux/io.github.houssemko.Grab.json
```
