#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== fmt";        cargo fmt --all -- --check
echo "== clippy";     cargo clippy --workspace --all-targets -- -D warnings
echo "== test";       cargo test --workspace
echo "== msrv";       if rustup toolchain list | grep -q '^1.93'; then cargo +1.93 check --workspace; else echo "  (1.93 toolchain not installed; run 'rustup toolchain install 1.93' — required in CI)"; fi
echo "== deny";       if command -v cargo-deny >/dev/null; then cargo deny check; else echo "  (cargo-deny not installed)"; fi
echo "== xtask";      cargo run -q -p xtask -- tree-check && cargo run -q -p xtask -- deps-check && cargo run -q -p xtask -- check-interfaces && cargo run -q -p xtask -- check-proxies && cargo run -q -p xtask -- check-packaging && cargo run -q -p xtask -- check-assets
echo "== smoke";      tests/integration/run.sh
echo "== deb";        if command -v cargo-deb >/dev/null; then cargo deb -p lege-sessiond --no-build -q >/dev/null 2>&1 || cargo deb -p lege-sessiond -q; echo "  built target/debian/legeos-sessiond_*.deb"; else echo "  (cargo-deb not installed)"; fi
echo "ci: ok"
