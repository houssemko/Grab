#!/usr/bin/env python3
"""Generate build-aux/cargo-sources.json from Cargo.lock for offline Flatpak builds.

Same output shape as flatpak-builder-tools' flatpak-cargo-generator.py for
registry-only locks (all our deps are crates.io): per-crate archives +
.cargo-checksum.json inlines + a vendored-sources cargo config inline.
Stdlib only (tomllib), so it runs anywhere. Re-run after every Cargo.lock
change and commit the result.
Usage: ./build-aux/gen-cargo-sources.py   (run from the repo root)
"""
import json
import sys
import tomllib

LOCK = "Cargo.toml"
OUT = "build-aux/cargo-sources.json"
CRATES_IO = "https://static.crates.io/crates"


def main() -> None:
    with open("Cargo.lock", "rb") as f:
        lock = tomllib.load(f)
    metadata = lock.get("metadata", {})
    sources: list = []
    n = 0
    for pkg in lock["package"]:
        source = pkg.get("source", "")
        if not source.startswith("registry+"):
            print(f"SKIP non-registry package: {pkg['name']}", file=sys.stderr)
            continue
        name, version = pkg["name"], pkg["version"]
        key = f"checksum {name} {version} ({source})"
        checksum = metadata.get(key, pkg.get("checksum"))
        if not checksum:
            sys.exit(f"ERROR: no checksum for {name} {version}")
        dest = f"cargo/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"{CRATES_IO}/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )
        n += 1
    config = (
        '[source.crates-io]\nreplace-with = "vendored-sources"\n'
        '[source.vendored-sources]\ndirectory = "cargo"\n'
    )
    sources.append(
        {
            "type": "inline",
            "contents": config,
            "dest": "cargo",
            "dest-filename": "config",
        }
    )
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(sources, f, indent=4)
        f.write("\n")
    print(f"wrote {OUT} with {n} crates")


if __name__ == "__main__":
    main()
