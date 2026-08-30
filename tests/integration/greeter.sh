#!/usr/bin/env bash
# Headless greetd contract: exercise the real framed JSON client against its in-process fake peer,
# then verify the installed command chain preserves greetd's socket into the compositor child.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

cargo test -p lege-greeter
cargo build -p lege-comp
bin="${CARGO_TARGET_DIR:-$root/target}/debug"

config="session/units/greetd/config.toml"
grep -q '^command = "lege-comp --backend udev --exec /usr/bin/lege-greeter"$' "$config"
grep -q '^user = "lege-greeter"$' "$config"

fake_socket="$root/tests/fixtures/fake-greetd.sock"
inherited="$(GREETD_SOCK="$fake_socket" "$bin/lege-comp" --check --exec /usr/bin/env)"
grep -q "^GREETD_SOCK=$fake_socket$" <<<"$inherited"
grep -q '^WAYLAND_DISPLAY=wayland-' <<<"$inherited"

echo "greeter integration: ok (mock greetd flows + compositor environment contract)"
