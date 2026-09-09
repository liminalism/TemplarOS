#!/usr/bin/env bash
# One desktop's memory footprint, measured the same way on any of them.
#
# The point is comparison: run it on the machine you have now, run it on TemplarOS later, and the
# two numbers mean the same thing. So it reads only what every Linux exposes — /proc/meminfo and
# per-process /proc/*/smaps_rollup — and never a desktop-specific tool.
#
# PSS, not RSS. RSS counts a shared library once per process that maps it, so summing RSS over a
# session double-counts every toolkit and comes out larger than the machine's total. PSS divides
# each shared page by the number of processes sharing it, so a session's PSS sums to something a
# person can compare. Reading it needs privilege for processes you do not own; run as the desktop
# user and it covers that user's session, which is the part a desktop is responsible for.
#
# Read it at an idle desktop shortly after logging in. A machine that has been compiling has a
# page cache full of build output, and "used" will be dominated by reclaimable cache that says
# nothing about the desktop.
set -euo pipefail
mib() { awk -v k="$1" '$1 == k":" { printf "%d", $2/1024 }' /proc/meminfo; }

printf '# memory snapshot\n'
printf 'when              %s\n' "$(date -Is)"
printf 'host              %s\n' "$(uname -n)"
printf 'kernel            %s\n' "$(uname -sr)"
printf 'os                %s\n' "$(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}")"
printf 'desktop           %s\n' "${XDG_CURRENT_DESKTOP:-${DESKTOP_SESSION:-unknown}}"
printf 'uptime            %s\n' "$(awk '{printf "%.1f hours", $1/3600}' /proc/uptime)"
printf '\n'
printf 'MemTotal          %6d MiB\n' "$(mib MemTotal)"
printf 'MemAvailable      %6d MiB   <- what a new program can have without swapping\n' "$(mib MemAvailable)"
printf 'MemFree           %6d MiB   <- not a useful number on its own; cache is reclaimable\n' "$(mib MemFree)"
printf 'Buffers+Cached    %6d MiB\n' "$(( $(mib Buffers) + $(mib Cached) ))"
printf 'Shmem             %6d MiB   <- tmpfs, /dev/shm, graphics buffers\n' "$(mib Shmem)"
printf 'Slab              %6d MiB   <- kernel structures\n' "$(mib Slab)"
printf 'SwapTotal/Free    %6d / %d MiB\n' "$(mib SwapTotal)" "$(mib SwapFree)"
printf '\n'
printf 'in use, excluding reclaimable cache:\n'
printf '  MemTotal - MemAvailable  %6d MiB\n' "$(( $(mib MemTotal) - $(mib MemAvailable) ))"
printf '\n'

# Per-process PSS for this user, largest first. Anything unreadable is skipped rather than guessed.
printf 'session processes by PSS (this user):\n'
total=0
{
    for rollup in /proc/[0-9]*/smaps_rollup; do
        pid="${rollup#/proc/}"; pid="${pid%/smaps_rollup}"
        [ -O "$rollup" ] || continue
        pss="$(awk '/^Pss:/ { s += $2 } END { printf "%d", s }' "$rollup" 2>/dev/null || true)"
        [ -n "$pss" ] && [ "$pss" -gt 0 ] || continue
        comm="$(tr -d '\0' < "/proc/$pid/comm" 2>/dev/null || echo '?')"
        printf '%d %s\n' "$pss" "$comm"
    done
} | sort -rn > /tmp/.memsnap.$$
total="$(awk '{ s += $1 } END { printf "%d", s/1024 }' /tmp/.memsnap.$$)"
head -20 /tmp/.memsnap.$$ | awk '{ printf "  %8.1f MiB  %s\n", $1/1024, $2 }'
count="$(wc -l < /tmp/.memsnap.$$)"
rm -f /tmp/.memsnap.$$
printf '  ---\n'
printf '  %8d MiB  TOTAL PSS over %d processes owned by %s\n' "$total" "$count" "${USER:-$(id -un)}"
printf '\n'

# The number worth comparing between two desktops is what the desktop itself costs, not what the
# person happened to have open. Both spellings, so a snapshot taken before the rename and one
# taken after are the same measurement.
printf 'the desktop itself (compositor, shell, session services):\n'
desktop=0
for name in comp shell sessiond resourced filed hostd appd greeter portal; do
    for prefix in templar lege; do
        for pid in $(pgrep -u "$(id -u)" -x "$prefix-$name" 2>/dev/null || true); do
            rollup="/proc/$pid/smaps_rollup"
            [ -O "$rollup" ] || continue
            pss="$(awk '/^Pss:/ { s += $2 } END { printf "%d", s }' "$rollup" 2>/dev/null || echo 0)"
            [ "$pss" -gt 0 ] || continue
            printf '  %8.1f MiB  %s-%s\n' "$(awk -v p="$pss" 'BEGIN { print p/1024 }')" "$prefix" "$name"
            desktop=$(( desktop + pss ))
        done
    done
done
printf '  ---\n'
printf '  %8d MiB  DESKTOP TOTAL PSS\n' "$(( desktop / 1024 ))"
