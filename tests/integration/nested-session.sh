#!/usr/bin/env bash
# Live desktop journey: private D-Bus, isolated state, nested compositor, real shell surface, and
# CLI round trips through the three user-session daemons. Run under an existing X11 display; CI
# supplies one with xvfb-run.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
bin="${CARGO_TARGET_DIR:-$root/target}/debug"

if [ "${LEGE_NESTED_ISOLATED:-0}" != 1 ]; then
    if [ -z "${DISPLAY:-}" ]; then
        echo "nested session: FAILED — an X11 display is required (use xvfb-run -a)" >&2
        exit 1
    fi
    cargo build --workspace
    nested_root="$(mktemp -d /tmp/legeos-nested.XXXXXX)"
    nested_cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    nested_rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
    cleanup_root() {
        case "$nested_root" in
            /tmp/legeos-nested.*) rm -rf -- "$nested_root" ;;
            *) echo "refusing to remove unexpected nested path: $nested_root" >&2 ;;
        esac
    }
    trap cleanup_root EXIT
    mkdir -p "$nested_root"/{home,config,data,state,cache,runtime,logs}
    chmod 700 "$nested_root/runtime"
    dbus-run-session -- env \
        LEGE_NESTED_ISOLATED=1 \
        LEGE_NESTED_ROOT="$nested_root" \
        CARGO_HOME="$nested_cargo_home" \
        RUSTUP_HOME="$nested_rustup_home" \
        HOME="$nested_root/home" \
        XDG_CONFIG_HOME="$nested_root/config" \
        XDG_DATA_HOME="$nested_root/data" \
        XDG_DATA_DIRS="$nested_root/data" \
        XDG_STATE_HOME="$nested_root/state" \
        XDG_CACHE_HOME="$nested_root/cache" \
        XDG_RUNTIME_DIR="$nested_root/runtime" \
        bash "$root/tests/integration/nested-session.sh"
    exit $?
fi
: "${LEGE_NESTED_ROOT:?isolated wrapper did not provide LEGE_NESTED_ROOT}"

pids=()
cleanup_processes() {
    local pid
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
}
trap cleanup_processes EXIT

start_service() {
    local name="$1"
    shift
    "$@" >"$LEGE_NESTED_ROOT/logs/$name.log" 2>&1 &
    pids+=("$!")
}

wait_for_path() {
    local path="$1"
    for _ in $(seq 1 100); do
        [ -S "$path" ] && return 0
        sleep 0.1
    done
    echo "nested session: FAILED — socket did not appear: $path" >&2
    return 1
}

wait_for_name() {
    local name="$1"
    for _ in $(seq 1 100); do
        if gdbus call --session --dest org.freedesktop.DBus \
            --object-path /org/freedesktop/DBus \
            --method org.freedesktop.DBus.NameHasOwner "$name" 2>/dev/null | grep -q true; then
            return 0
        fi
        sleep 0.1
    done
    echo "nested session: FAILED — bus name did not appear: $name" >&2
    return 1
}

# Force the host side to X11. WAYLAND_DISPLAY belongs to the caller's runtime directory, which is
# deliberately hidden from this isolated session.
start_service compositor env -u WAYLAND_DISPLAY "$bin/lege-comp" \
    --backend winit --socket-name legeos-integration
wait_for_path "$XDG_RUNTIME_DIR/legeos-integration"
wait_for_path "$XDG_RUNTIME_DIR/legeos-integration-shell"

start_service sessiond env WAYLAND_DISPLAY=legeos-integration "$bin/lege-sessiond" \
    --manifest-dir "$root/tests/fixtures/manifests" \
    --application-dir "$LEGE_NESTED_ROOT/data/applications" \
    --no-default-discovery \
    --state-db "$LEGE_NESTED_ROOT/state/session.db"
start_service filed "$bin/lege-filed" --reserve-percent 0 \
    --state-db "$LEGE_NESTED_ROOT/state/session.db" \
    --index-root "$root/tests/fixtures"
start_service resourced "$bin/lege-resourced" --interval-ms 250 \
    --state-dir "$LEGE_NESTED_ROOT/state/resources"
# The portal backend belongs in the journey for the same reason the daemons do: a sandboxed
# application reaches this system through it. Safe here where it would not be on a shared bus —
# this session's bus is private (dbus-run-session, above), so claiming the freedesktop name
# cannot collide with a portal the person is using.
start_service portal env WAYLAND_DISPLAY=legeos-integration "$bin/xdg-desktop-portal-lege"

wait_for_name org.legeos.Session
wait_for_name org.legeos.Files
wait_for_name org.legeos.Resources
wait_for_name org.freedesktop.impl.portal.desktop.legeos

# Exercise typed state over D-Bus and verify the read-after-write/history path.
"$bin/lege" app list --json | grep -q 'org.legeos.Viewer'
"$bin/lege" files search image-root --json | grep -q 'image-root'
"$bin/lege" settings set shell.wallpaper.enabled '{"type":"bool","value":true}' >/dev/null
"$bin/lege" settings get shell.wallpaper.enabled --json | grep -q 'true'
"$bin/lege" history search wallpaper --json | grep -q 'shell.wallpaper.enabled'
"$bin/lege" providers report --json | grep -q 'notification-server'
"$bin/lege" res --json | grep -q 'observe-only'

# The graphical shell connects through the compositor's trusted socket and must survive long
# enough to fetch its D-Bus-backed model and present frames. Its log is retained on failure.
start_service shell env WAYLAND_DISPLAY=legeos-integration-shell "$bin/lege-shell" --surface
shell_pid="${pids[$((${#pids[@]} - 1))]}"
sleep 3
if ! kill -0 "$shell_pid" 2>/dev/null; then
    echo "nested session: FAILED — graphical shell exited during startup" >&2
    cat "$LEGE_NESTED_ROOT/logs/shell.log" >&2
    exit 1
fi
if ! kill -0 "${pids[0]}" 2>/dev/null; then
    echo "nested session: FAILED — compositor exited during the journey" >&2
    cat "$LEGE_NESTED_ROOT/logs/compositor.log" >&2
    exit 1
fi

echo "nested session: ok (live compositor, shell, services, CLI state journey)"
