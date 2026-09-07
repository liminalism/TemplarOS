#!/usr/bin/env bash
# What this machine actually needs from a kernel and a firmware package, read from the running
# system. The ISO is built from a declaration; this is the other half of the pair — the facts a
# declaration has to cover, so "will the ISO leave me without video or networking" has an answer
# before anyone writes a USB stick.
#
#   scripts/hardware-inventory.sh [OUT.json]     (default: stdout)
#
# Everything here is read-only and unprivileged: /proc, /sys, lsmod, modinfo and the kernel log.
# `modinfo -F firmware` is what a module *declares* it may request; the kernel log is what was
# actually loaded or refused. Both are reported, because a module that never asked for its firmware
# on this boot may still need it on another machine's.
set -euo pipefail
out="${1:-}"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

# The kernel log, or an honest refusal. dmesg is restricted by kernel.dmesg_restrict on Ubuntu;
# journalctl -k works for members of `adm`, which is the ordinary developer account.
if journalctl -k -b >"$work/klog" 2>/dev/null && [ -s "$work/klog" ]; then
    klog_source="journalctl -k -b"
elif dmesg >"$work/klog" 2>/dev/null && [ -s "$work/klog" ]; then
    klog_source="dmesg"
else
    : > "$work/klog"
    klog_source="NOT AVAILABLE (dmesg is restricted and journalctl -k refused)"
fi

# modules.tsv: name <tab> space-separated firmware paths the module declares
lsmod | awk 'NR>1 {print $1}' | sort > "$work/modules"
# name <tab> firmware paths <tab> where the running kernel loaded it from. The origin turns a
# "no package provides this" row in the closure into an answer: out-of-tree DKMS modules are not
# expected to be in the base, in-tree ones that are absent have been renamed or built in.
while read -r m; do
    printf '%s\t%s\t%s\n' "$m" \
        "$(modinfo -F firmware "$m" 2>/dev/null | sort -u | tr '\n' ' ')" \
        "$(case "$(modinfo -F filename "$m" 2>/dev/null)" in
            */updates/dkms/*) echo dkms ;; "(builtin)"|"") echo builtin ;; *) echo kernel ;;
        esac)"
done < "$work/modules" > "$work/modules.tsv"

# Firmware the kernel says it loaded or could not load, on this boot.
# Any firmware-looking path the kernel mentioned. Some drivers (cs35l41) name the file without
# ever saying "firmware" on that line, so the whole log is scanned and the extension does the
# filtering; this is a best-effort record of what was touched, not an authority.
grep -E '\.(bin|wmfw|sfi|ddc|db)( |:|$)' "$work/klog" \
    | grep -oE '[A-Za-z0-9_-]+(/[A-Za-z0-9_.+-]+)+\.(bin|wmfw|sfi|ddc|db)' | sort -u > "$work/fw-seen" || true
grep -E 'Direct firmware load for .* failed|Falling back to sysfs fallback' "$work/klog" \
    | sed -E 's/.*Direct firmware load for ([^ ]+) failed.*/\1/' | sort -u > "$work/fw-failed" || true

# DRM: card, driver, connected connectors. The compositor picks a card with a driver bound and at
# least one connected connector; naming both is what makes a preflight failure diagnosable.
: > "$work/drm.tsv"
for card in /sys/class/drm/card[0-9]*; do
    [ -e "$card/device/driver" ] || continue
    name="$(basename "$card")"
    case "$name" in *-*) continue ;; esac
    connected=""
    for conn in /sys/class/drm/"$name"-*; do
        [ -r "$conn/status" ] || continue
        [ "$(cat "$conn/status")" = connected ] && connected="$connected ${conn##*/"$name"-}"
    done
    printf '%s\t%s\t%s\t%s\n' "$name" "$(basename "$(readlink -f "$card/device/driver")")" \
        "$([ -e "/dev/dri/$name" ] && echo yes || echo no)" "${connected# }" >> "$work/drm.tsv"
done

