# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita.

## Screenshots

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="screenshots/dark-main.png">
    <img src="screenshots/light-main.png" alt="Grab main window with active downloads" width="720">
  </picture>
  <br>
  <em>Active downloads with per-segment progress maps</em>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="screenshots/dark-prefs-downloads.png">
    <img src="screenshots/light-prefs-downloads.png" alt="Grab preferences, Downloads tab" width="720">
  </picture>
  <br>
  <em>Preferences: destination, power, and notifications</em>
</p>

<p align="center">
  <img src="screenshots/light-prefs-network.png" alt="Grab preferences, Network tab" width="720">
  <br>
  <em>Preferences: simultaneous downloads, connections, retries, speed limit</em>
</p>

<p align="center">
  <img src="screenshots/light-prefs-torrent.png" alt="Grab preferences, Torrent tab" width="720">
  <br>
  <em>Preferences: seeding, DHT, and peer limit</em>
</p>

<p align="center">
  <img src="screenshots/dark-empty.png" alt="Grab empty state in dark mode" width="720">
  <br>
  <em>Empty state (dark mode)</em>
</p>

## Install

From FlatPark (recommended, gets updates via `flatpak update`):

```bash
flatpak remote-add --if-not-exists flatpark https://dl.flatpark.org/flatpark.flatpakrepo
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

## License

Grab is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3 only. See [LICENSE](LICENSE) for the full text.
