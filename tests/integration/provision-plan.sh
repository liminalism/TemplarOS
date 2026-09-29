#!/usr/bin/env bash
# The reviewed operation plan: one sequence, drift refused, and preservation that preserves.
#
# `provision-qemu.sh` checks that the printed plan matches the layout declaration. This checks the
# properties `@templaros.work.provisioning-storage-plan-and-reuse` asks for, which are about the plan
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

work="$(mktemp -d /tmp/templaros-provision-plan.XXXXXX)"
cleanup() {
    case "$work" in
        /tmp/templaros-provision-plan.*) rm -rf -- "$work" ;;
        *) echo "refusing to remove unexpected work directory: $work" >&2 ;;
    esac
}
trap cleanup EXIT

archive="$work/devrepo"
mkdir -p "$archive/sets/test-set"
printf 'test-set\n' > "$archive/current"
printf '{}\n' > "$archive/sets/test-set/manifest.json"
: > "$archive/Packages"; : > "$archive/Release"; : > "$archive/InRelease"; : > "$archive/templaros-dev.gpg"
export DEVREPO_ROOT="$archive"
export TEMPLAROS_PROVISION_MOUNT="$work/missing/templaros-provision"

fail() { echo "provision-plan: FAILED — $*" >&2; exit 1; }
not_executed() { echo "provision-plan: NOT EXECUTED: $*"; }
ok() { echo "provision-plan: $*"; }

[ -x "$provision" ] || fail "installer/provision.sh is missing or not executable"

# Whole-device detection must query the named row (`lsblk -d ... TYPE`), not infer from PKNAME rows
# emitted for its descendants. The fixture includes every Linux naming family used by the installer.
mkdir -p "$work/fake-bin"
cat > "$work/fake-bin/lsblk" <<'SH'
#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
    /dev/nvme0n1|/dev/nvme1n1|/dev/sda|/dev/mmcblk0) echo disk ;;
    /dev/nvme0n1p1|/dev/nvme0n1p2|/dev/sda1|/dev/mmcblk0p1) echo part ;;
    *) exit 1 ;;
esac
SH
chmod +x "$work/fake-bin/lsblk"
devices='/dev/nvme0n1 /dev/nvme1n1 /dev/nvme0n1p1 /dev/nvme0n1p2 /dev/sda /dev/sda1 /dev/mmcblk0 /dev/mmcblk0p1'
types="$(PATH="$work/fake-bin:$PATH" TEMPLAROS_PROVISION_TEST_BLOCK_TYPES="$devices" "$provision")"
for expected in \
    '/dev/nvme0n1=disk' '/dev/nvme1n1=disk' '/dev/nvme0n1p1=part' '/dev/nvme0n1p2=part' \
    '/dev/sda=disk' '/dev/sda1=part' '/dev/mmcblk0=disk' '/dev/mmcblk0p1=part'; do
    printf '%s\n' "$types" | grep -Fxq "$expected" || fail "device classifier missed $expected"
done
ok "whole disks and partitions are classified from the named lsblk row across NVMe, sd and mmc"

# `python3 -c` rather than jq: jq is not in the seeds, python3 is (and every other check in this
# repository already assumes it).
json() { python3 -c "$1" "$2"; }

# --- 1. A dry run writes the plan, and performs nothing -------------------------------------------

image="$work/target.img"
plan="$work/target.img.plan.json"
"$provision" --image "$image" --user plantest --password-stdin > "$work/dry.txt" 2>&1 \
    || fail "the dry run exited non-zero:$(printf '\n'; cat "$work/dry.txt")"

[ -e "$image" ] && fail "the dry run created $image — a plan writes nothing but the plan"
[ -e "$TEMPLAROS_PROVISION_MOUNT" ] && fail "the dry run created the missing mount directory"
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

json "import json,sys
ops = json.load(open(sys.argv[1]))['operations']
def pos(pred): return next(i for i,o in enumerate(ops) if pred(o))
mkdir_mount = pos(lambda o: o['argv'][:2] == ['mkdir', '-p'] and o['argv'][-1].endswith('/missing/templaros-provision'))
first_mount = pos(lambda o: o['argv'] and o['argv'][0] == 'mount')
bootstrap = pos(lambda o: o['argv'] and o['argv'][0] == 'mmdebstrap')
host_bind = pos(lambda o: o['argv'][:2] == ['mount', '--bind'])
first_chroot = pos(lambda o: o['argv'] and o['argv'][0] == 'chroot')
assert mkdir_mount < first_mount < bootstrap < host_bind < first_chroot
assert '--skip=check/empty' in ops[bootstrap]['argv']
assert not any(o['argv'][:2] == ['mount', '--bind'] for o in ops[:bootstrap])" "$plan" \
    || fail "mountpoint/bootstrap/bind/chroot lifecycle is out of order"
