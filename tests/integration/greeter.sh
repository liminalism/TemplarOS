#!/usr/bin/env bash
# Headless greetd contract: exercise the real framed JSON client against its in-process fake peer,
# then verify the installed command chain preserves greetd's socket into the compositor child.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

cargo test -p templar-greeter
cargo build -p templar-comp
bin="${CARGO_TARGET_DIR:-$root/target}/debug"

config="session/units/greetd/config.toml"
grep -q '^command = "templar-comp --backend udev --exec /usr/bin/templar-greeter"$' "$config"
grep -q '^user = "templar-greeter"$' "$config"

fake_socket="$root/tests/fixtures/fake-greetd.sock"
inherited="$(GREETD_SOCK="$fake_socket" "$bin/templar-comp" --check --exec /usr/bin/env)"
grep -q "^GREETD_SOCK=$fake_socket$" <<<"$inherited"
grep -q '^WAYLAND_DISPLAY=wayland-' <<<"$inherited"

echo "greeter integration: ok (mock greetd flows + compositor environment contract)"
