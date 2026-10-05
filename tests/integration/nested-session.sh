#!/usr/bin/env bash
# Live desktop journey: private D-Bus, isolated state, nested compositor, real shell surface, and
# CLI round trips through the three user-session daemons. Run under an existing X11 display; CI
# supplies one with xvfb-run.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
bin="${TEMPLAROS_NESTED_BIN_DIR:-$("scripts/native-target-dir.sh")/debug}"

if [ "${TEMPLAR_NESTED_ISOLATED:-0}" != 1 ]; then
    if [ -z "${DISPLAY:-}" ]; then
        echo "nested session: FAILED — an X11 display is required (use xvfb-run -a)" >&2
        exit 1
    fi
    if [ "${TEMPLAR_NESTED_SKIP_BUILD:-0}" != 1 ]; then
        scripts/cargo-native.sh build --workspace
    fi
    nested_root="$(mktemp -d /tmp/templaros-nested.XXXXXX)"
    nested_cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    nested_rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
    status=0
    # A failed run's logs are the only account of why it failed, and deleting them is how this
    # harness spent a long time reporting "socket did not appear" with nothing to read. The root
    # is removed on success and kept, with its path named, on failure.
    keep=0
    cleanup_root() {
        if [ "$keep" = 1 ]; then
            echo "nested session: state kept for inspection: $nested_root" >&2
            echo "               logs: $nested_root/logs/" >&2
            return
        fi
        case "$nested_root" in
            /tmp/templaros-nested.*) rm -rf -- "$nested_root" ;;
            *) echo "refusing to remove unexpected nested path: $nested_root" >&2 ;;
        esac
    }
    trap cleanup_root EXIT
    mkdir -p "$nested_root"/{home,config,data,state,cache,runtime,logs}
    chmod 700 "$nested_root/runtime"
    dbus-run-session -- env \
        TEMPLAR_NESTED_ISOLATED=1 \
        TEMPLAR_NESTED_SKIP_BUILD=1 \
        TEMPLAR_NESTED_INPUT_JOURNEY="${TEMPLAR_NESTED_INPUT_JOURNEY:-0}" \
        TEMPLAROS_NESTED_BIN_DIR="$bin" \
        TEMPLAR_NESTED_ROOT="$nested_root" \
        CARGO_HOME="$nested_cargo_home" \
        RUSTUP_HOME="$nested_rustup_home" \
        HOME="$nested_root/home" \
        XDG_CONFIG_HOME="$nested_root/config" \
        XDG_DATA_HOME="$nested_root/data" \
        XDG_DATA_DIRS="$nested_root/data" \
        XDG_STATE_HOME="$nested_root/state" \
        XDG_CACHE_HOME="$nested_root/cache" \
        XDG_RUNTIME_DIR="$nested_root/runtime" \
        bash "$root/tests/integration/nested-session.sh" || status=$?
    # `|| status=$?` and not a bare call: `set -e` would take the failure out of this shell before
    # the next line ran, which is how the root came to be deleted on exactly the runs that needed it.
    [ "${status:-0}" -eq 0 ] || keep=1
    exit "${status:-0}"
fi
: "${TEMPLAR_NESTED_ROOT:?isolated wrapper did not provide TEMPLAR_NESTED_ROOT}"

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
    "$@" >"$TEMPLAR_NESTED_ROOT/logs/$name.log" 2>&1 &
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
            --method org.freedesktop.DBus.NameHasOwner "$name" 2>/dev/null | grep true >/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    echo "nested session: FAILED — bus name did not appear: $name" >&2
    return 1
}

# Force the host side to X11. WAYLAND_DISPLAY belongs to the caller's runtime directory, which is
# deliberately hidden from this isolated session.
start_service compositor env -u WAYLAND_DISPLAY "$bin/templar-comp" \
    --backend winit --socket-name templaros-integration \
    --test-input-socket "$TEMPLAR_NESTED_ROOT/runtime/input.sock"