grep -q 'trap cleanup EXIT' "$provision" && grep -q 'owned_mounts' "$provision" \
    || fail "the provisioner has no owned EXIT cleanup"
ok "a missing mount root is planned first; bootstrap precedes host binds and chroot; cleanup is installed"

fallback_image="$work/fallback.img"
TEMPLAROS_BOOTSTRAP_BACKEND=debootstrap "$provision" --image "$fallback_image" --user plantest \
    --password-stdin > "$work/fallback.txt" 2>&1 \
    || fail "the debootstrap fallback dry run failed"
fallback_plan="$fallback_image.plan.json"
json "import json,sys
ops = json.load(open(sys.argv[1]))['operations']
names = [o['argv'][0] for o in ops if o['argv']]
assert 'debootstrap' in names and 'mmdebstrap' not in names
bootstrap = names.index('debootstrap')
assert not any(o['argv'][:2] == ['mount', '--bind'] for o in ops[:next(i for i,o in enumerate(ops) if o['argv'][0] == 'debootstrap')])" "$fallback_plan" \
    || fail "the debootstrap fallback does not share the clean pre-bootstrap lifecycle"
ok "the debootstrap fallback uses the same clean target lifecycle"

# The plan says what it was reviewed against.
json "import json,sys
plan = json.load(open(sys.argv[1]))
assert 'observed' in plan, 'no observed block'
assert plan['target_kind'] == 'image', plan['target_kind']
assert plan['reuse'] is False and plan['format_data'] is False" "$plan" \
    || fail "the plan does not record what it was reviewed against"
ok "the plan records the state it was reviewed against"

json "import json,sys
request = set(json.load(open(sys.argv[1]))['request'])
assert 'archive_set=test-set' in request
assert any(x.startswith('archive_digest=') and not x.endswith('missing') for x in request)" "$plan" \
    || fail "the plan does not bind the immutable archive set and its digest"

# --- 2. Apply refuses a plan the machine no longer matches ----------------------------------------

# `--apply` as an unprivileged user refuses before it can reach the drift check, so the refusal
# below is checked on the argument path instead: a plan is only for an apply.
if "$provision" --image "$image" --user plantest --password-stdin --plan "$plan" > "$work/planless.txt" 2>&1; then
    fail "a dry run accepted --plan; a plan is consumed by an apply, not produced by one"
fi
grep -q 'plan is for --apply' "$work/planless.txt" \
    || fail "--plan on a dry run was refused for the wrong reason:$(printf '\n'; cat "$work/planless.txt")"

if "$provision" --image "$image" --user plantest --password-stdin --apply --plan "$work/nonexistent.json" \
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
"$provision" --image "$image" --user plantest --password-stdin --apply --plan "$drifted" > "$work/drift.txt" 2>&1 \
    && fail "--apply accepted a plan reviewed against a different machine"
if grep -q 'not what this plan was reviewed against' "$work/drift.txt"; then
ok "apply refuses a plan the machine no longer matches, and prints the difference"
else
    grep -q 'needs root' "$work/drift.txt" \
        && fail "the privilege refusal fired before the drift check; drift would go unnoticed on a real apply"
    fail "a drifted plan was refused for the wrong reason:$(printf '\n'; cat "$work/drift.txt")"
fi


# An unchanged disk observation is not enough: options and source inputs are part of the review.
if "$provision" --image "$image" --user another-user --password-stdin --apply --plan "$plan" \
        > "$work/request-drift.txt" 2>&1; then
    fail "apply accepted different account/options against the reviewed plan"
fi
grep -q 'requested operation is not the one' "$work/request-drift.txt" \
    || fail "request drift was refused for the wrong reason"
