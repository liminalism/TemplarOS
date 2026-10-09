#!/usr/bin/env bash
# The Debian quilt overlay expects the exact clean upstream pin.
set -euo pipefail
src=${1:?source directory required}
expected=05ed650ab101ee3d61795fa5eeeb5c32e1e366c3
actual=$(git -C "$src" rev-parse HEAD)
if [[ "$actual" != "$expected" ]]; then
    echo "FSearch source must be the manifest's upstream pin $expected" >&2
    exit 1
fi
if [[ -n $(git -C "$src" status --porcelain) ]]; then
    echo "FSearch packaging expects clean upstream source; Debian applies the complete downstream patch." >&2
    exit 1
fi
test -f "$src/src/fsearch_database.c"
test -f "$src/LICENSE"
