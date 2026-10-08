#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="$(scripts/native-target-dir.sh)"
echo "== fmt";        cargo fmt --all -- --check
echo "== clippy";     cargo clippy --workspace --all-targets -- -D warnings
echo "== test";       scripts/test-nonzero.sh --workspace
echo "== msrv";       if rustup toolchain list | grep -q '^1.97'; then cargo +1.97 check --workspace; else echo "  (1.97 toolchain not installed; run 'rustup toolchain install 1.97' — required in CI)"; fi
echo "== deny";       if command -v cargo-deny >/dev/null; then cargo deny check; else echo "  (cargo-deny not installed)"; fi
echo "== xtask";      cargo run -q -p xtask -- tree-check && cargo run -q -p xtask -- deps-check && cargo run -q -p xtask -- check-interfaces && cargo run -q -p xtask -- check-interface-signatures && cargo run -q -p xtask -- check-proxies && cargo run -q -p xtask -- check-packaging && cargo run -q -p xtask -- check-assets && cargo run -q -p xtask -- check-ownership && cargo run -q -p xtask -- check-actions
echo "== Wayland versions"; cargo run -q -p xtask -- check-wayland-versions
echo "== native target"; tests/integration/native-target-dir.sh
echo "== smoke";      tests/integration/run.sh
echo "== nested";     if command -v xvfb-run >/dev/null; then xvfb-run -a env TEMPLAR_NESTED_INPUT_JOURNEY=1 tests/integration/nested-session.sh; else echo "  (xvfb-run not installed)"; fi
echo "== greeter";    tests/integration/greeter.sh
echo "== packages";   if command -v cargo-deb >/dev/null; then tests/integration/package-install.sh; else echo "  (cargo-deb not installed)"; fi
echo "== os-update";  tests/integration/update-templaros.sh
echo "== source";      ci/source-build.sh   # prints NOT EXECUTED where mmdebstrap/podman are absent
echo "ci: ok"
