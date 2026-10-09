#!/usr/bin/env bash
# A whole TemplarOS session, nested inside whatever desktop is already running.
#
# Why this exists: every failure so far has been invisible. The units start, the journal says
# nothing, and the only account of what went wrong is a screen that does not change. Nested, every
# process's stdout and stderr goes to a file in this directory, and nothing depends on logging out
# of a working desktop to find out.
#
# It is deliberately not the installed session: it uses its own Wayland socket names, its own state
# database, and the binaries in target/ rather than the ones in /usr, so a rebuild is picked up by
# restarting one process and the installed session is never touched.
#
#   scripts/nested-session.sh start [--no-daemons]
#                                         compositor, sessiond, filed, portal and shell, in that
#                                         order; --no-daemons starts only the compositor and the
#                                         shell, for lanes that bring their own session daemon
#   scripts/nested-session.sh shell     rebuild and restart just the shell (the hot-swap loop)
#   scripts/nested-session.sh launch <app-id>
#   scripts/nested-session.sh apps      what the registry offers
#   scripts/nested-session.sh logs      the tail of every log
#   scripts/nested-session.sh stop
set -uo pipefail

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
root=$(CDPATH= cd -- "$here/.." && pwd -P)
run=${TEMPLAROS_NESTED_RUN_DIR:-"$root/.agent/scratch/nested-session"}
# Automated lanes can use a private directory and marker so cleanup affects only their run.
marker_prefix=${TEMPLAROS_NESTED_MARKER_PREFIX:-templar-nested}
if [[ ! $marker_prefix =~ ^[a-zA-Z0-9_-]+$ ]]; then
    echo "TEMPLAROS_NESTED_MARKER_PREFIX must contain only letters, digits, underscores and hyphens" >&2
    exit 1
fi
socket=templaros-nested
# The trusted-shell socket is named the way the compositor names it: a prefix, never a suffix.
# A `wayland-0-shell` would fall inside snapd's `wayland-[0-9]*` permission, and that socket is
# the one no confined application may ever open (compositor/templar-comp/src/shell_socket.rs).
shell_socket="templaros-shell-$socket"
profile=${TEMPLAROS_NESTED_PROFILE:-debug}
cargo_profile=$profile
[ "$profile" != debug ] || cargo_profile=dev
bin=${TEMPLAROS_NESTED_BIN_DIR:-$("$here/native-target-dir.sh")/$profile}
input_socket="$run/input.sock"

mkdir -p "$run"
chmod 700 "$run"

pidfile() { echo "$run/$1.pid"; }
logfile() { echo "$run/$1.log"; }

alive() {
    local pid
    pid=$(cat "$(pidfile "$1")" 2>/dev/null) || return 1
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

stop_one() {
    local name=$1 pid cmdline
    pid=$(cat "$(pidfile "$name")" 2>/dev/null) || return 0
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        # A stale pidfile after PID reuse must fail closed, not kill a stranger: every daemon
        # this script starts carries `$marker_prefix:$name` as its argv[0] (see below).
        cmdline=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null) || cmdline=""
        if [[ $cmdline != "$marker_prefix:$name "* ]]; then
            echo "  $name pidfile points at pid $pid (${cmdline%% *}) — not this run's $name; leaving it alone" >&2
        else
            kill "$pid" 2>/dev/null
            for _ in $(seq 20); do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.1
            done
            kill -9 "$pid" 2>/dev/null
        fi
    fi
    rm -f "$(pidfile "$name")"
}

# Survivors whose pidfiles went stale (a scratch prune, an overwritten pidfile) keep running
# past `stop` and hold the bus names and sockets the next `start` wants. The argv[0] marker
# only this script sets makes them findable without endangering anything else.
sweep_nested_leftovers() {
    local pid cmdline name
    for pid in $(pgrep -f "^$marker_prefix:" 2>/dev/null); do
        cmdline=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null) || continue
        name=${cmdline#"$marker_prefix:"}
        name=${name%% *}
        echo "  sweep: $name (pid $pid) survived its pidfile — stopping it" >&2
        kill "$pid" 2>/dev/null
        for _ in $(seq 20); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$pid" 2>/dev/null
    done
    return 0
}

# Run one nested daemon: without the host displays, with the given Wayland socket, and
# with an argv[0] marker only this script sets, so `stop_one` and the survivor sweep can
# tell this run's processes from strangers sharing a recycled PID.
# `WAYLAND_DISPLAY` is set per-process by the caller; `DISPLAY` is cleared so winit cannot
# quietly fall back to the host's X server and connect a "nested" client to the wrong
# compositor. Called as `spawn_nested ... &`, the `&` fork runs this function and the
# function `exec`s the daemon directly — no `( )` subshell, which would fork once more and
# leave `$!` pointing at an exited middleman instead of the daemon. The unset/export only
# touch the fork, never the caller's environment.
spawn_nested() {
    local name=$1 wayland=$2
    shift 2
    if [[ $BASHPID == $$ ]]; then
        echo "spawn_nested $name must run backgrounded (spawn_nested ... &)" >&2
        return 1
    fi
    unset DISPLAY
    WAYLAND_DISPLAY="$wayland" RUST_LOG="${RUST_LOG:-info}" exec -a "$marker_prefix:$name" "$@"
}

