#!/usr/bin/env bash
# Build and stage the Rust 1.97 preview companion used by lege-filed.
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
MANIFEST="$REPO_ROOT/helpers/lege-preview-codecs/Cargo.toml"
PREVIEW_TARGET_DIR="$REPO_ROOT/target/preview-codecs-build"
STAGE_DIR="$REPO_ROOT/target/preview-codecs"
TOOLCHAIN=${LEGEOS_PREVIEW_TOOLCHAIN:-1.97.1}

if ! rustup toolchain list | grep -q "^${TOOLCHAIN}"; then
    echo "preview codecs require Rust ${TOOLCHAIN}; install it with: rustup toolchain install ${TOOLCHAIN}" >&2
    exit 1
fi

# The companion intentionally consumes mutable sibling path dependencies. Their
# manifests can change without this repository changing, which makes Cargo's
# saved dependency graph stale. Re-resolve from the local cache before using
# --locked so packaging remains offline and the subsequent build is repeatable.
cargo "+${TOOLCHAIN}" metadata --offline --manifest-path "$MANIFEST" --format-version 1 >/dev/null
cargo "+${TOOLCHAIN}" test --locked --manifest-path "$MANIFEST" --target-dir "$PREVIEW_TARGET_DIR"
cargo "+${TOOLCHAIN}" build --release --locked --manifest-path "$MANIFEST" --target-dir "$PREVIEW_TARGET_DIR"

install -d "$STAGE_DIR/licenses"
install -m 0755 "$PREVIEW_TARGET_DIR/release/lege-preview-codecs" "$STAGE_DIR/lege-preview-codecs"
install -m 0644 "$REPO_ROOT/../jpegXL-rs/JPXL/LICENSE-MIT" "$STAGE_DIR/licenses/JPXL-LICENSE-MIT"
install -m 0644 "$REPO_ROOT/../Lege-ecosystem/LICENSE" "$STAGE_DIR/licenses/LEGE-PDF-LICENSE"
"$STAGE_DIR/lege-preview-codecs" --self-check
