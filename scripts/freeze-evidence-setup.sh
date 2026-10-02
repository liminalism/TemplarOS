#!/usr/bin/env bash
# The root-only half of `templar doctor freeze`: collect what a frozen boot left in the system
# journal, then arm this machine so the next freeze leaves enough to triage.
#
#   sudo scripts/freeze-evidence-setup.sh                  collect boot -1, then arm the machine
#   sudo scripts/freeze-evidence-setup.sh --boot <id|-N>   collect another boot
#   sudo scripts/freeze-evidence-setup.sh --collect-only   collect, change nothing
#
# Why root at all: the installer used to create the account without `adm`, so `journalctl` run by
# that user shows their own session and nothing else — not the kernel, not logind, not greetd. The
# GPU/driver half of a freeze lives exactly there. After this script the user is in `adm` (from
# the next login on) and `templar doctor freeze` needs no sudo.
#
# Arming installs the same files the templaros-hostd package ships (services/templar-hostd's
# [package.metadata.deb] assets), at the same paths, so the next deployment simply takes them over:
#   /usr/lib/sysctl.d/60-templaros-hang-evidence.conf   SysRq dumps, hung-task/lockup reporting, panic=10
#   /usr/lib/systemd/journald.conf.d/60-templaros.conf  persistent journal, 15 s sync
#   /usr/lib/systemd/coredump.conf.d/templaros.conf     keep cores locally
# plus the systemd-coredump package. It restarts systemd-journald (it has no reload) and applies the
# sysctls; it never touches a templar-* unit or the running session.
set -euo pipefail

boot=-1
collect_only=0
while [ $# -gt 0 ]; do
    case "$1" in
        --boot) boot="${2:?--boot needs a value}"; shift 2 ;;
        --boot=*) boot="${1#--boot=}"; shift ;;
        --collect-only) collect_only=1; shift ;;
        -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

[ "$(id -u)" = 0 ] || { echo "run with sudo: it reads the system journal and writes /usr/lib" >&2; exit 1; }
user="${SUDO_USER:-}"
[ -n "$user" ] && [ "$user" != root ] || { echo "run through sudo from the desktop user's shell (SUDO_USER is unset)" >&2; exit 1; }
home="$(getent passwd "$user" | cut -d: -f6)"
group="$(id -gn "$user")"
repo="$(cd "$(dirname "$0")/.." && pwd)"

say() { printf '\n== %s\n' "$*"; }

# ---------------------------------------------------------------------------------------------
say "collecting boot $boot"
boot_id="$(journalctl -b "$boot" -n 1 -o export --output-fields=_BOOT_ID 2>/dev/null \
    | sed -n 's/^_BOOT_ID=//p' | head -n 1)"
if [ -z "$boot_id" ]; then
    echo "no journal for boot $boot (journalctl --list-boots shows what exists)" >&2
    exit 1
fi
out="$home/.local/state/templaros/freeze-reports/boot-$boot_id-root"
install -d -m 0750 -o "$user" -g "$group" "$out"
last_epoch="$(journalctl -b "$boot_id" -n 1 -o short-unix --no-pager -q | awk 'NR==1 {print int($1)}')"

journalctl -b "$boot_id" -k -o short-precise --no-pager -q > "$out/kernel.txt" || true
journalctl -b "$boot_id" -o short-precise --no-pager -q > "$out/system-all.txt" || true
journalctl -b "$boot_id" -o short-precise --no-pager -q \
    -u systemd-logind -u greetd -u templar-hostd -u templar-boot-health -u systemd-oomd \
    -u systemd-journald -u systemd-coredump@\* > "$out/system-units.txt" || true
if [ -n "$last_epoch" ]; then
    journalctl -b "$boot_id" --since "@$((last_epoch - 900))" -o short-precise --no-pager -q \
        | grep -v 'templar_resourced::daemon: tick' > "$out/last-15min.txt" || true
fi
{
    echo "# kernel and session-end lines worth reading first (boot $boot_id)"
    grep -E -i 'GPU HANG|i915.*(ERROR|error|hang|reset)|xe .*(ERROR|reset)|\*ERROR\*|flip_done timed out|drm.*(timed out|error)|NVRM: Xid|nvidia-modeset.*ERROR|blocked for more than|soft lockup|hard LOCKUP|rcu.*(stall|detected)|Out of memory|oom-kill|invoked oom-killer|BUG:|Oops|Call Trace|BTRFS (error|warning)|sysrq: ' \
        "$out/kernel.txt" || echo "(no kernel GPU/hang/memory markers)"
    echo
    echo "# how the boot ended (logind, power key, shutdown)"
    grep -E 'Power key|power button|System is (powering down|rebooting)|systemd-shutdown|Reached target .*(Power-Off|Reboot|Shutdown)|New session|Removed session|Session [0-9]+ logged out' \
        "$out/system-all.txt" | tail -n 25 || echo "(none)"
} > "$out/findings.txt"
{
    echo "# /sys/fs/pstore"; ls -la /sys/fs/pstore 2>&1 || true
    echo "# /var/lib/systemd/pstore"; ls -laR /var/lib/systemd/pstore 2>&1 || true
} > "$out/pstore-listing.txt"
if [ -d /var/lib/systemd/pstore ] && [ -n "$(ls -A /var/lib/systemd/pstore 2>/dev/null)" ]; then
    cp -a /var/lib/systemd/pstore "$out/pstore"