# Wait for the compositor to *say* a socket is ready, not for the socket file to exist. A killed
# compositor leaves its socket file behind, so the file is not evidence that anything is listening:
# believing it is what raced the shell into connecting to a dead socket.
wait_for_log() {
    local pattern=$1
    for _ in $(seq 150); do
        grep -q "$pattern" "$(logfile comp)" 2>/dev/null && return 0
        alive comp || return 1
        sleep 0.1
    done
    return 1
}

# Sockets from a previous run, which nothing else removes.
clear_stale_sockets() {
    rm -f "${XDG_RUNTIME_DIR}/${socket}" "${XDG_RUNTIME_DIR}/${socket}.lock" \
        "${XDG_RUNTIME_DIR}/${shell_socket}" "${XDG_RUNTIME_DIR}/${shell_socket}.lock"
}

start_comp() {
    if alive comp; then
        echo "compositor already running"
        return 0
    fi
    if [ -z "${TEMPLAROS_NESTED_NO_DAEMONS:-}" ]; then
        refuse_over_live_session || return 1
    fi
    [ -x "$bin/templar-comp" ] || { echo "missing $bin/templar-comp — build it first" >&2; return 1; }
    clear_stale_sockets
    : >"$(logfile comp)"
    echo "starting compositor (winit, socket $socket)"
    # Keep only the chosen host backend. Clients below still inherit neither host display.
    local host_wayland=${TEMPLAROS_HOST_WAYLAND:-${WAYLAND_DISPLAY:-}}
    local host_x11=${TEMPLAROS_HOST_DISPLAY:-${DISPLAY:-}}
    local -a host_env
    if [ -n "$host_wayland" ]; then
        host_env=(env -u DISPLAY WAYLAND_DISPLAY="$host_wayland")
    elif [ -n "$host_x11" ]; then
        host_env=(env -u WAYLAND_DISPLAY DISPLAY="$host_x11")
    else
        echo "no host Wayland or X11 display; use xvfb-run -a for an isolated test" >&2
        return 1
    fi
    rm -f "$input_socket"
    # Same argv[0] marker `spawn_nested` sets (the compositor keeps the host display, so it
    # cannot use that helper, but `stop` must still find it by the same marker).
    "${host_env[@]}" RUST_LOG="${RUST_LOG:-info}" \
        bash -c 'marker=$1; shift; exec -a "$marker:comp" "$@"' _ "$marker_prefix" \
        "$bin/templar-comp" --backend winit --socket-name "$socket" \
        --test-input-socket "$input_socket" \
        >"$(logfile comp)" 2>&1 &
    echo $! >"$(pidfile comp)"
    wait_for_log "trusted-shell socket ready" \
        || { echo "trusted-shell socket never announced; see $(logfile comp)" >&2; return 1; }
    wait_for_log "compositor is ready" \
        || { echo "compositor never presented a frame; see $(logfile comp)" >&2; return 1; }
    echo "  sockets ready: $socket, $shell_socket"
}

# The nested session shares the person's user bus. If the *installed* session daemon owns
# org.templaros.Session, a nested one would fight it for the name and every launch it makes would
# land in the wrong session — refuse, rather than disturb a desktop somebody is working in.
refuse_over_live_session() {
    if systemctl --user is-active --quiet templar-sessiond.service 2>/dev/null \
        || systemctl --user is-active --quiet templar-comp.service 2>/dev/null; then
        echo "the installed TemplarOS session is running (templar-sessiond/templar-comp user units are active);" >&2
        echo "a nested session would share its bus and disturb it. Not starting." >&2
        return 1
    fi
    if ! alive sessiond && busctl --user status org.templaros.Session >/dev/null 2>&1; then
        echo "org.templaros.Session already has an owner on the user bus that this script did not start;" >&2
        echo "not starting a second session daemon against it." >&2
        return 1
    fi
}

