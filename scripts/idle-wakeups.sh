#!/usr/bin/env bash
# What each TemplarOS daemon costs while nothing is happening.
#
# `cargo xtask index-bench idle` answers this for one process it starts itself, against a fixture
# tree. This answers it for the daemons actually running on this machine, which is the measurement
# @templaros.work.event-driven-service-audit's `idle-activity` check asks for: "native Lubuntu
# measurements show reduced or justified idle wakeups without missed state changes or recovery
# regressions".
#
# It reads /proc and nothing else. No unit is started, stopped, signalled or reconfigured, nothing
# is written outside the output file, and the live session is not touched — which is what makes it
# safe for an agent to run rather than a step for a person (AGENTS.md).
#
#   scripts/idle-wakeups.sh                       # 60 seconds, JSON to stdout
#   scripts/idle-wakeups.sh 300 > docs/measurements/idle-<label>.json
#
# The fields are the ones docs/measurements/idle-after-head.json already uses, so a run here is
# comparable with the index watcher's own before/after:
#
#   schedstat_pcount_delta        times the scheduler ran this process (its wakeups)
#   voluntary_ctxt_switches_delta times it blocked of its own accord
#   read_bytes_delta              bytes actually fetched from a block device
#   syscw_delta / wchar_delta     write syscalls, and bytes written
#
# Leave the machine alone while it runs. A daemon that woke because you moved the mouse is not
# idle, and this script cannot tell the difference.
set -euo pipefail

SECONDS_TO_WATCH=${1:-60}
case $SECONDS_TO_WATCH in
    ''|*[!0-9]*) echo "usage: $0 [seconds]" >&2; exit 2 ;;
esac

# The daemons this repository owns. Matched against /proc/<pid>/comm, which is the truncated
# binary name and cannot be confused with a command line that merely mentions one.
# `TEMPLAR_IDLE_DAEMONS` overrides the list so a nested session (scripts/nested-session.sh) or a
# single process can be measured without editing this file.
DAEMONS=${TEMPLAR_IDLE_DAEMONS:-"templar-sessiond templar-resourced templar-filed templar-hostd templar-comp templar-shell"}

# The one field of /proc/<pid>/status or /proc/<pid>/io named $2, or empty.
field() { sed -n "s/^$2:[[:space:]]*//p" "/proc/$1/$3" 2>/dev/null | head -1; }

# schedstat is three unlabelled numbers: run time, wait time, and the number of timeslices — the
# third is the one that counts wakeups.
timeslices() { awk '{ print $3 }' "/proc/$1/schedstat" 2>/dev/null; }

sample() { # pid -> "timeslices voluntary read_bytes syscw wchar"
    printf '%s %s %s %s %s\n' \
        "$(timeslices "$1")" \
        "$(field "$1" voluntary_ctxt_switches status)" \
        "$(field "$1" read_bytes io)" \
        "$(field "$1" syscw io)" \
        "$(field "$1" wchar io)"
}

# Every running pid whose comm is one of ours. `pgrep -x` matches the executable name exactly, so
# this cannot pick up an editor with a daemon's name in its title — and it only ever reads.
pids_of() { pgrep -x "$1" 2>/dev/null || true; }

declare -A BEFORE=()
declare -A NAME=()
for daemon in $DAEMONS; do
    for pid in $(pids_of "$daemon"); do
        reading=$(sample "$pid")
        # A process whose /proc entries could not be read at all (permissions, or it exited
        # between the pgrep and the read) is skipped rather than reported as zero.
        [ -n "${reading// /}" ] || continue
        BEFORE[$pid]="$reading"
        NAME[$pid]="$daemon"
    done
done

if [ ${#BEFORE[@]} -eq 0 ]; then
    printf '{\n  "seconds": %s,\n  "daemons": [],\n' "$SECONDS_TO_WATCH"
    printf '  "note": "NOT EXECUTED: none of the TemplarOS daemons (%s) is running on this host"\n}\n' "$DAEMONS"
    exit 0
fi

echo "idle-wakeups: watching ${#BEFORE[@]} process(es) for ${SECONDS_TO_WATCH}s — leave the machine alone" >&2
sleep "$SECONDS_TO_WATCH"

delta() { # before after -> after-before, or null when either reading is missing
    if [ -z "${1:-}" ] || [ -z "${2:-}" ]; then printf 'null'; else printf '%s' "$(( $2 - $1 ))"; fi
}

printf '{\n'
printf '  "label": "%s",\n' "${TEMPLAR_IDLE_LABEL:-$(uname -sr) $(date -u +%Y-%m-%dT%H:%M:%SZ)}"
printf '  "seconds": %s,\n' "$SECONDS_TO_WATCH"
printf '  "daemons": [\n'
first=1
for pid in "${!BEFORE[@]}"; do
    read -r b_slices b_vol b_read b_syscw b_wchar <<<"${BEFORE[$pid]}"
    # The process may have exited during the window; say so rather than reporting a fabricated
    # delta against a missing reading.
    if [ ! -d "/proc/$pid" ]; then
        after=""
    else
        after=$(sample "$pid")
    fi
    read -r a_slices a_vol a_read a_syscw a_wchar <<<"$after"
    [ $first -eq 1 ] || printf ',\n'
    first=0
    printf '    {"name": "%s", "pid": %s, "exited": %s, ' \
        "${NAME[$pid]}" "$pid" "$([ -d "/proc/$pid" ] && echo false || echo true)"
    printf '"schedstat_pcount_delta": %s, ' "$(delta "$b_slices" "${a_slices:-}")"
    printf '"voluntary_ctxt_switches_delta": %s, ' "$(delta "$b_vol" "${a_vol:-}")"
    printf '"read_bytes_delta": %s, ' "$(delta "$b_read" "${a_read:-}")"
    printf '"syscw_delta": %s, ' "$(delta "$b_syscw" "${a_syscw:-}")"
    printf '"wchar_delta": %s}' "$(delta "$b_wchar" "${a_wchar:-}")"
done
printf '\n  ]\n}\n'
