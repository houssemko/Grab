#!/usr/bin/env python3
"""Verify a built Flatpak bundle: does it contain what the release claims?

`flatpak build-bundle` embeds a commit manifest, but that manifest does
not record which dependency revisions were compiled in, so a tag whose
Cargo.lock moved after publication can ship stale dependencies with no
visible sign. This reads the bundle's own metadata and cross-checks it
against the release tag: the embedded app version and metainfo release
must match the tag, and the vendored-source list inside the bundle must
match build-aux/cargo-sources.json (which is derived from Cargo.lock).

Usage:
  ./build-aux/check-bundle-contents.py Grab.flatpak --tag v4.3.1

Exit codes: 0 consistent, 1 mismatch (message on stderr), 2 usage/IO error.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import tarfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CARGO_LOCK = ROOT / "Cargo.lock"
METAINFO = ROOT / "data" / "io.github.houssemko.Grab.metainfo.xml.in"
VENDOR = ROOT / "build-aux" / "cargo-sources.json"
RELEASE_MARKER = '<release version="'
METAINFO_PATH = "metainfo/io.github.houssemko.Grab.metainfo.xml"
APP_VERSION_PATH = "metadata/io.github.houssemko.Grab"


class Mismatch(Exception):
    """A consistency check failed; message is user-facing."""


def fail(msg: str) -> Mismatch:
    return Mismatch(msg)


def version_key(version: str) -> tuple[int, ...]:
    core = re.split(r"[-+]", version)[0]
    return tuple(int(part) if part.isdigit() else 0 for part in core.split("."))


def package_version() -> str:
    with CARGO_TOML.open("rb") as fh:
        return tomllib.load(fh)["package"]["version"]


def lock_version() -> str:
    with CARGO_LOCK.open("rb") as fh:
        lock = tomllib.load(fh)
    for pkg in lock["package"]:
        if pkg["name"] == "grab":
            return str(pkg["version"])
    raise fail("Cargo.lock has no grab package")


def metainfo_versions(xml: str) -> list[str]:
    versions = re.findall(r'<release version="([^"]+)"', xml)
    if not versions:
        raise fail("metainfo has no <release> entries")
    return versions


def expected_vendor_revisions() -> dict[str, str]:
    """Crate version per source entry in the committed vendor file."""
    data = json.loads(VENDOR.read_text(encoding="utf-8"))
    revisions = vendor_revisions(data)
    if not revisions:
        raise fail("cargo-sources.json has no crate download sources")
    return revisions


def vendor_revisions(data: object) -> dict[str, str]:
    """Crate version per archive source entry (flat flatpak-builder list)."""
    entries = data if isinstance(data, list) else data.get("sources", [])  # type: ignore[union-attr]
    revisions: dict[str, str] = {}
    for src in entries:
        if not isinstance(src, dict) or src.get("type") != "archive":
            continue
        # /crates/<name>/<name>-<version>.crate: the crate name may itself
        # contain dashes, so split on the last dash that starts a version.
        m = re.search(r"/crates/([^/]+)/([^/]+)-(\d[^-]*(?:-[^/]*)?)\.crate$", src.get("url") or "")
        if m:
            revisions[m.group(1)] = m.group(3)
    return revisions


def bundle_revisions(bundle: pathlib.Path) -> dict[str, str]:
    """Crate version per cargo-sources.json embedded in the bundle."""
    with tarfile.open(bundle, "r:gz") as tar:
        name = next(
            (n for n in tar.getnames() if n.endswith("cargo-sources.json")), None
        )
        if name is None:
            raise fail("bundle contains no cargo-sources.json")
        data = json.load(tar.extractfile(name))  # type: ignore[arg-type]
    return vendor_revisions(data)


def bundle_file(bundle: pathlib.Path, suffix: str) -> str | None:
    with tarfile.open(bundle, "r:gz") as tar:
        name = next((n for n in tar.getnames() if n.endswith(suffix)), None)
        if name is None:
            return None
        handle = tar.extractfile(name)
        return handle.read().decode("utf-8") if handle else None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=pathlib.Path, help="path to Grab.flatpak")
    parser.add_argument("--tag", required=True, help="release tag, e.g. v4.3.1")
    args = parser.parse_args(argv)

    want = args.tag[1:] if args.tag.startswith("v") else args.tag
    notes: list[str] = []

    try:
        version = package_version()
        lock = lock_version()
        if version != lock:
            raise fail(f"Cargo.toml version {version} != Cargo.lock version {lock}")
        if version != want:
            raise fail(f"tag {args.tag} (version {want}) != package version {version}")

        app_version = bundle_file(args.bundle, APP_VERSION_PATH)
        if app_version is None:
            raise fail(f"bundle has no {APP_VERSION_PATH}")
        app_version = app_version.strip()
        if app_version != version:
            raise fail(f"bundle app version {app_version} != tag version {want}")
        notes.append(f"bundle app version {app_version}")

        metainfo = bundle_file(args.bundle, METAINFO_PATH)
        if metainfo is None:
            raise fail(f"bundle has no {METAINFO_PATH}")
        releases = metainfo_versions(metainfo)
        newest = max(releases, key=version_key)
        if newest != version:
            raise fail(f"bundle metainfo newest release {newest} != version {version}")
        if releases[0] != version:
            raise fail(
                f"bundle metainfo lists {releases[0]} first, newest is {version}"
            )
        source = METAINFO.read_text(encoding="utf-8")
        if source != metainfo:
            raise fail(
                "bundle metainfo differs from the tag source: the release was "
                "tagged before the metainfo was updated"
            )
        notes.append(f"bundle metainfo newest-first release {newest}, matches tag source")

        expected = expected_vendor_revisions()
        shipped = bundle_revisions(args.bundle)
        if shipped != expected:
            missing = sorted(set(expected) - set(shipped))
            extra = sorted(set(shipped) - set(expected))
            changed = sorted(
                c for c in set(expected) & set(shipped) if expected[c] != shipped[c]
            )
            detail = []
            if missing:
                detail.append(f"missing: {', '.join(missing[:5])}")
            if extra:
                detail.append(f"extra: {', '.join(extra[:5])}")
            if changed:
                detail.append(
                    "version drift: "
                    + ", ".join(f"{c} {expected[c]}->{shipped[c]}" for c in changed[:5])
                )
            raise fail("bundle dependencies differ from the tag's lock: " + "; ".join(detail))
        notes.append(f"bundle vendors {len(expected)} crates matching the tag lock")

    except Mismatch as exc:
        print(f"bundle consistency check FAILED: {exc}", file=sys.stderr)
        return 1
    except (OSError, tarfile.TarError, json.JSONDecodeError, tomllib.TOMLDecodeError) as exc:
        print(f"bundle consistency check ERROR: {exc}", file=sys.stderr)
        return 2

    for note in notes:
        print(f"ok: {note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
