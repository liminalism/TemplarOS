#!/usr/bin/env bash
# Provisioning smoke: the declared layout, the provisioner's plan and its refusals, and — where the
# host can actually do it — a boot of a disposable image under QEMU.
#
# Three things are asserted here, in decreasing order of what a workstation can honestly check:
#
#   1. the plan `installer/provision.sh` prints is the layout `distro/deploy/layout.toml` declares,
#      and printing it touches nothing (always runs);
#   2. the destructive paths refuse the targets they are supposed to refuse (always runs);
#   3. a disposable image boots under QEMU and reports its deployment id (runs only with qemu, an
#      OVMF firmware and an image to boot).
#
# Step 3 is the one that needs equipment. When something is missing this script says
# `NOT EXECUTED: <reason>` and exits successfully — a check that did not run is not a check that
# passed, and neither is it a failure of the repository. It never provisions a real disk, never
# writes to a device, and never runs anything as root.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
provision="$root/installer/provision.sh"
layout="$root/distro/deploy/layout.toml"

work="$(mktemp -d /tmp/legeos-provision-qemu.XXXXXX)"
cleanup() {
    case "$work" in
        /tmp/legeos-provision-qemu.*) rm -rf -- "$work" ;;
        *) echo "refusing to remove unexpected work directory: $work" >&2 ;;
    esac
}
trap cleanup EXIT

fail() { echo "provision-qemu: FAILED — $*" >&2; exit 1; }
not_executed() { echo "provision-qemu: NOT EXECUTED: $*"; }

[ -x "$provision" ] || fail "installer/provision.sh is missing or not executable"
[ -f "$layout" ] || fail "distro/deploy/layout.toml is missing"

# --- 1. The plan is the declaration ---------------------------------------------------------------

image="$work/disposable.img"
plan="$work/plan.txt"
"$provision" --image "$image" --user provisiontest > "$plan" 2>&1 \
    || fail "the dry run exited non-zero:\n$(cat "$plan")"

[ -e "$image" ] && fail "the dry run created $image — it must write nothing"

subvolumes="$(awk '/^subvolumes/ { sub(/^subvolumes *= */, ""); gsub(/[][",]/, ""); print }' "$layout")"
[ -n "$subvolumes" ] || fail "layout.toml declares no subvolumes"
for subvol in $subvolumes; do
    grep -q "btrfs subvolume create .*${subvol#/}\$" "$plan" \
        || fail "the plan never creates the declared subvolume $subvol"
done
# The one the update engine stages candidates into, named explicitly so a silent removal from the
# declaration cannot quietly pass this test.
grep -q '@deployments' "$plan" || fail "the plan does not create @deployments"

grep -q 'legeos.deployment=install-' "$plan" \
    || fail "the seeded deployment's command line carries no legeos.deployment= id"
grep -q "GRUB_DEFAULT=saved" "$plan" || fail "the plan does not set GRUB_DEFAULT=saved"
grep -q "09_legeos" "$plan" || fail "the plan does not install the /etc/grub.d stub"
grep -q "shim-signed grub-efi-amd64-signed" "$plan" \
    || fail "the plan does not install the Canonical-signed shim and GRUB (ADR-0004)"
grep -q "repo verify" "$plan" \
    || fail "the plan installs from the local archive without verifying the set"
grep -q "cp -a .*devrepo /.*var/lib/legeos/devrepo" "$plan" \
    || fail "the plan does not hand the archive over to the target"
grep -q "grub-install .*--no-nvram" "$plan" \
    || fail "grub-install would write this host's firmware variables from inside the chroot"
for package in legeos-archive-keyring legeos-desktop legeos-kernel; do
    grep -q "$package" "$plan" || fail "the plan never installs $package"
done
# No fixed password, no autologin, root locked: the three things the dev image has and a
# provisioned machine must not.
grep -qi 'passwd --expire provisiontest' "$plan" \
    || fail "the account is not created with an expired password"
grep -q 'passwd --lock root' "$plan" || fail "the plan does not lock the root account"
grep -qiE 'RootPassword|Autologin|password=' "$plan" \
    && fail "the plan mentions a baked-in credential"
echo "provision-qemu: plan matches the layout declaration and bakes in no credentials"

# --- 1b. Free space on a disk shaped like the reference laptop ------------------------------------
# A 16 MiB reserved partition, a large NTFS-typed one, a 100 MiB ESP, then unallocated space: the
# plan must create exactly two partitions after them and name every one it keeps.

