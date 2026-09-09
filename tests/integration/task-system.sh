#!/usr/bin/env bash
# The Task System's three doc acceptances, in a form that can actually be run
# (`docs/task-system-taskfield-island-peek.md` §13, Phase 2):
#
#   from-fullscreen-every-time  enter and leave Taskfield over a fullscreen client many times;
#                               the scene comes back byte-for-byte every time.
#   no-configure-sent           no `xdg_toplevel.configure` reaches a client during the mode.
#   shell-dead                  kill the shell inside the mode; activation still works.
#
# It runs a whole nested session — its own compositor, its own bus, its own state — and never
# touches the installed one. The one thing it cannot isolate is the X server's keyboard: driving
# the chords means XTEST, which goes to whatever window has focus. So the two checks that need
# keys refuse to run unless TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1 says the desktop may be borrowed for
# a minute, and even then every keystroke is preceded by a check that the nested compositor's own
# window is the one that will receive it. A key sent to the wrong window is not a failed test, it
# is typing into the user's editor.
#
#   TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1 tests/integration/task-system.sh
#   TEMPLAR_TASKFIELD_ROUNDS=100 TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1 tests/integration/task-system.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
bin="${CARGO_TARGET_DIR:-$root/target}/debug"
rounds="${TEMPLAR_TASKFIELD_ROUNDS:-20}"
socket=templaros-taskfield
shell_socket="templaros-shell-$socket"

if [ "${TEMPLAR_TASKFIELD_ISOLATED:-0}" != 1 ]; then
    if [ -z "${DISPLAY:-}" ]; then
        echo "task system: SKIPPED — an X11 display is required (winit hosts the nested compositor)"
        exit 0
    fi
    for tool in xdotool dbus-run-session; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            echo "task system: SKIPPED — $tool is not installed"
            exit 0
        fi
    done
    cargo build --workspace
    tf_root="$(mktemp -d /tmp/templaros-taskfield.XXXXXX)"
    status=0
    keep=0
    cleanup_root() {
        if [ "$keep" = 1 ]; then
            echo "task system: state kept for inspection: $tf_root" >&2
            echo "             logs: $tf_root/logs/" >&2
            return
        fi
        case "$tf_root" in
            /tmp/templaros-taskfield.*) rm -rf -- "$tf_root" ;;
            *) echo "refusing to remove unexpected path: $tf_root" >&2 ;;
        esac
    }
    trap cleanup_root EXIT
    mkdir -p "$tf_root"/{home,config,data,state,cache,runtime,logs}
    chmod 700 "$tf_root/runtime"
    dbus-run-session -- env \
        TEMPLAR_TASKFIELD_ISOLATED=1 \
        TEMPLAR_TASKFIELD_ROOT="$tf_root" \
        TEMPLAR_TASKFIELD_ROUNDS="$rounds" \
        TEMPLAR_TASKFIELD_ACCEPT_FOCUS="${TEMPLAR_TASKFIELD_ACCEPT_FOCUS:-0}" \
        CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
        RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
        HOME="$tf_root/home" \
        XDG_CONFIG_HOME="$tf_root/config" \
        XDG_DATA_HOME="$tf_root/data" \
        XDG_DATA_DIRS="$tf_root/data" \
        XDG_STATE_HOME="$tf_root/state" \
        XDG_CACHE_HOME="$tf_root/cache" \
        XDG_RUNTIME_DIR="$tf_root/runtime" \
        bash "$root/tests/integration/task-system.sh" || status=$?
    [ "${status:-0}" -eq 0 ] || keep=1
    exit "${status:-0}"
fi
: "${TEMPLAR_TASKFIELD_ROOT:?isolated wrapper did not provide TEMPLAR_TASKFIELD_ROOT}"
logs="$TEMPLAR_TASKFIELD_ROOT/logs"

