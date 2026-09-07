#!/usr/bin/env bash
# Build the LegeOS installation medium: a UEFI-only live ISO that boots the LegeOS session through
# Canonical's signed shim and GRUB and carries one build set plus the provisioner (ISO-PLAN.md §3).
#
#   rootfs  mmdebstrap (unshare mode, no root) from the seeds + one build set
#   live    casper; the customize hook writes the live-only autologin and copies the boot files out
#   medium  mksquashfs -tar → casper/filesystem.squashfs, xorriso → legeos-<set>.iso
#
# Every step is skipped when its output already exists (delete it to redo that step), so a failed
# xorriso does not cost another bootstrap. Needs: mmdebstrap, mksquashfs, xorriso, mtools,
# dosfstools, and a user namespace (Ubuntu ships an AppArmor profile for mmdebstrap).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

die() { printf 'iso: %s\n' "$*" >&2; exit 1; }
usage() {
    cat <<'USAGE'
usage: distro/iso/build.sh [--set ID] [--archive DIR] [--work DIR] [--out DIR] [--mode MODE]

  --set ID        the build set to carry (default: the archive's `current` marker)
  --archive DIR   the LegeOS archive (default: /var/lib/legeos/devrepo)
  --work DIR      where the rootfs, squashfs and ISO tree are built
                  (default: $LEGEOS_BUILD_DIR, else /var/tmp/legeos-build/@build). Not NTFS.
  --out DIR       where the .iso and its checksum land (default: the work directory)
  --mode MODE     mmdebstrap mode (default: unshare; root for a privileged build)
USAGE
}

archive="${LEGEOS_DEVREPO:-/var/lib/legeos/devrepo}"
set_id="${LEGEOS_BUILD_SET:-}"
work="${LEGEOS_BUILD_DIR:-/var/tmp/legeos-build/@build}"
out=""
mode=unshare
while [ $# -gt 0 ]; do
    case "$1" in
        --set) set_id="${2:?}"; shift 2 ;;
        --archive) archive="${2:?}"; shift 2 ;;
        --work) work="${2:?}"; shift 2 ;;
        --out) out="${2:?}"; shift 2 ;;
        --mode) mode="${2:?}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument $1" ;;
    esac
done
out="${out:-$work}"

for tool in mmdebstrap mksquashfs xorriso mmd mcopy mkfs.vfat; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed"
done
[ -n "$set_id" ] || { [ -r "$archive/current" ] && set_id="$(cat "$archive/current")"; }
[ -n "$set_id" ] || die "no build set: publish one (just deb; lege dev repo publish) or pass --set"
[ -d "$archive/sets/$set_id" ] || die "the archive at $archive holds no set $set_id"
[ -f "$archive/legeos-dev.gpg" ] || die "$archive has no public key (legeos-dev.gpg)"
mkdir -p "$work" "$out" 2>/dev/null || die "cannot create $work — mount the build volume first (ISO-PLAN.md P0.2)"
case "$(stat -f -c %T "$work")" in
    fuseblk|ntfs*) die "$work is on NTFS, which cannot hold a rootfs; use a btrfs/ext4 build volume" ;;
esac

# --- Inputs from the one declaration -------------------------------------------------------------
base="$root/distro/base.toml"
base_value() { sed -n "s/^$1 *= *\"\([^\"]*\)\".*/\1/p" "$base" | head -1; }
codename="$(base_value codename)"
mirror="$(base_value snapshot_url)"
[ -n "$mirror" ] || mirror="$(base_value mirror)"
security="$(base_value security_mirror)"
components="main,universe,restricted"
# What goes in: the metapackages (they pull the whole seed selection at the set's version) and the
# live layer, which exists only here. `cargo xtask seeds` keeps live.seed current.
( cd "$root" && cargo run -q -p xtask -- seeds >/dev/null )
# No legeos-nvidia-open on the medium. The live session runs on whatever KMS driver the kernel
# binds (the Intel iGPU on the reference laptop), and the NVIDIA LRM is installed into the *target*
# by the provisioner, where there is a real /boot and a configured initramfs. In an unshared
# bootstrap chroot nvidia-kernel-common's postinst runs `update-initramfs` for the *host's* kernel
# before initramfs-tools has been configured, which fails the whole transaction (2026-09-07).
includes="legeos-desktop,legeos-kernel,legeos-dev-rust"
for seed in live hardware; do
    includes="$includes,$(grep -v '^#' "$root/distro/seeds/$seed.seed" | grep -v '^$' | paste -sd,)"