if command -v sgdisk >/dev/null 2>&1; then
    fixture="$work/laptop.img"
    truncate -s 20G "$fixture"
    sgdisk --zap-all "$fixture" >/dev/null
    sgdisk --new=1:0:+16M --typecode=1:0c01 --new=2:0:+6G --typecode=2:0700 \
        --new=3:0:+100M --typecode=3:ef00 "$fixture" >/dev/null
    before="$(sgdisk -p "$fixture")"
    free_plan="$work/free-plan.txt"
    "$provision" --free-space "$fixture" --user provisiontest > "$free_plan" 2>&1 \
        || fail "the free-space dry run exited non-zero:\n$(cat "$free_plan")"
    [ "$before" = "$(sgdisk -p "$fixture")" ] || fail "the free-space dry run changed the partition table"
    grep -q 'sgdisk --new=4:0:+1G --typecode=4:ef00' "$free_plan" || fail "the ESP is not partition 4"
    grep -q 'sgdisk --new=5:0:0 --typecode=5:8304' "$free_plan" || fail "the root is not partition 5"
    grep -q 'sgdisk --zap-all' "$free_plan" && fail "the free-space plan zaps the disk"
    for kept in 1 2 3; do
        grep -q "partition $kept (sectors" "$free_plan" || fail "the plan does not name kept partition $kept"
    done
    grep -q 'mkfs.btrfs -f -L legeos-root .*p5' "$free_plan" || fail "the root filesystem is not made on partition 5"
    grep -q '\$ efibootmgr' "$free_plan" && fail "a file target must not touch the firmware"
    # A hole between partitions is not a tail: fill the tail and leave a gap in the middle.
    holed="$work/holed.img"
    truncate -s 20G "$holed"
    sgdisk --zap-all "$holed" >/dev/null
    sgdisk --new=1:0:+1G --typecode=1:ef00 --new=2:12G:0 --typecode=2:8300 "$holed" >/dev/null
    if "$provision" --free-space "$holed" --user t >"$work/holed.txt" 2>&1; then
        fail "free space in the middle of a disk was accepted as a tail"
    fi
    grep -q 'not the tail' "$work/holed.txt" || fail "the hole was refused for the wrong reason"
    echo "provision-qemu: the free-space plan adds two partitions and keeps three"
else
    not_executed "sgdisk is not installed; the free-space plan was not checked"
fi

# --- 2. The refusals ------------------------------------------------------------------------------

refuses() { # description, args...
    local description="$1"; shift
    if "$provision" "$@" >"$work/refusal.txt" 2>&1; then
        fail "provision.sh accepted $description"
    fi
    grep -qi 'provision:' "$work/refusal.txt" || fail "$description was refused without saying why"
}
refuses "a device path passed as --image" --image /dev/null --user t
refuses "--device without --i-mean-it" --device /dev/null --user t
refuses "--device --i-mean-it without the environment switch" --device /dev/null --i-mean-it --user t
refuses "no target at all" --user t
refuses "--esp without --root" --esp /dev/null --user t
refuses "--free-space on a missing disk" --free-space "$work/does-not-exist" --user t
echo "provision-qemu: destructive targets are refused as designed"

# `--apply` as a non-root user must refuse rather than start and fail halfway.
if "$provision" --image "$image" --user t --apply >"$work/apply.txt" 2>&1; then
    fail "provision.sh --apply ran as an unprivileged user"
fi
grep -q 'needs root' "$work/apply.txt" || fail "--apply as non-root failed for the wrong reason"
[ -e "$image" ] && fail "the refused --apply still created $image"
echo "provision-qemu: --apply refuses to start without privileges"

# --- 3. Boot a disposable image under QEMU --------------------------------------------------------

# Provisioning an image for real needs root (loop devices, mkfs, mount, chroot), which this script
# will not take. So step 3 boots an image somebody else built — `just image`, or one passed in.
bootable="${LEGE_PROVISION_IMAGE:-}"
if [ -z "$bootable" ]; then
    bootable="$(find "$root/distro/image/mkosi.output" -maxdepth 1 -name '*.raw' -print -quit 2>/dev/null || true)"
fi
firmware="${LEGE_OVMF:-}"
if [ -z "$firmware" ]; then
    for candidate in /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
                     /usr/share/edk2/x64/OVMF_CODE.4m.fd /usr/share/qemu/OVMF.fd; do
        [ -f "$candidate" ] && { firmware="$candidate"; break; }
    done
fi

if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    not_executed "qemu-system-x86_64 is not installed"
elif [ -z "$bootable" ]; then
    not_executed "no image to boot (build one with 'just image', or set LEGE_PROVISION_IMAGE)"
elif [ -z "$firmware" ]; then
    not_executed "no OVMF firmware found (set LEGE_OVMF); a BIOS boot is not the LegeOS boot path"
else
    echo "provision-qemu: booting $bootable under QEMU (read-only snapshot, 120s)"
    serial="$work/serial.log"
    # -snapshot: every write goes to a temporary overlay, so the image on disk is never modified.
    timeout 120 qemu-system-x86_64 \
        -machine q35,accel=tcg -m 2048 -nographic -snapshot \
        -drive "if=pflash,format=raw,readonly=on,file=$firmware" \
        -drive "file=$bootable,format=raw,if=virtio" \
        -serial "file:$serial" -display none >/dev/null 2>&1 || true
    if [ ! -s "$serial" ]; then
        fail "QEMU produced no serial output at all from $bootable"
    fi
    if grep -qi 'legeos.deployment=' "$serial"; then
        echo "provision-qemu: the booted kernel command line carries a deployment id"
    else
        # The mkosi image boots systemd-boot with no LegeOS deployment staged, so its first boot
        # legitimately has no id on the command line. Say so rather than calling it a pass.
        not_executed "the image booted but staged no LegeOS deployment, so no legeos.deployment= id was on its command line"
    fi
    echo "provision-qemu: QEMU boot completed (serial log kept at $serial until cleanup)"
fi

echo "provision-qemu: OK"