fi
if command -v coredumpctl >/dev/null; then
    coredumpctl list -b "$boot_id" --no-pager > "$out/coredumps.txt" 2>&1 || true
else
    echo "coredumpctl not installed; core_pattern=$(cat /proc/sys/kernel/core_pattern)" > "$out/coredumps.txt"
fi
chown -R "$user:$group" "$out"
cat "$out/findings.txt"
echo
echo "written: $out"

[ "$collect_only" = 1 ] && exit 0

# ---------------------------------------------------------------------------------------------
say "letting $user read the system journal (group adm; effective from the next login)"
if id -nG "$user" | tr ' ' '\n' | grep -qx adm; then
    echo "already in adm"
else
    usermod -aG adm "$user"
    echo "added"
fi

say "systemd-coredump"
if dpkg-query -W -f='${Status}' systemd-coredump 2>/dev/null | grep -q 'install ok installed'; then
    echo "already installed"
else
    apt-get install -y systemd-coredump
fi

say "evidence policy files (the paths templaros-hostd ships them to)"
install -D -m 0644 "$repo/distro/policy/sysctl.d/60-templaros-hang-evidence.conf" \
    /usr/lib/sysctl.d/60-templaros-hang-evidence.conf
install -D -m 0644 "$repo/distro/policy/journald/journald.conf.d/60-templaros.conf" \
    /usr/lib/systemd/journald.conf.d/60-templaros.conf
install -D -m 0644 "$repo/distro/policy/coredump/coredump.conf.d/templaros.conf" \
    /usr/lib/systemd/coredump.conf.d/templaros.conf
sysctl -q -p /usr/lib/sysctl.d/60-templaros-hang-evidence.conf
if [ -f /usr/lib/sysctl.d/50-coredump.conf ] && ! grep -q systemd-coredump /proc/sys/kernel/core_pattern; then
    sysctl -q -p /usr/lib/sysctl.d/50-coredump.conf
fi
# journald has no reload; a restart keeps every service's stdout stream (they live in its fd store).
systemctl restart systemd-journald
systemctl is-enabled --quiet systemd-pstore.service || systemctl enable systemd-pstore.service

say "armed"
for key in sysrq hung_task_timeout_secs hung_task_warnings softlockup_all_cpu_backtrace \
    hardlockup_all_cpu_backtrace hardlockup_panic panic core_pattern; do
    printf '  kernel.%-30s %s\n' "$key" "$(cat "/proc/sys/kernel/$key")"
done
printf '  %-37s %s\n' "pstore backend" "$(cat /sys/module/pstore/parameters/backend 2>/dev/null || echo none)"
printf '  %-37s %s\n' "journald" "$(systemd-analyze cat-config systemd/journald.conf 2>/dev/null \
    | grep -E '^(Storage|SyncIntervalSec)=' | tr '\n' ' ')"

cat <<'NEXT'

Log out and back in once (or reboot): the adm group, and the core-size limit systemd-coredump's
DefaultLimitCORE=infinity drop-in gives the user manager, only reach a session started after this.
The compositor watchdog arrives with the next templaros-comp build you deploy; to have a stall of
N seconds abort templar-comp into a core dump (and back to the greeter) instead of waiting for you:
  systemctl --user edit templar-comp.service    ->   [Service]
                                                     Environment=TEMPLAR_COMP_WATCHDOG_ABORT_AFTER_SECS=60
(takes effect at the next login; it ends the session, so leave it off if you would rather wait).

At the next freeze (display and pointer stuck):
  1. Wait 15 seconds: the compositor watchdog logs a stall after 5 s, with where its main thread is.
  2. Hold Alt, press and release SysRq (PrtSc; Fn+PrtSc on some keyboards), keep Alt held and press
     W (blocked tasks' kernel stacks), then L (every CPU's backtrace). Wait 5 seconds.
  3. Press the power button briefly once: logind shuts down in order and the journal is kept.
     Only if nothing happens within a minute: Alt+SysRq+S (sync), wait 2 s, U (read-only), B (reboot).
  4. After logging back in: templar doctor freeze      (report in ~/.local/state/templaros/freeze-reports/)
NEXT