done
# hardware.seed is listed as well as legeos-desktop on purpose: the metapackage inside a published
# set was rendered when that set was built, so a firmware or module package added to the seeds
# after it would not reach the medium until the next publish. The declaration wins on the medium.

tree="$work/iso"
squashfs="$tree/casper/filesystem.squashfs"
iso="$out/legeos-$set_id.iso"
mkdir -p "$tree/casper" "$tree/.disk" "$tree/boot/grub" "$tree/legeos" "$tree/efi"
# The customize hook runs inside mmdebstrap's user namespace, where this user's uid is not mapped:
# from in there these directories belong to nobody and even the namespace's root cannot write to
# them, so the hook cannot drop the kernel, the initramfs and the signed boot files here. A
# world-writable mode is what lets it; this is a scratch build tree, never a shipped path.
chmod 0777 "$tree" "$tree/casper" "$tree/efi" "$tree/legeos"

printf 'LegeOS live medium\n  set      %s\n  archive  %s\n  mirror   %s\n  work     %s\n  out      %s\n' \
    "$set_id" "$archive" "$mirror" "$work" "$iso"

# --- 1. rootfs → squashfs --------------------------------------------------------------------------
if [ -s "$squashfs" ]; then
    echo "1. $squashfs exists; skipping the bootstrap"
else
    echo "1. mmdebstrap $codename ($mode) → $squashfs"
    # Pin the set so a pool holding three sets yields this one, not the newest.
    pref="$work/legeos-set.pref"
    printf 'Package: legeos-* lege-files\nPin: version %s\nPin-Priority: 1001\n' "$set_id" > "$pref"
    rm -f "$squashfs"
    LEGEOS_ISO_OUT="$tree" LEGEOS_ISO_SET="$set_id" \
    mmdebstrap --mode="$mode" --variant=minbase --architectures=amd64 --components="$components" \
        --include="$includes" \
        --aptopt='Apt::Install-Recommends "true"' \
        --keyring="$archive/legeos-dev.gpg" \
        --setup-hook="upload $pref /etc/apt/preferences.d/legeos-set" \
        --customize-hook="upload $root/distro/branding/os-release /usr/lib/os-release.legeos" \
        --customize-hook="upload $root/distro/image/mkosi.postinst /tmp/legeos-postinst" \
        --customize-hook='chroot "$1" sh /tmp/legeos-postinst; rm -f "$1/tmp/legeos-postinst"' \
        --customize-hook="$here/live/customize.sh" \
        "$codename" - \
        "deb $mirror $codename main universe restricted" \
        "deb $mirror $codename-updates main universe restricted" \
        "deb $security $codename-security main universe restricted" \
        "deb copy://$archive ./" \
        | mksquashfs - "$squashfs" -tar -comp zstd -Xcompression-level 15 -noappend -quiet \
        || { rm -f "$squashfs"; die "the bootstrap failed (see above); the partial squashfs was removed"; }
    [ -s "$squashfs" ] || die "no squashfs was produced"
fi
for f in casper/vmlinuz casper/initrd efi/BOOTX64.EFI efi/grubx64.efi efi/mmx64.efi casper/filesystem.manifest; do
    [ -s "$tree/$f" ] || die "the customize hook left no $f; delete $squashfs and rebuild"
done