ok "apply binds target options, provisioner/layout digests and the immutable archive set"

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
    # preservation logic is exercised through the same code path with TEMPLAROS_PROVISION_FIXTURE.
    esp="$work/esp.img"; rootfs="$work/root.img"; data="$work/data.img"
    truncate -s 512M "$esp"
    truncate -s 2G "$rootfs"
    truncate -s 1G "$data"
    mkfs.vfat -F32 -n TEMPLAR_ESP "$esp" >/dev/null 2>&1 || fail "mkfs.vfat on a file failed"
    mkfs.btrfs -q -f -L templaros-root "$rootfs" >/dev/null 2>&1 || fail "mkfs.btrfs on a file failed"
    mkfs.btrfs -q -f -L templaros-data "$data" >/dev/null 2>&1 || fail "mkfs.btrfs on a file failed"

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

grep -q 'Filesystem identity: /etc/fstab and /etc/templaros/install.toml, both by UUID' "$work/dry.txt" \
    || fail "the plan never writes /etc/fstab — a provisioned machine would not know how to mount itself"
grep -Fq "writing $TEMPLAROS_PROVISION_MOUNT/etc/fstab" "$work/dry.txt" \
    || fail "the plan does not name the fstab it writes"
grep -Fq "writing $TEMPLAROS_PROVISION_MOUNT/etc/templaros/install.toml" "$work/dry.txt" \
    || fail "the plan does not write the installation identity templar-hostd reads"
grep -q 'root=UUID=' "$work/dry.txt" || grep -q 'root=UUID=\${\|read at apply' "$work/dry.txt" \
    || fail "the plan does not say the boot command line will name the root by UUID"
ok "the plan writes fstab and install.toml, and identity is by UUID"

# --- 6. A free block anywhere on the disk, not only the tail ---------------------------------------
#
# The shape a deleted operating system leaves: a large gap at the *front* of the disk with every
# surviving partition above it. This is the reference laptop after Windows was removed, and until
# 2026-09-09 --free-space refused it outright ("a hole between partitions, not the tail"). What
# matters is not only that it is accepted but that the two new partitions stay inside the gap and
# no existing partition moves by a sector.
# 30 GiB sparse, with a ~12 GiB gap at the front: comfortably over the layout's 9 GiB floor
# (ESP 1 GiB + root size_min 8 GiB) so the check is about position, not size.
gap="$work/leading-gap.img"
truncate -s 30G "$gap"
sgdisk -n 1:25000000:25200000 -t 1:0700 -c 1:recovery "$gap" >/dev/null 2>&1
sgdisk -n 2:25200000:60000000 -t 2:8300 -c 2:other-root "$gap" >/dev/null 2>&1
before="$(sgdisk -p "$gap" 2>/dev/null | grep -E '^ +[0-9]+ ')"

gap_plan="$work/gap-plan.json"
"$provision" --free-space "$gap" --user dev --plan-out "$gap_plan" > "$work/gap.txt" 2>&1 \
    || fail "--free-space refused a disk whose largest free block is at the front: $(tail -3 "$work/gap.txt")"
grep -q 'free block: sectors 2048\.\.' "$work/gap.txt" \
    || fail "the plan did not choose the leading gap: $(grep -i 'free block' "$work/gap.txt" || echo 'no free-block line at all')"

# The dry run must have changed nothing; apply it to a copy to check where the partitions land.
after_dry="$(sgdisk -p "$gap" 2>/dev/null | grep -E '^ +[0-9]+ ')"
[ "$before" = "$after_dry" ] || fail "the dry run altered the partition table"

python3 - "$gap_plan" "$gap" <<'PY'
import json, subprocess, sys
plan = json.load(open(sys.argv[1]))
target = sys.argv[2]
for op in plan["operations"]:
    argv = op["argv"]
    if argv and argv[0] == "sgdisk" and argv[-1] == target and any(a.startswith("--new=") for a in argv):
        subprocess.run(argv, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
PY
after="$(sgdisk -p "$gap" 2>/dev/null | grep -E '^ +[12] ')"
[ "$(printf '%s\n' "$before" | grep -E '^ +[12] ')" = "$after" ] \
    || fail "creating the two partitions in the gap moved an existing one"
root_end="$(sgdisk -i 4 "$gap" 2>/dev/null | sed -n 's/^Last sector: \([0-9]*\).*/\1/p')"
[ "$root_end" -lt 25000000 ] \
    || fail "the root partition ran past the end of the gap into partition 1 (ends at $root_end, the gap ends at 24999999)"
ok "--free-space uses a gap at the front of the disk and stays inside it"

echo "provision-plan: OK"
