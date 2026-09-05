#!/usr/bin/env sh
# Build the Cargo project and copy the binary to the Meson output path.
# Usage: cargo.sh <source-root> <output-bin> <profile>
set -eu
src="$1"; out="$2"; profile="${3:-release}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$CARGO_HOME/bin:$PATH"
# shellcheck disable=SC2086
cargo build --offline --profile "$profile" --manifest-path "$src/Cargo.toml" \
  ${CARGO_TARGET_DIR:+--target-dir "$CARGO_TARGET_DIR"}
bin="$CARGO_TARGET_DIR/$profile/wget-manager"
if [ ! -f "$bin" ]; then
  bin="$src/target/$profile/wget-manager"
fi
cp "$bin" "$out"