# --- 2. The set, and what tier each package is -----------------------------------------------------
echo "2. archive → legeos/devrepo, tiers manifest"
rm -rf "$tree/legeos/devrepo"
mkdir -p "$tree/legeos/devrepo"
# Everything but the signing key: a private key has no place on a medium.
for entry in "$archive"/*; do
    [ "$(basename "$entry")" = gnupg ] && continue
    cp -a "$entry" "$tree/legeos/devrepo/"
done
native="$(sed -n 's/^Package: //p' "$archive/Packages" | sort -u)"
{
    echo "# package version tier — native: built by this repository; inherited: Ubuntu $codename as is."
    echo "# set $set_id"
    while read -r pkg ver; do
        tier=inherited
        printf '%s\n' "$native" | grep -qx "$pkg" && tier=native
        printf '%s %s %s\n' "$pkg" "$ver" "$tier"
    done < "$tree/casper/filesystem.manifest"
} > "$tree/legeos/TIERS.manifest"
printf 'native %s, inherited %s\n' "$(grep -c ' native$' "$tree/legeos/TIERS.manifest")" \
    "$(grep -c ' inherited$' "$tree/legeos/TIERS.manifest")"

# --- 3. Boot files -------------------------------------------------------------------------------
echo "3. GRUB configuration and the EFI system partition image"
printf 'LegeOS %s amd64 (%s)\n' "$set_id" "$(date -u +%Y-%m-%d)" > "$tree/.disk/info"
# console=ttyS0 keeps the QEMU harness able to read the boot; it costs nothing on a laptop.
cat > "$tree/boot/grub/grub.cfg" <<CFG
set default=0
set timeout=8
menuentry "LegeOS $set_id — live session" {
    linux /casper/vmlinuz boot=casper console=ttyS0,115200 console=tty0 quiet ---
    initrd /casper/initrd
}
menuentry "LegeOS $set_id — live session, loaded into RAM" {
    linux /casper/vmlinuz boot=casper toram console=ttyS0,115200 console=tty0 quiet ---
    initrd /casper/initrd
}
# The two rescue entries. Neither is a variant of the first for its own sake: each answers one of
# the two ways a machine fails to reach a session.
#
# Safe graphics does NOT use nomodeset. nomodeset turns off kernel modesetting for every driver,
# including the Intel one the compositor renders on, so the session would come up on a blank
# screen — the very symptom being escaped. What actually goes wrong on a hybrid laptop is the
# NVIDIA stack, so this entry removes exactly that and leaves i915/amdgpu modesetting alone.
menuentry "LegeOS $set_id — safe graphics (no NVIDIA)" {
    linux /casper/vmlinuz boot=casper modprobe.blacklist=nvidia,nvidia_drm,nvidia_modeset,nvidia_uvm,nouveau console=ttyS0,115200 console=tty0 ---
    initrd /casper/initrd
}
# No session at all: multi-user.target stops before greetd, so casper's autologin lands on a text
# console with the preflight report already printed. This is the entry to boot when the graphical
# one shows nothing, and it is where nomodeset belongs, since nothing here needs modesetting.
menuentry "LegeOS $set_id — text console (no session)" {
    linux /casper/vmlinuz boot=casper systemd.unit=multi-user.target nomodeset console=ttyS0,115200 console=tty0 ---
    initrd /casper/initrd
}
CFG
# The signed removable-media GRUB (gcdx64) reads EFI/BOOT/grub.cfg from the partition it was
# loaded from and is then pointed at the ISO's own configuration.
cat > "$work/efi-grub.cfg" <<CFG
search --set=root --file /.disk/info
set prefix=(\$root)/boot/grub
configfile \$prefix/grub.cfg
CFG
efi_img="$tree/boot/grub/efi.img"
rm -f "$efi_img"
mkfs.vfat -C "$efi_img" 8192 >/dev/null
mmd -i "$efi_img" ::/EFI ::/EFI/BOOT
mcopy -i "$efi_img" "$tree/efi/BOOTX64.EFI" "$tree/efi/grubx64.efi" "$tree/efi/mmx64.efi" ::/EFI/BOOT/
mcopy -i "$efi_img" "$work/efi-grub.cfg" ::/EFI/BOOT/grub.cfg
# The same files on the ISO9660 side, for firmware that reads it as a filesystem.
mkdir -p "$tree/EFI/BOOT"
cp "$tree/efi/BOOTX64.EFI" "$tree/efi/grubx64.efi" "$tree/efi/mmx64.efi" "$tree/EFI/BOOT/"
cp "$work/efi-grub.cfg" "$tree/EFI/BOOT/grub.cfg"
rm -rf "$tree/efi"

# --- 4. The ISO ----------------------------------------------------------------------------------
echo "4. xorriso → $iso"
rm -f "$iso"
# UEFI only (ADR-0004: no legacy BIOS path). El Torito for optical media; the appended GPT
# partition is what a USB stick's firmware boots.
xorriso -as mkisofs -r -V LEGEOS_LIVE -J -joliet-long -l -iso-level 3 \
    -o "$iso" -partition_offset 16 \
    -append_partition 2 0xef "$efi_img" -appended_part_as_gpt \
    -eltorito-alt-boot -e --interval:appended_partition_2:all:: -no-emul-boot \
    "$tree" 2>&1 | grep -v '^xorriso : UPDATE' | tail -3
[ -s "$iso" ] || die "xorriso produced no ISO"
( cd "$out" && sha256sum "$(basename "$iso")" > "$(basename "$iso").sha256" )
cp "$tree/legeos/TIERS.manifest" "$out/legeos-$set_id.TIERS.manifest"
printf '\n%s\n%s\n' "$iso" "$(cat "$iso.sha256")"
