#!/usr/bin/env bash
# Explicit CARGO_TARGET_DIR always wins (Debian builds deliberately use target/).
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    printf '%s\n' "$CARGO_TARGET_DIR"
    exit 0
fi
# os-release is an OS identity, unlike the desktop name (Lubuntu uses Ubuntu's toolchain).
ID=unknown
VERSION_ID=unknown
. "${TEMPLAROS_OS_RELEASE:-/etc/os-release}"
key=$(printf '%s-%s' "$ID" "$VERSION_ID" | tr -c 'A-Za-z0-9._-' '_')
printf '%s/target/native/%s\n' "$root" "$key"