wait_for_path "$XDG_RUNTIME_DIR/templaros-integration"
# The trusted-shell socket is a *prefix* of the main socket's name, not a suffix — a trailing
# `-shell` on `wayland-0` would land inside snapd's `wayland-[0-9]*` permission, which is the one
# socket no confined application may open (compositor/templar-comp/src/shell_socket.rs).
wait_for_path "$XDG_RUNTIME_DIR/templaros-shell-templaros-integration"

start_service sessiond env WAYLAND_DISPLAY=templaros-integration "$bin/templar-sessiond" \
    --manifest-dir "$root/tests/fixtures/manifests" \
    --application-dir "$TEMPLAR_NESTED_ROOT/data/applications" \
    --no-default-discovery \
    --state-db "$TEMPLAR_NESTED_ROOT/state/session.db"
start_service filed "$bin/templar-filed" --reserve-percent 0 \
    --state-db "$TEMPLAR_NESTED_ROOT/state/session.db" \
    --index-root "$root/tests/fixtures"
start_service resourced "$bin/templar-resourced" --interval-ms 250 \
    --state-dir "$TEMPLAR_NESTED_ROOT/state/resources"
# The portal backend belongs in the journey for the same reason the daemons do: a sandboxed
# application reaches this system through it. Safe here where it would not be on a shared bus —
# this session's bus is private (dbus-run-session, above), so claiming the freedesktop name
# cannot collide with a portal the person is using.
start_service portal env WAYLAND_DISPLAY=templaros-integration "$bin/xdg-desktop-portal-templar"

wait_for_name org.templaros.Session
wait_for_name org.templaros.Files
wait_for_name org.templaros.Resources
wait_for_name org.freedesktop.impl.portal.desktop.templaros

# Exercise typed state over D-Bus and verify the read-after-write/history path.
"$bin/templar" app list --json | grep 'org.templaros.Viewer' >/dev/null
"$bin/templar" files search image-root --json | grep 'image-root' >/dev/null
"$bin/templar" settings set shell.wallpaper.enabled '{"type":"bool","value":true}' >/dev/null
"$bin/templar" settings get shell.wallpaper.enabled --json | grep 'true' >/dev/null
"$bin/templar" history search wallpaper --json | grep 'shell.wallpaper.enabled' >/dev/null
"$bin/templar" providers report --json | grep 'notification-server' >/dev/null
"$bin/templar" res --json | grep 'observe-only' >/dev/null

# The graphical shell connects through the compositor's trusted socket and must survive long
# enough to fetch its D-Bus-backed model and present frames. Its log is retained on failure.
start_service shell env WAYLAND_DISPLAY=templaros-shell-templaros-integration "$bin/templar-shell" --surface
shell_pid="${pids[$((${#pids[@]} - 1))]}"
sleep 3
if ! kill -0 "$shell_pid" 2>/dev/null; then
    echo "nested session: FAILED — graphical shell exited during startup" >&2
    cat "$TEMPLAR_NESTED_ROOT/logs/shell.log" >&2
    exit 1
fi
if ! kill -0 "${pids[0]}" 2>/dev/null; then
    echo "nested session: FAILED — compositor exited during the journey" >&2
    cat "$TEMPLAR_NESTED_ROOT/logs/compositor.log" >&2
    exit 1
fi

if [ "${TEMPLAR_NESTED_INPUT_JOURNEY:-0}" = 1 ]; then
    # Direct first-party Wayland clients, not transient user units or host tools.
    start_service client-settings env -u DISPLAY WAYLAND_DISPLAY=templaros-integration "$bin/templar-settings"
    start_service client-files env -u DISPLAY WAYLAND_DISPLAY=templaros-integration "$bin/templar-files" "$root/tests/fixtures"
    "$root/tests/integration/nested-input.py" --socket "$TEMPLAR_NESTED_ROOT/runtime/input.sock"
fi

echo "nested session: ok (live compositor, shell, services, CLI state journey)"