# Network: every interface, its driver, whether it is a radio. A machine with no wired port (this
# laptop has none) makes "at least one interface with a driver" the only honest preflight rule.
: > "$work/net.tsv"
for n in /sys/class/net/*; do
    name="$(basename "$n")"
    [ "$name" = lo ] && continue
    driver=""
    [ -e "$n/device/driver" ] && driver="$(basename "$(readlink -f "$n/device/driver")")"
    printf '%s\t%s\t%s\t%s\n' "$name" "${driver:-none}" \
        "$([ -d "$n/wireless" ] && echo yes || echo no)" "$(cat "$n/operstate" 2>/dev/null || echo unknown)" >> "$work/net.tsv"
done

# Storage controllers, input device names, sound cards.
: > "$work/storage.tsv"
for b in /sys/block/*; do
    name="$(basename "$b")"
    case "$name" in loop*|zram*|ram*) continue ;; esac
    # NVMe hangs the driver off the controller, one level above the namespace device.
    driver=""
    for d in "$b/device/driver" "$b/device/device/driver"; do
        [ -e "$d" ] && { driver="$(basename "$(readlink -f "$d")")"; break; }
    done
    printf '%s\t%s\n' "$name" "${driver:-none}" >> "$work/storage.tsv"
done
cat /sys/class/input/event*/device/name 2>/dev/null | sort -u > "$work/input"
sed -n 's/^ *[0-9]* \[ *\([^] ]*\) *\].*: \(.*\)$/\1: \2/p' /proc/asound/cards 2>/dev/null \
    | sed 's/ *$//' > "$work/audio" || true

python3 - "$work" "$klog_source" "$out" <<'PY'
import json, os, subprocess, sys, time
work, klog_source, out = sys.argv[1], sys.argv[2], sys.argv[3]

def lines(name):
    path = os.path.join(work, name)
    if not os.path.exists(path):
        return []
    return [l.rstrip("\n") for l in open(path) if l.strip()]

def rows(name):
    return [l.split("\t") for l in lines(name)]

def field(path, key, sep=":"):
    for line in open(path):
        if line.startswith(key):
            return line.split(sep, 1)[1].strip()
    return ""

modules = [{"name": r[0], "firmware": (r[1].split() if len(r) > 1 else []),
             "origin": (r[2] if len(r) > 2 else "unknown")} for r in rows("modules.tsv")]
inventory = {
    "observed_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "host": os.uname().nodename,
    "product": (open("/sys/class/dmi/id/product_name").read().strip()
                if os.path.exists("/sys/class/dmi/id/product_name") else ""),
    "kernel": os.uname().release,
    "kernel_log_source": klog_source,
    "cpu": {
        "vendor": field("/proc/cpuinfo", "vendor_id"),
        "model": field("/proc/cpuinfo", "model name"),
        "microcode": field("/proc/cpuinfo", "microcode"),
    },
    "uefi": os.path.isdir("/sys/firmware/efi"),
    "modules": modules,
    "firmware_declared": sorted({f for m in modules for f in m["firmware"]}),
    "firmware_loaded": lines("fw-seen"),
    "firmware_failed": lines("fw-failed"),
    "drm": [{"card": r[0], "driver": r[1], "device_node": r[2] == "yes",
             "connected": r[3].split() if len(r) > 3 else []} for r in rows("drm.tsv")],
    "net": [{"name": r[0], "driver": r[1], "wireless": r[2] == "yes", "operstate": r[3]}
            for r in rows("net.tsv")],
    "regulatory_domain": (subprocess.run(["iw", "reg", "get"], capture_output=True, text=True).stdout
                          .split("country ", 1)[-1][:2] if os.path.exists("/sbin/iw") or os.path.exists("/usr/sbin/iw") else "unknown"),
    "storage": [{"name": r[0], "driver": r[1]} for r in rows("storage.tsv")],
    "input": lines("input"),
    "audio": lines("audio"),
}
text = json.dumps(inventory, indent=2, sort_keys=True) + "\n"
if out:
    open(out, "w").write(text)
    print(f"hardware-inventory: {len(modules)} modules, "
          f"{len(inventory['firmware_declared'])} declared firmware files -> {out}", file=sys.stderr)
else:
    sys.stdout.write(text)
PY
