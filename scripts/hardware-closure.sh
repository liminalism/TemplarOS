#!/usr/bin/env bash
# Join an inventory from scripts/hardware-inventory.sh to the package that would provide each
# module and each firmware file on the base the ISO is built from — so "the ISO will have no
# video/network/sound on this machine" is answered before the ISO exists.
#
#   scripts/hardware-closure.sh --inventory docs/measurements/hardware-inventory-<host>-<date>.json \
#       [--contents FILE.gz] [--out FILE.md]
#
# Route: Ubuntu publishes one file->package index per suite at dists/<codename>/Contents-amd64.gz
# (57 MB; the per-component paths do not exist). It is fetched ONCE from the snapshot serial pinned
# in distro/base.toml and cached; every later run is offline. No apt-file, no private apt tree, no
# .deb downloads.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
inventory=""; contents=""; out=""
while [ $# -gt 0 ]; do
    case "$1" in
        --inventory) inventory="${2:?}"; shift 2 ;;
        --contents) contents="${2:?}"; shift 2 ;;
        --out) out="${2:?}"; shift 2 ;;
        *) echo "usage: $0 --inventory FILE.json [--contents FILE.gz] [--out FILE.md]" >&2; exit 2 ;;
    esac
done
[ -n "$inventory" ] && [ -r "$inventory" ] || { echo "closure: --inventory FILE.json is required" >&2; exit 2; }
out="${out:-${inventory%.json}.md}"

base_value() { sed -n "s/^$1 *= *\"\([^\"]*\)\".*/\1/p" "$root/distro/base.toml" | head -1; }
codename="$(base_value codename)"
snapshot="$(base_value snapshot_url)"
serial="$(base_value snapshot_serial)"
contents="${contents:-$root/.agent/scratch/contents-$codename-$serial-amd64.gz}"
if [ ! -s "$contents" ]; then
    url="$snapshot/dists/$codename/Contents-amd64.gz"
    echo "closure: fetching $url (once; cached at $contents)" >&2
    mkdir -p "$(dirname "$contents")"
    curl -fsS -o "$contents.part" "$url" && mv "$contents.part" "$contents" \
        || { echo "closure: NOT CHECKED — could not fetch $url" >&2; exit 1; }
fi

python3 - "$inventory" "$contents" "$out" "$codename" "$serial" "$snapshot" <<'PY'
import fnmatch, gzip, json, sys
inv_path, contents, out, codename, serial, snapshot = sys.argv[1:7]
inv = json.load(open(inv_path))

# What we are looking for. Module files are named with dashes where lsmod prints underscores.
# Module file names use dashes where lsmod prints underscores, and some mix both
# (nls_iso8859-1), so both sides are normalised to underscores.
def norm(name):
    return name.replace("-", "_")
want_mod = {norm(m["name"]) + ".ko": m["name"] for m in inv["modules"]}
loaded = list(inv["firmware_loaded"])
declared = [f for f in inv["firmware_declared"] if "*" not in f]
globs = [f for f in inv["firmware_declared"] if "*" in f]
want_fw = {f: None for f in dict.fromkeys(loaded + declared)}

mod_hits, fw_hits, flavours = {}, {}, {}
with gzip.open(contents, "rt", errors="replace") as fh:
    for line in fh:
        path, _, pkgs = line.rpartition(" ")
        path, pkgs = path.strip(), pkgs.strip()
        if not path:
            continue
        # /lib/firmware/, not "firmware" anywhere in the path: kernel modules live under
        # kernel/drivers/firmware/ too, and swallowing them here loses them from the module table.
        if "lib/firmware/" in path:
            key = path.split("lib/firmware/", 1)[1]
            if key.endswith(".zst"):
                key = key[:-4]
            if key in want_fw:
                fw_hits.setdefault(key, set()).update(pkgs.split(","))
        if "-generic/kernel/" in path:
            base = norm(path.rsplit("/", 1)[1])
            if base.endswith(".zst"):
                base = base[:-4]
            if base in want_mod:
                version = path.split("/kernel/", 1)[0].rsplit("/", 1)[1]
                mod_hits.setdefault(want_mod[base], {}).setdefault(version, set()).update(pkgs.split(","))
                flavours[version] = flavours.get(version, 0) + 1

