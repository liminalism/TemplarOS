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
#   scripts/nested-session.sh start     compositor, sessiond and shell, in that order
#   scripts/nested-session.sh shell     rebuild and restart just the shell (the hot-swap loop)
#   scripts/nested-session.sh launch <app-id>
#   scripts/nested-session.sh apps      what the registry offers
#   scripts/nested-session.sh logs      the tail of every log
#   scripts/nested-session.sh stop
set -uo pipefail

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
root=$(CDPATH= cd -- "$here/.." && pwd -P)
run="$root/.agent/scratch/nested-session"
socket=templaros-nested
# The trusted-shell socket is named the way the compositor names it: a prefix, never a suffix.
# A `wayland-0-shell` would fall inside snapd's `wayland-[0-9]*` permission, and that socket is
# the one no confined application may ever open (compositor/templar-comp/src/shell_socket.rs).
shell_socket="templaros-shell-$socket"
profile=${TEMPLAROS_NESTED_PROFILE:-debug}
bin="$root/target/$profile"

mkdir -p "$run"

pidfile() { echo "$run/$1.pid"; }
logfile() { echo "$run/$1.log"; }

alive() {
    local pid
    pid=$(cat "$(pidfile "$1")" 2>/dev/null) || return 1
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

stop_one() {
    local name=$1 pid
    pid=$(cat "$(pidfile "$name")" 2>/dev/null) || return 0
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null
        for _ in $(seq 20); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$pid" 2>/dev/null
    fi
    rm -f "$(pidfile "$name")"
}

# Everything a nested process must NOT inherit from the host desktop, plus the one thing it must.
# `WAYLAND_DISPLAY` is set per-process by the caller; `DISPLAY` is cleared so winit cannot quietly
# fall back to the host's X server and connect a "nested" client to the wrong compositor.
nested_env() {
    env -u DISPLAY -u WAYLAND_DISPLAY "$@"
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
    refuse_over_live_session || return 1
    [ -x "$bin/templar-comp" ] || { echo "missing $bin/templar-comp — build it first" >&2; return 1; }
    clear_stale_sockets
    : >"$(logfile comp)"
    echo "starting compositor (winit, socket $socket)"
    # The host's WAYLAND_DISPLAY is what winit nests *into*, so this one keeps it.
    env -u DISPLAY WAYLAND_DISPLAY="${TEMPLAROS_HOST_WAYLAND:-$WAYLAND_DISPLAY}" \
        RUST_LOG="${RUST_LOG:-info}" \
        "$bin/templar-comp" --backend winit --socket-name "$socket" \
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
    nested_env WAYLAND_DISPLAY="$socket" RUST_LOG="${RUST_LOG:-info}" \
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
    nested_env WAYLAND_DISPLAY="$socket" RUST_LOG="${RUST_LOG:-info}" \
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
    nested_env WAYLAND_DISPLAY="$socket" RUST_LOG="${RUST_LOG:-info}" \
        "$bin/xdg-desktop-portal-templar" \
        >"$(logfile portal)" 2>&1 &
    echo $! >"$(pidfile portal)"
    sleep 0.5
}

start_shell() {
    stop_one shell
    [ -x "$bin/templar-shell" ] || { echo "missing $bin/templar-shell" >&2; return 1; }
    echo "starting shell (trusted socket $shell_socket)"
    nested_env WAYLAND_DISPLAY="$shell_socket" RUST_LOG="${RUST_LOG:-info}" \
        "$bin/templar-shell" --surface \
        >"$(logfile shell)" 2>&1 &
    echo $! >"$(pidfile shell)"
    sleep 1
    alive shell || { echo "  the shell exited immediately; see $(logfile shell)" >&2; return 1; }
}

case "${1:-start}" in
start)
    start_comp && start_sessiond && start_filed && start_portal && start_shell \
        && echo "nested session up — logs in $run"
    ;;
shell)
    # The hot-swap loop: rebuild only the shell and restart it against the live compositor.
    ( cd "$root" && cargo build -p templar-shell ${profile:+$([ "$profile" = release ] && echo --release)} ) || exit 1
    start_shell && echo "shell restarted"
    ;;
launch)
    [ $# -ge 2 ] || { echo "usage: $0 launch <app-id>" >&2; exit 2; }
    "$bin/templar" app launch "$2"
    ;;
apps)
    "$bin/templar" app list 2>/dev/null | head -40
    ;;
logs)
    for name in comp sessiond filed portal shell; do
        echo "===== $name"
        tail -n "${2:-25}" "$(logfile "$name")" 2>/dev/null || echo "(no log)"
    done
    ;;
stop)
    for name in shell portal filed sessiond comp; do stop_one "$name"; done
    clear_stale_sockets
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
