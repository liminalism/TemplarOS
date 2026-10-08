#!/usr/bin/env bash
# Build and stage the Rust 1.97 preview companion used by templar-filed.
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
MANIFEST="$REPO_ROOT/helpers/templar-preview-codecs/Cargo.toml"
PREVIEW_TARGET_DIR="$("$SCRIPT_DIR/native-target-dir.sh")/preview-codecs-build"
STAGE_DIR="$REPO_ROOT/target/preview-codecs"
TOOLCHAIN=${TEMPLAROS_PREVIEW_TOOLCHAIN:-1.97.1}

if ! rustup toolchain list | grep -q "^${TOOLCHAIN}"; then
    echo "preview codecs require Rust ${TOOLCHAIN}; install it with: rustup toolchain install ${TOOLCHAIN}" >&2
    exit 1
fi

# A production build must consume the declared lock, never rewrite it to match
# whichever sibling happens to be present. Authoring refreshes are explicit;
# complete source archives close both this graph and the workspace graph.
cargo "+${TOOLCHAIN}" metadata --locked --offline --manifest-path "$MANIFEST" --format-version 1 >/dev/null
cargo "+${TOOLCHAIN}" test --locked --manifest-path "$MANIFEST" --target-dir "$PREVIEW_TARGET_DIR"
cargo "+${TOOLCHAIN}" build --release --locked --manifest-path "$MANIFEST" --target-dir "$PREVIEW_TARGET_DIR"

install -d "$STAGE_DIR/licenses"
install -m 0755 "$PREVIEW_TARGET_DIR/release/templar-preview-codecs" "$STAGE_DIR/templar-preview-codecs"
install -m 0644 "$REPO_ROOT/../Lege-ecosystem/LICENSE" "$STAGE_DIR/licenses/LEGE-PDF-LICENSE"
"$STAGE_DIR/templar-preview-codecs" --self-check
