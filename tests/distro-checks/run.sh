#!/usr/bin/env bash
set -euo pipefail
image_root="${1:?usage: run.sh <unpacked-image-root>}"
exec cargo run -q -p templar -- doctor --root "$image_root" --json
