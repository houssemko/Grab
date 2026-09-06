# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita, downloads over HTTP(S).

## Install

Get `Grab.flatpak` from the [latest release](https://github.com/houssemko/Grab/releases):

```bash
flatpak install --user Grab.flatpak
```

Then open Grab from the app grid. The GNOME 50 runtime comes from Flathub on its own.

## Try it without installing

Run Grab straight from source. Nothing is installed on your system.

**1. Install the build tools** (one time):

```bash
# Fedora / RHEL
sudo dnf install rustc cargo gtk4-devel libadwaita-devel
# Arch / CachyOS
sudo pacman -S rust gtk4 libadwaita
# Debian / Ubuntu
sudo apt install cargo libgtk-4-dev libadwaita-1-dev
```

**2. Get the code:**

```bash
git clone https://github.com/houssemko/Grab.git
cd Grab
```

**3. Run it:**

```bash
cargo run -- https://example.com/file.iso
```

The first build takes a few minutes. The app window opens and the download starts. Settings are stored per-user, so your system is untouched — delete the folder to remove everything.

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
