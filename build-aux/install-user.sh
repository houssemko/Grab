#!/usr/bin/env sh
# User-local install (no sudo needed): desktop entry, icon, GSettings schema.
# Run after every `cargo build` you want reflected in the dash. The installed
# desktop entry points at target/debug; for an optimized build, set BIN=...
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
bin="${BIN:-$root/target/debug/wget-manager}"
prefix="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$prefix/applications" "$prefix/icons/hicolor/scalable/apps" "$prefix/glib-2.0/schemas"
sed "s|^Exec=.*|Exec=$bin %U|" "$root/data/org.gnome.WgetFrontend.desktop.in" \
    > "$prefix/applications/org.gnome.WgetFrontend.desktop"
cp "$root/data/icons/hicolor/scalable/apps/org.gnome.WgetFrontend.svg" \
    "$prefix/icons/hicolor/scalable/apps/"
cp "$root/data/org.gnome.WgetFrontend.gschema.xml" "$prefix/glib-2.0/schemas/"
glib-compile-schemas "$prefix/glib-2.0/schemas/"
command -v update-desktop-database >/dev/null \
    && update-desktop-database "$prefix/applications" || true
command -v gtk-update-icon-cache >/dev/null \
    && gtk-update-icon-cache -f -t "$prefix/icons/hicolor" >/dev/null || true
echo "installed Grab -> $prefix (restart the app)"