start_sessiond() {
    if alive sessiond; then
        echo "sessiond already running"
        return 0
    fi
    refuse_over_live_session || return 1
    [ -x "$bin/templar-sessiond" ] || { echo "missing $bin/templar-sessiond" >&2; return 1; }
    echo "starting sessiond (state $run/session.db)"
    # An app launched from here must reach the nested compositor, not the host's. sessiond passes
    # its own environment down to every transient unit it starts, so this is where that is decided.
    spawn_nested sessiond "$socket" \
        "$bin/templar-sessiond" --state-db "$run/session.db" \
        >"$(logfile sessiond)" 2>&1 &
    echo $! >"$(pidfile sessiond)"
    for _ in $(seq 50); do
        busctl --user status org.templaros.Session >/dev/null 2>&1 && break
        sleep 0.1
    done
    busctl --user status org.templaros.Session >/dev/null 2>&1 \
        || echo "  warning: org.templaros.Session did not appear; see $(logfile sessiond)" >&2
}

# The file service backs six of the eight drawers. Without it Home renders "service unavailable"
# in every one of them, which is honest but tells you nothing about how the drawers actually look.
start_filed() {
    if alive filed; then
        echo "filed already running"
        return 0
    fi
    if [ ! -x "$bin/templar-filed" ]; then
        echo "  no $bin/templar-filed — drawers will read \"service unavailable\"" >&2
        return 0
    fi
    echo "starting filed"
    spawn_nested filed "$socket" \
        "$bin/templar-filed" \
        >"$(logfile filed)" 2>&1 &
    echo $! >"$(pidfile filed)"
    sleep 0.5
}

# The portal backend, so a nested browser or Flatpak application can be driven through the real
# FileChooser and AppChooser rather than through a description of them. Unlike the daemons above
# it claims a *freedesktop* well-known name on the shared user bus, so it starts only when nothing
# else owns that name — the same courtesy `refuse_over_live_session` pays `org.templaros.Session`.
start_portal() {
    if alive portal; then
        echo "portal already running"
        return 0
    fi
    if [ ! -x "$bin/xdg-desktop-portal-templar" ]; then
        echo "  no $bin/xdg-desktop-portal-templar — portal journeys will not run" >&2
        return 0
    fi
    if busctl --user status org.freedesktop.impl.portal.desktop.templaros >/dev/null 2>&1; then
        echo "  org.freedesktop.impl.portal.desktop.templaros already has an owner that this script" >&2
        echo "  did not start; not starting a second portal backend against it." >&2
        return 0
    fi
    echo "starting portal"
    spawn_nested portal "$socket" \
        "$bin/xdg-desktop-portal-templar" \
        >"$(logfile portal)" 2>&1 &
    echo $! >"$(pidfile portal)"
    sleep 0.5
}

start_shell() {
    stop_one shell
    [ -x "$bin/templar-shell" ] || { echo "missing $bin/templar-shell" >&2; return 1; }
    echo "starting shell (trusted socket $shell_socket)"
    spawn_nested shell "$shell_socket" \
        "$bin/templar-shell" --surface \
        >"$(logfile shell)" 2>&1 &
    echo $! >"$(pidfile shell)"
    sleep 1
    alive shell || { echo "  the shell exited immediately; see $(logfile shell)" >&2; return 1; }
}

case "${1:-start}" in
start)
    if [ "${2:-}" = "--no-daemons" ]; then
        # Compositor and shell only, for lanes that bring their own session daemon: nothing
        # here takes a bus name, so the live-session refusal does not apply either.
        TEMPLAROS_NESTED_NO_DAEMONS=1 start_comp && start_shell \
            && echo "nested session up without daemons — logs in $run"
    else
        start_comp && start_sessiond && start_filed && start_portal && start_shell \
            && echo "nested session up — logs in $run"
    fi
    ;;
shell)
    # The hot-swap loop rebuilds the native shell; an explicit binary directory is restart-only.
    if [ -z "${TEMPLAROS_NESTED_BIN_DIR:-}" ]; then
        ( cd "$root" && "$here/cargo-native.sh" build -p templar-shell --profile "$cargo_profile" ) || exit 1
    fi
    start_shell && echo "shell restarted"
    ;;
launch)
    [ $# -ge 2 ] || { echo "usage: $0 launch <app-id>" >&2; exit 2; }
    "$bin/templar" app launch "$2"
    ;;
apps)
    "$bin/templar" app list 2>/dev/null | head -40
    ;;
input)
    shift
    "$here/nested-input.py" --socket "$input_socket" "$@"
    ;;
logs)
    for name in comp sessiond filed portal shell; do
        echo "===== $name"
        tail -n "${2:-25}" "$(logfile "$name")" 2>/dev/null || echo "(no log)"
    done
    ;;
stop)
    for name in shell portal filed sessiond comp; do stop_one "$name"; done
    sweep_nested_leftovers
    clear_stale_sockets
    rm -f "$input_socket"
    echo "nested session stopped"
    ;;
status)
    for name in comp sessiond filed portal shell; do
        printf '%-10s %s\n' "$name" "$(alive "$name" && echo running || echo stopped)"
    done
    ;;
*)
    sed -n '2,20p' "$0"
    exit 2
    ;;
esac
