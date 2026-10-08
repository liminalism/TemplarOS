#!/usr/bin/env bash
# Tests own their bus and writable XDG state. A successful empty filter is not a passing gate.
set -euo pipefail

if [[ ${1:-} != --private ]]; then
    scratch=$(mktemp -d "${TMPDIR:-/tmp}/templaros-tests.XXXXXXXX")
    trap 'rm -rf -- "$scratch"' EXIT
    mkdir -p "$scratch"/{home,config,config-dirs,data,state,cache,runtime}
    chmod 700 "$scratch/runtime"
    script=$(realpath -- "$0")
    env -u DISPLAY -u WAYLAND_DISPLAY -u DBUS_SYSTEM_BUS_ADDRESS \
        CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
        TEMPLAROS_TEST_PARENT_BUS="${DBUS_SESSION_BUS_ADDRESS:-}" \
        HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/config" XDG_DATA_HOME="$scratch/data" \
        XDG_STATE_HOME="$scratch/state" XDG_CACHE_HOME="$scratch/cache" XDG_RUNTIME_DIR="$scratch/runtime" \
        XDG_CONFIG_DIRS="$scratch/config-dirs" XDG_DATA_DIRS=/usr/share \
        dbus-run-session -- bash "$script" --private "$scratch" "$@"
    exit $?
fi
shift
scratch=$1
shift
if [[ -z ${DBUS_SESSION_BUS_ADDRESS:-} || $DBUS_SESSION_BUS_ADDRESS == "${TEMPLAROS_TEST_PARENT_BUS:-}" ]]; then
    echo 'Refusing tests without a newly owned private session bus.' >&2
    exit 1
fi
# Even code that opens the system bus reaches only the disposable fixture bus.
export DBUS_SYSTEM_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS
set +e
cargo test "$@" 2>&1 | tee "$scratch/results.log"
status=${PIPESTATUS[0]}
set -e
if (( status != 0 )); then exit "$status"; fi
executed=0
while IFS= read -r line; do
    if [[ $line =~ ^test\ result:\ ok\.\ ([0-9]+)\ passed\;\ ([0-9]+)\ failed\; ]]; then
        executed=$((executed + BASH_REMATCH[1] + BASH_REMATCH[2]))
    fi
done < "$scratch/results.log"
if (( executed == 0 )); then
    echo 'FAIL: cargo exited successfully but no tests executed (check package/filter).' >&2
    exit 1
fi
printf 'PASS: %d tests executed on a private bus and disposable HOME/XDG state.\n' "$executed"
