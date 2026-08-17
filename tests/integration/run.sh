#!/usr/bin/env bash
# Smoke: daemons start in dry-run mode, the CLI's doctor runs against a fixture root.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
cargo run -q -p lege-sessiond -- --dry-run --manifest-dir tests/fixtures/manifests --state-db "$(mktemp -d)/s.db"
cargo run -q -p lege-resourced -- --once
cargo run -q -p lege-filed -- --dry-run >/dev/null
cargo run -q -p lege-session -- --dry-run
cargo run -q -p lege-shell -- --list-drawers >/dev/null
cargo run -q -p lege -- doctor --root tests/fixtures/image-root --json >/dev/null || [ $? -ne 0 ]
echo "integration smoke: ok"
