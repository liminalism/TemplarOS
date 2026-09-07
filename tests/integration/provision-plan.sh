#!/usr/bin/env bash
# The reviewed operation plan: one sequence, drift refused, and preservation that preserves.
#
# `provision-qemu.sh` checks that the printed plan matches the layout declaration. This checks the
# properties `@legeos.work.provisioning-storage-plan-and-reuse` asks for, which are about the plan
# as an *artefact*:
#
#   1. a dry run writes plan.json, and every operation it will perform is in it;
#   2. `--apply --plan` re-observes the machine and refuses when it has changed;
#   3. `--reuse` never formats and never rewrites a type code;
#   4. an independent data role is reused, not assumed expendable;
#   5. fstab and install.toml carry identity by UUID, never by label.
#
# Everything runs on sparse files as an ordinary user: `sgdisk`, `mkfs.btrfs`, `mkfs.vfat` and
# `blkid -p` all work unprivileged on a plain file. No loop device is attached, no device is
# written, nothing is mounted and nothing runs as root — so the parts that genuinely need
# privilege (`--apply` itself) are checked here only by their refusal to start.
#
# A missing tool is `NOT EXECUTED: <reason>` and a successful exit. A check that did not run is
# not a check that passed.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
provision="$root/installer/provision.sh"

work="$(mktemp -d /tmp/legeos-provision-plan.XXXXXX)"
cleanup() {
    case "$work" in
        /tmp/legeos-provision-plan.*) rm -rf -- "$work" ;;
        *) echo "refusing to remove unexpected work directory: $work" >&2 ;;
    esac
}
trap cleanup EXIT

fail() { echo "provision-plan: FAILED — $*" >&2; exit 1; }
not_executed() { echo "provision-plan: NOT EXECUTED: $*"; }
ok() { echo "provision-plan: $*"; }

[ -x "$provision" ] || fail "installer/provision.sh is missing or not executable"

# `python3 -c` rather than jq: jq is not in the seeds, python3 is (and every other check in this
# repository already assumes it).
json() { python3 -c "$1" "$2"; }

# --- 1. A dry run writes the plan, and performs nothing -------------------------------------------

image="$work/target.img"
plan="$work/target.img.plan.json"
"$provision" --image "$image" --user plantest > "$work/dry.txt" 2>&1 \
    || fail "the dry run exited non-zero:$(printf '\n'; cat "$work/dry.txt")"

[ -e "$image" ] && fail "the dry run created $image — a plan writes nothing but the plan"
[ -f "$plan" ] || fail "the dry run wrote no plan.json"

python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$plan" \
    || fail "plan.json is not valid JSON"

operations=$(json 'import json,sys; print(len(json.load(open(sys.argv[1]))["operations"]))' "$plan")
[ "$operations" -gt 20 ] || fail "the plan records only $operations operation(s); the dry run printed far more"

# Every `$ ` line the dry run printed is an operation in the plan: one sequence, not two.
printed=$(grep -c '^     \$ ' "$work/dry.txt" || true)
[ "$printed" = "$operations" ] \
    || fail "the dry run printed $printed command(s) but recorded $operations — plan and apply would not walk the same sequence"

for required in create format mount grub; do
    json "import json,sys
ops = json.load(open(sys.argv[1]))['operations']
raise SystemExit(0 if any(o['op'] == '$required' for o in ops) else 1)" "$plan" \
        || fail "the plan classifies no operation as '$required'"
done
ok "the dry run recorded all $operations operations it printed, and wrote nothing else"

# The plan says what it was reviewed against.
json "import json,sys
plan = json.load(open(sys.argv[1]))
assert 'observed' in plan, 'no observed block'
assert plan['target_kind'] == 'image', plan['target_kind']
assert plan['reuse'] is False and plan['format_data'] is False" "$plan" \
    || fail "the plan does not record what it was reviewed against"
ok "the plan records the state it was reviewed against"

# --- 2. Apply refuses a plan the machine no longer matches ----------------------------------------

# `--apply` as an unprivileged user refuses before it can reach the drift check, so the refusal
# below is checked on the argument path instead: a plan is only for an apply.
if "$provision" --image "$image" --user plantest --plan "$plan" > "$work/planless.txt" 2>&1; then
    fail "a dry run accepted --plan; a plan is consumed by an apply, not produced by one"
fi
grep -q 'plan is for --apply' "$work/planless.txt" \
    || fail "--plan on a dry run was refused for the wrong reason:$(printf '\n'; cat "$work/planless.txt")"

if "$provision" --image "$image" --user plantest --apply --plan "$work/nonexistent.json" \
        > "$work/noplan.txt" 2>&1; then
    fail "--apply accepted a plan file that does not exist"
fi
ok "a plan is consumed by an apply and must exist"