# One kernel flavour answers for all of them: the newest -generic the index carries.
kernel = max(flavours, key=lambda v: (flavours[v], v)) if flavours else "NONE"
def pkgs_for(mod):
    got = mod_hits.get(mod, {}).get(kernel) or set().union(*mod_hits.get(mod, {}).values()) if mod_hits.get(mod) else set()
    return sorted(p.split("/")[-1] for p in got)

origin = {m["name"]: m.get("origin", "unknown") for m in inv["modules"]}
def mod_answer(mod):
    got = pkgs_for(mod)
    if got:
        return ", ".join(got)
    if origin.get(mod) == "dkms":
        return "out-of-tree (DKMS on the inventoried host; not expected in the base)"
    return "**MISSING**"
missing_mod = sorted(m["name"] for m in inv["modules"]
                     if not pkgs_for(m["name"]) and origin.get(m["name"]) != "dkms")
missing_fw = sorted(f for f in want_fw if f not in fw_hits)
missing_loaded = [f for f in loaded if f not in fw_hits]

def table(rows, head):
    lines = ["| " + " | ".join(head) + " |", "|" + "|".join(["---"] * len(head)) + "|"]
    lines += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(lines)

report = f"""# Hardware inventory and package closure — {inv['host']} ({inv['product']})

Observed {inv['observed_at']} by `scripts/hardware-inventory.sh`; the closure joined by
`scripts/hardware-closure.sh` against Ubuntu **{codename}** snapshot **{serial}**
(`{snapshot}/dists/{codename}/Contents-amd64.gz`, the whole-archive file->package index — one
fetch, then offline). Kernel package family answering here: **{kernel}**. The running kernel when
the inventory was taken was {inv['kernel']} ({inv['kernel_log_source']}); the module *names* are
what carries across, not that version.

## The machine

- CPU {inv['cpu']['model']} ({inv['cpu']['vendor']}, microcode {inv['cpu']['microcode']}), UEFI: {inv['uefi']}
- GPUs: {', '.join(f"{d['card']} {d['driver']} (connected: {', '.join(d['connected']) or 'none'})" for d in inv['drm'])}
- Network: {', '.join(f"{n['name']} {n['driver']}{' (wireless)' if n['wireless'] else ''}" for n in inv['net']) or 'none'}; regulatory domain {inv['regulatory_domain']}
- Audio: {'; '.join(inv['audio'])}
- Storage: {', '.join(f"{s['name']} {s['driver']}" for s in inv['storage'])}
- Input: {', '.join(inv['input'][:8])}{' …' if len(inv['input']) > 8 else ''}

## Firmware the kernel actually loaded on this boot -> package

{table([[f, ", ".join(sorted(p.split('/')[-1] for p in fw_hits[f])) if f in fw_hits else "**MISSING**"] for f in loaded], ["firmware", "package in " + codename])}

## Modules

{len(inv['modules'])} loaded modules; {len(inv['modules']) - len(missing_mod)} are provided by a
{codename} package, {len(missing_mod)} are not.

{table([[m, mod_answer(m)] for m in sorted(m['name'] for m in inv['modules'])], ["module", "package"])}

## Firmware declared by those modules (whether or not this machine asked for it)

{len(want_fw)} exact paths checked, {len(missing_fw)} not in {codename}; {len(globs)} declarations
are glob patterns and are covered by the loaded list above.

{table([[f, "**MISSING**"] for f in missing_fw] or [["—", "none missing"]], ["firmware", "package"])}

## Verdict

- firmware actually in use that no package provides: **{len(missing_loaded)}**{' — ' + ', '.join(missing_loaded) if missing_loaded else ''}
- loaded modules no package provides: **{len(missing_mod)}**{' — ' + ', '.join(missing_mod) if missing_mod else ''}

Every MISSING row is either a seed to add or a recorded reason; a module with no package is usually
out-of-tree (DKMS, a container/virtualisation module) rather than a gap in the base.
"""
open(out, "w").write(report)
print(f"hardware-closure: {len(missing_loaded)} loaded firmware and {len(missing_mod)} modules "
      f"unprovided -> {out}", file=sys.stderr)
PY
