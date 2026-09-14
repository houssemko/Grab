# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita.

## Install

From FlatPark (recommended, gets updates via `flatpak update`):

```bash
flatpak remote-add --if-not-exists flatpark https://flatpark.org/repo/flatpark.flatpakrepo
flatpak install flatpark io.github.houssemko.Grab
```

Or manually from the [latest release](https://github.com/houssemko/Grab/releases):

```bash
flatpak install --user Grab.flatpak
```

## Notes for packagers

Flatpak-only.

```bash
flatpak-builder --user --install build build-aux/io.github.houssemko.Grab.json
```