pids=()
cleanup_processes() {
    local pid
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap cleanup_processes EXIT

start_service() {
    local name="$1"; shift
    "$@" >"$logs/$name.log" 2>&1 &
    pids+=("$!")
}

wait_for_path() {
    local path="$1"
    for _ in $(seq 1 100); do [ -S "$path" ] && return 0; sleep 0.1; done
    echo "task system: FAILED — socket did not appear: $path" >&2
    return 1
}

fail() { echo "task system: FAILED — $*" >&2; exit 1; }

# --- the nested session ------------------------------------------------------------------------

start_service compositor env -u WAYLAND_DISPLAY "$bin/templar-comp" \
    --backend winit --socket-name "$socket"
comp_pid="${pids[0]}"
wait_for_path "$XDG_RUNTIME_DIR/$socket"
wait_for_path "$XDG_RUNTIME_DIR/$shell_socket"

start_service sessiond env WAYLAND_DISPLAY="$socket" "$bin/templar-sessiond" \
    --manifest-dir "$root/tests/fixtures/manifests" \
    --application-dir "$TEMPLAR_TASKFIELD_ROOT/data/applications" \
    --no-default-discovery \
    --state-db "$TEMPLAR_TASKFIELD_ROOT/state/session.db"
start_service shell env WAYLAND_DISPLAY="$shell_socket" "$bin/templar-shell" --surface
shell_pid="${pids[$((${#pids[@]} - 1))]}"
sleep 3
kill -0 "$comp_pid" 2>/dev/null || fail "the compositor exited during startup"

# The nested compositor's own X window: every synthetic key must land here and nowhere else.
winit_window="$(xdotool search --name '^Smithay$' | head -n1 || true)"
[ -n "$winit_window" ] || fail "could not find the nested compositor's window"

send_key() {
    [ "${TEMPLAR_TASKFIELD_ACCEPT_FOCUS:-0}" = 1 ] || return 0
    xdotool windowactivate --sync "$winit_window" 2>/dev/null || true
    local active
    active="$(xdotool getactivewindow 2>/dev/null || echo 0)"
    # Refuse rather than type into whatever the person is doing. This is the whole reason the
    # keyboard half of this script is opt-in.
    [ "$active" = "$winit_window" ] || fail "the nested window does not have focus; refusing to send $1"
    xdotool key --clearmodifiers "$1"
    sleep 0.15
}

keys_available() { [ "${TEMPLAR_TASKFIELD_ACCEPT_FOCUS:-0}" = 1 ]; }

# --- a client that fills the screen ---------------------------------------------------------

client=""
for candidate in weston-terminal foot kitty alacritty; do
    command -v "$candidate" >/dev/null 2>&1 && client="$candidate" && break
done
[ -n "$client" ] || fail "no Wayland terminal to fill the screen with (weston-terminal, foot, kitty or alacritty)"
case "$client" in
    kitty)     client_cmd=(kitty --start-as fullscreen --hold sh -c 'sleep 600') ;;
    foot)      client_cmd=(foot --fullscreen sh -c 'sleep 600') ;;
    alacritty) client_cmd=(alacritty -o window.startup_mode=Fullscreen -e sh -c 'sleep 600') ;;
    *)         client_cmd=(weston-terminal --fullscreen) ;;
esac
start_service client env WAYLAND_DISPLAY="$socket" WAYLAND_DEBUG=1 "${client_cmd[@]}"
sleep 3
grep -q 'xdg_toplevel' "$logs/client.log" || fail "the fullscreen client never spoke xdg-shell"

# --- from-fullscreen-every-time --------------------------------------------------------------

screenshot() {
    env WAYLAND_DISPLAY="$shell_socket" "$bin/templar-shell" --screenshot "$1" >/dev/null 2>&1
}

if keys_available; then
    before="$TEMPLAR_TASKFIELD_ROOT/before.png"
    after="$TEMPLAR_TASKFIELD_ROOT/after.png"
    screenshot "$before" || fail "could not capture the scene before the mode"
    for i in $(seq 1 "$TEMPLAR_TASKFIELD_ROUNDS"); do
        send_key super+Tab
        send_key Escape
    done
    sleep 0.5
    screenshot "$after" || fail "could not capture the scene after the mode"
    if [ "$(sha256sum <"$before" | cut -d' ' -f1)" != "$(sha256sum <"$after" | cut -d' ' -f1)" ]; then
        fail "from-fullscreen-every-time: the scene did not come back byte-for-byte after $TEMPLAR_TASKFIELD_ROUNDS rounds"
    fi
    echo "task system: from-fullscreen-every-time ok ($TEMPLAR_TASKFIELD_ROUNDS rounds)"
else
    echo "task system: from-fullscreen-every-time SKIPPED (set TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1)"
fi

# --- no-configure-sent -------------------------------------------------------------------------

# No markers to correlate: the client has been idle for seconds, so any `configure` that appears
# from here on was caused by the mode and by nothing else. Entering and leaving without choosing a
# card activates nothing, so a correct implementation sends the client nothing at all.
if keys_available; then
    baseline="$(grep -c 'xdg_toplevel.*configure' "$logs/client.log" || true)"
    for i in $(seq 1 10); do
        send_key super+Tab
        send_key Escape
    done
    sleep 1
    now="$(grep -c 'xdg_toplevel.*configure' "$logs/client.log" || true)"
    [ "$baseline" = "$now" ] || fail "no-configure-sent: $((now - baseline)) configure event(s) reached the client during the mode"
    echo "task system: no-configure-sent ok (no configure in 10 rounds)"
else
    echo "task system: no-configure-sent SKIPPED (set TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1)"
fi

# --- shell-dead ---------------------------------------------------------------------------------

# The mode is the compositor's, so it must outlive the shell that decorates it: enter, kill the
# shell, and activate a card from the keyboard. Nothing here needs the pointer, which is what makes
# it runnable at all.
if keys_available; then
    send_key super+Tab
    kill "$shell_pid" 2>/dev/null || true
    sleep 1
    kill -0 "$comp_pid" 2>/dev/null || fail "shell-dead: the compositor died with the shell"
    send_key Return
    sleep 0.5
    kill -0 "$comp_pid" 2>/dev/null || fail "shell-dead: the compositor died activating a card"
    grep -q 'leaving Taskfield' "$logs/compositor.log" \
        || fail "shell-dead: activation did not leave the mode (no 'leaving Taskfield' in the log)"
    echo "task system: shell-dead ok (mode survived the shell and activated a card)"
else
    # What survives without the keyboard is still worth asserting: the compositor must not depend
    # on the shell being alive for anything at all.
    kill "$shell_pid" 2>/dev/null || true
    sleep 1
    kill -0 "$comp_pid" 2>/dev/null || fail "shell-dead: the compositor died with the shell"
    echo "task system: shell-dead PARTIAL (compositor survived the shell; the card activation half needs TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1)"
fi

echo "task system: ok"