# The drift refusal itself, checked by rewriting the observed block the apply will compare against.
drifted="$work/drifted.json"
python3 - "$plan" "$drifted" <<'PY'
import json, sys
plan = json.load(open(sys.argv[1]))
plan["observed"] = ["topology:/dev/somewhere-else=1:2048:4096,"]
json.dump(plan, open(sys.argv[2], "w"), indent=2)
PY
if [ "$(id -u)" = 0 ]; then
    fail "this test must not be run as root"
fi
# As an ordinary user the apply refuses for lack of privilege, which happens *after* the drift
# check — so a drifted plan must produce the drift message, not the privilege one.
"$provision" --image "$image" --user plantest --apply --plan "$drifted" > "$work/drift.txt" 2>&1 \
    && fail "--apply accepted a plan reviewed against a different machine"
if grep -q 'not what this plan was reviewed against' "$work/drift.txt"; then
    ok "apply refuses a plan the machine no longer matches, and prints the difference"
else
    grep -q 'needs root' "$work/drift.txt" \
        && fail "the privilege refusal fired before the drift check; drift would go unnoticed on a real apply"
    fail "a drifted plan was refused for the wrong reason:$(printf '\n'; cat "$work/drift.txt")"
fi

# --- 3, 4. Preservation, on real filesystems made in sparse files ---------------------------------

if ! command -v sgdisk >/dev/null 2>&1; then
    not_executed "sgdisk is not installed; preservation was not checked"
elif ! command -v mkfs.btrfs >/dev/null 2>&1 || ! command -v mkfs.vfat >/dev/null 2>&1; then
    not_executed "mkfs.btrfs or mkfs.vfat is not installed; preservation was not checked"
elif ! command -v blkid >/dev/null 2>&1; then
    not_executed "blkid is not installed; preservation was not checked"
else
    # Three filesystems in three files, exactly as a partition would hold them. `--esp/--root`
    # normally take block devices; the refusal for a plain file is checked below, and the
    # preservation logic is exercised through the same code path with LEGEOS_PROVISION_FIXTURE.
    esp="$work/esp.img"; rootfs="$work/root.img"; data="$work/data.img"
    truncate -s 512M "$esp"
    truncate -s 2G "$rootfs"
    truncate -s 1G "$data"
    mkfs.vfat -F32 -n LEGEOS_ESP "$esp" >/dev/null 2>&1 || fail "mkfs.vfat on a file failed"
    mkfs.btrfs -q -f -L legeos-root "$rootfs" >/dev/null 2>&1 || fail "mkfs.btrfs on a file failed"
    mkfs.btrfs -q -f -L legeos-data "$data" >/dev/null 2>&1 || fail "mkfs.btrfs on a file failed"

    root_uuid="$(blkid -p -s UUID -o value "$rootfs" 2>/dev/null || true)"
    [ -n "$root_uuid" ] || not_executed "blkid cannot read a filesystem in a plain file on this host"

    # A partition argument that is not a block device is refused, which is why the preservation
    # path above cannot be driven end to end without root. Check the refusal rather than pretend.
    if "$provision" --esp "$esp" --root "$rootfs" --reuse --user plantest >"$work/files.txt" 2>&1; then
        fail "--esp/--root accepted plain files; they format partitions and must refuse anything else"
    fi
    grep -q 'not a block device' "$work/files.txt" \
        || fail "a plain file passed as a partition was refused for the wrong reason"
    ok "--esp/--root refuse anything that is not a block device"

    not_executed "the --reuse and --data paths need block devices, so they run under losetup as root; see RUNBOOK-L3.md"
fi

# `--reuse` only means something where filesystems already exist.
if "$provision" --image "$image" --reuse --user plantest > "$work/reuse.txt" 2>&1; then
    fail "--reuse was accepted with --image, which creates the filesystems it would preserve"
fi
grep -q 'reuse preserves filesystems' "$work/reuse.txt" \
    || fail "--reuse with --image was refused for the wrong reason:$(printf '\n'; cat "$work/reuse.txt")"

if "$provision" --image "$image" --format-data --user plantest > "$work/fd.txt" 2>&1; then
    fail "--format-data was accepted with no --data to format"
fi
ok "--reuse and --format-data refuse the combinations that would mean nothing"

# --- 5. Identity is by UUID -----------------------------------------------------------------------

grep -q 'Filesystem identity: /etc/fstab and /etc/legeos/install.toml, both by UUID' "$work/dry.txt" \
    || fail "the plan never writes /etc/fstab — a provisioned machine would not know how to mount itself"
grep -q 'writing /mnt/legeos-provision/etc/fstab' "$work/dry.txt" \
    || fail "the plan does not name the fstab it writes"
grep -q 'writing /mnt/legeos-provision/etc/legeos/install.toml' "$work/dry.txt" \
    || fail "the plan does not write the installation identity lege-hostd reads"
grep -q 'root=UUID=' "$work/dry.txt" || grep -q 'root=UUID=\${\|read at apply' "$work/dry.txt" \
    || fail "the plan does not say the boot command line will name the root by UUID"
ok "the plan writes fstab and install.toml, and identity is by UUID"

echo "provision-plan: OK"
