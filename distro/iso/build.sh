#!/usr/bin/env bash
# Build the TemplarOS installation medium: a UEFI-only live ISO that boots the TemplarOS session through
# Canonical's signed shim and GRUB and carries one build set plus the provisioner (ISO-PLAN.md §3).
#
#   rootfs  mmdebstrap (unshare mode, no root) from the seeds + one build set, or a native
#           refresh onto a previous live squashfs when the Ubuntu snapshot/seeds are unchanged
#   live    casper; the customize hook writes the live-only autologin and copies the boot files out
#   medium  mksquashfs -tar → casper/filesystem.squashfs, xorriso → templaros-<set>.iso
#   cache   $work/apt-cache/<snapshot>/archives — mmdebstrap sync-in/out, so a later full
#           bootstrap does not re-fetch resolute; $work/ubuntu-layer/<ubuntu_key>/ holds the last
#           matching live squashfs for native-only refreshes
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
                           [--no-layer-reuse]

  --set ID        the build set to carry (default: the archive's `current` marker)
  --archive DIR   the TemplarOS archive (default: /var/lib/templaros/devrepo)
  --work DIR      where the rootfs, squashfs and ISO tree are built
                  (default: $TEMPLAROS_BUILD_DIR, else /var/tmp/templaros-build/@build). Not NTFS.
  --out DIR       where the .iso and its checksum land (default: the work directory)
  --mode MODE     mmdebstrap mode (default: unshare; root for a privileged build)
  --no-layer-reuse  always mmdebstrap; do not refresh natives onto a previous live rootfs
USAGE
}

archive="${TEMPLAROS_DEVREPO:-/var/lib/templaros/devrepo}"
set_id="${TEMPLAROS_BUILD_SET:-}"
work="${TEMPLAROS_BUILD_DIR:-/var/tmp/templaros-build/@build}"
out=""
mode=unshare
no_layer_reuse=0
while [ $# -gt 0 ]; do
    case "$1" in
        --set) set_id="${2:?}"; shift 2 ;;
        --archive) archive="${2:?}"; shift 2 ;;
        --work) work="${2:?}"; shift 2 ;;
        --out) out="${2:?}"; shift 2 ;;
        --mode) mode="${2:?}"; shift 2 ;;
        --no-layer-reuse) no_layer_reuse=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument $1" ;;
    esac
done
out="${out:-$work}"

for tool in mmdebstrap mksquashfs unsquashfs xorriso mmd mcopy mkfs.vfat; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed"
done
[ -n "$set_id" ] || { [ -r "$archive/current" ] && set_id="$(cat "$archive/current")"; }
[ -n "$set_id" ] || die "no build set: publish one (just deb; templar dev repo publish) or pass --set"
[ -d "$archive/sets/$set_id" ] || die "the archive at $archive holds no set $set_id"
[ -f "$archive/templaros-dev.gpg" ] || die "$archive has no public key (templaros-dev.gpg)"
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
# No templaros-nvidia-open on the medium. The live session runs on whatever KMS driver the kernel
# binds (the Intel iGPU on the reference laptop), and the NVIDIA LRM is installed into the *target*
# by the provisioner, where there is a real /boot and a configured initramfs. In an unshared
# bootstrap chroot nvidia-kernel-common's postinst runs `update-initramfs` for the *host's* kernel
# before initramfs-tools has been configured, which fails the whole transaction (2026-09-07).
includes="templaros-desktop,templaros-kernel,templaros-dev-rust"
for seed in live hardware; do
    includes="$includes,$(grep -v '^#' "$root/distro/seeds/$seed.seed" | grep -v '^$' | paste -sd,)"
done
# hardware.seed is listed as well as templaros-desktop on purpose: the metapackage inside a published
# set was rendered when that set was built, so a firmware or module package added to the seeds
# after it would not reach the medium until the next publish. The declaration wins on the medium.

tree="$work/iso"
squashfs="$tree/casper/filesystem.squashfs"
cache_stamp="$tree/casper/filesystem.squashfs.inputs.sha256"
iso="$out/templaros-$set_id.iso"
mkdir -p "$tree/casper" "$tree/.disk" "$tree/boot/grub" "$tree/templaros" "$tree/efi"
# The customize hook runs inside mmdebstrap's user namespace, where this user's uid is not mapped:
# from in there these directories belong to nobody and even the namespace's root cannot write to
# them, so the hook cannot drop the kernel, the initramfs and the signed boot files here. A
# world-writable mode is what lets it; this is a scratch build tree, never a shipped path.
chmod 0777 "$tree" "$tree/casper" "$tree/efi" "$tree/templaros"

printf 'TemplarOS live medium\n  set      %s\n  archive  %s\n  mirror   %s\n  work     %s\n  out      %s\n' \
    "$set_id" "$archive" "$mirror" "$work" "$iso"

# Two cache keys. ubuntu_key is the snapshot, seeds and include *names* — the 1.2 GB resolute
# fetch. cache_key also covers the native set, provisioner and live customize. A code-only
# change must not re-download Ubuntu; it refreshes natives onto a matching layer. Relabelling
# an old squashfs as a new set is still refused: the provisioner SHA check below is that gate.
ubuntu_key="$({
    printf '%s\n' "$codename" "$mirror" "$security" "$components" "$includes" "$mode"
    for input in "$base" "$root/distro/seeds/live.seed" "$root/distro/seeds/hardware.seed" \
                 "$root/distro/image/mkosi.postinst"; do
        [ -f "$input" ] && sha256sum "$input"
    done
    mmdebstrap --version 2>/dev/null | head -1
    mksquashfs -version 2>/dev/null | head -1
} | sha256sum | awk '{print $1}')"
cache_key="$({
    printf '%s\n' "$ubuntu_key" "$set_id"
    for input in "$archive/sets/$set_id/manifest.json" "$archive/Packages" "$archive/Release" \
                 "$archive/InRelease" "$root/installer/provision.sh" \
                 "$here/live/customize.sh" "$root/distro/deploy/layout.toml"; do
        [ -f "$input" ] && sha256sum "$input"
    done
} | sha256sum | awk '{print $1}')"

serial="$(printf '%s' "$mirror" | sed 's|.*/||')"
[ -n "$serial" ] || serial=default
apt_archives="$work/apt-cache/$serial/archives"
mkdir -p "$apt_archives/partial"
chmod 0777 "$apt_archives" "$apt_archives/partial" 2>/dev/null || true
layer_dir="$work/ubuntu-layer/$ubuntu_key"
layer_sq="$layer_dir/filesystem.squashfs"
mkdir -p "$layer_dir" "$work/tmp"

find_prev_iso() {
    local f best="" best_m=0 m
    for f in "$work"/templaros-*.iso "$out"/templaros-*.iso; do
        [ -f "$f" ] || continue
        [ "$f" = "$iso" ] && continue
        m=$(stat -c %Y "$f")
        if [ "$m" -gt "$best_m" ]; then
            best_m=$m
            best=$f
        fi
    done
    printf '%s' "$best"
}

extract_layer_from_iso() {
    local prev="$1"
    echo "1. extracting Ubuntu layer from $prev (no snapshot fetch)"
    rm -f "$layer_sq"
    xorriso -osirrox on -indev "$prev" -extract /casper/filesystem.squashfs "$layer_sq"
    [ -s "$layer_sq" ] || die "failed to extract filesystem.squashfs from $prev"
    printf '%s\n' "$ubuntu_key" > "$layer_dir/key"
}

prepare_iso_tree_for_hook() {
    rm -f "$tree"/casper/vmlinuz "$tree"/casper/initrd "$tree"/casper/filesystem.manifest \
        "$tree"/efi/* "$tree"/templaros/kernel
    chmod 0777 "$tree" "$tree/casper" "$tree/efi" "$tree/templaros"
}

refresh_native() {
    echo "1. refreshing native set $set_id onto the Ubuntu layer (no snapshot fetch)"
    local refresh_root="$work/tmp/refresh-rootfs" squashfs_tmp="$squashfs.tmp"
    local debs="$archive/sets/$set_id/debs"
    local staged="$work/tmp/refresh-debs"
    [ -d "$debs" ] || die "no native debs at $debs"
    [ -s "$layer_sq" ] || die "no Ubuntu layer at $layer_sq"
    prepare_iso_tree_for_hook
    chmod 0777 "$work/tmp"
    rm -f "$squashfs_tmp"
    # Live medium omits NVIDIA/CUDA metapackages (build.sh --include). Stage only the
    # packages that belong on it, plus Ubuntu shlibdeps the new natives declared
    # (libseat1 lives in the resolute buildroot's apt cache — no snapshot fetch).
    rm -rf "$staged"
    mkdir -p "$staged"
    local f pkg
    for f in "$debs"/*.deb; do
        pkg=$(dpkg-deb -f "$f" Package)
        case "$pkg" in
            templaros-nvidia-*|templaros-dev-cuda|templaros-session-full) continue ;;
        esac
        cp "$f" "$staged/"
    done
    local seat
    seat="$(ls "$work/buildroot/var/cache/apt/archives"/libseat1_*.deb 2>/dev/null | head -1 || true)"
    [ -n "$seat" ] || die "no libseat1 .deb in the resolute buildroot cache"
    cp "$seat" "$staged/"
    cp "$seat" "$apt_archives/"
    # The unsquashed tree is owned by the namespace uid; only that uid can delete it.
    cleanup_refresh_root() {
        mmdebstrap --unshare-helper /bin/sh -c 'rm -rf "$1"' -- "$refresh_root" >/dev/null || true
    }
    if [ -x "$refresh_root/usr/bin/dpkg" ]; then
        echo "1. reusing unsquashed rootfs at $refresh_root"
    else
        cleanup_refresh_root
    fi
    mmdebstrap --unshare-helper /bin/sh -c '
        set -eu
        src=$1 dest=$2 rootfs=$3 debs=$4 postinst=$5 customize=$6 osrelease=$7 out=$8 set_id=$9
        export DEBIAN_FRONTEND=noninteractive TEMPLAROS_SKIP_FLATPAK=1
        export TEMPLAROS_ISO_OUT="$out" TEMPLAROS_ISO_SET="$set_id"
        if [ ! -x "$rootfs/usr/bin/dpkg" ]; then
            mkdir -p "$rootfs"
            # User namespaces cannot mknod; /dev is bind-mounted from the host afterwards.
            unsquashfs -f -ignore-errors -d "$rootfs" "$src" || [ -x "$rootfs/usr/bin/dpkg" ]
        fi
        # usr-merge symlinks: unsquashfs in a user namespace often drops /bin /lib /sbin.
        [ -e "$rootfs/bin" ] || ln -s usr/bin "$rootfs/bin"
        [ -e "$rootfs/lib" ] || ln -s usr/lib "$rootfs/lib"
        [ -e "$rootfs/lib64" ] || ln -s usr/lib64 "$rootfs/lib64"
        [ -e "$rootfs/sbin" ] || ln -s usr/sbin "$rootfs/sbin"
        chown 0:0 "$rootfs"
        chmod 0755 "$rootfs"
        mkdir -p "$rootfs/tmp/native" "$rootfs/proc" "$rootfs/dev" "$rootfs/sys" \
            "$rootfs/home" "$rootfs/media" "$rootfs/mnt" "$rootfs/opt" "$rootfs/srv"
        mount -t proc proc "$rootfs/proc"
        mount --rbind /dev "$rootfs/dev" 2>/dev/null || true
        mount --rbind /sys "$rootfs/sys" 2>/dev/null || true
        chroot "$rootfs" dpkg --purge templaros-nvidia-open templaros-dev-cuda \
            templaros-session-full 2>/dev/null || true
        rm -rf "$rootfs/tmp/native"
        mkdir -p "$rootfs/tmp/native"
        cp -a "$debs"/*.deb "$rootfs/tmp/native/"
        chroot "$rootfs" sh -c "dpkg -i /tmp/native/*.deb" || true
        chroot "$rootfs" dpkg --configure -a || true
        chroot "$rootfs" dpkg -s templaros-comp | grep -q "Status: install ok installed"
        rm -rf "$rootfs/tmp/native"
        cp "$osrelease" "$rootfs/usr/lib/os-release.templaros"
        cp "$postinst" "$rootfs/tmp/templaros-postinst"
        chroot "$rootfs" sh /tmp/templaros-postinst
        rm -f "$rootfs/tmp/templaros-postinst"
        /bin/sh "$customize" "$rootfs"
        umount "$rootfs/proc" 2>/dev/null || true
        umount -l "$rootfs/dev" 2>/dev/null || true
        umount -l "$rootfs/sys" 2>/dev/null || true
        tar -C "$rootfs" --one-file-system -cf - . \
            | mksquashfs - "$dest" -tar -comp zstd -Xcompression-level 15 -noappend -quiet
    ' -- "$layer_sq" "$squashfs_tmp" "$refresh_root" "$staged" \
        "$root/distro/image/mkosi.postinst" "$here/live/customize.sh" \
        "$root/distro/branding/os-release" "$tree" "$set_id" \
        || { rm -f "$squashfs_tmp"; die "native refresh failed"; }
    mv "$squashfs_tmp" "$squashfs"
    cleanup_refresh_root
    printf '%s\n' "$cache_key" > "$cache_stamp.tmp"
    mv "$cache_stamp.tmp" "$cache_stamp"
    [ -s "$squashfs" ] || die "no squashfs was produced"
}

save_ubuntu_layer() {
    cp --reflink=auto -a "$squashfs" "$layer_sq"
    printf '%s\n' "$ubuntu_key" > "$layer_dir/key"
}

# --- 1. rootfs → squashfs --------------------------------------------------------------------------
if [ -s "$squashfs" ] && [ "$(cat "$cache_stamp" 2>/dev/null || true)" = "$cache_key" ]; then
    echo "1. $squashfs exists; skipping the bootstrap"
elif [ "$no_layer_reuse" != 1 ] && [ -s "$layer_sq" ]; then
    [ ! -s "$squashfs" ] || echo "1. cached squashfs inputs changed; refreshing natives"
    refresh_native
elif [ "$no_layer_reuse" != 1 ] && prev_iso="$(find_prev_iso)" && [ -n "$prev_iso" ]; then
    extract_layer_from_iso "$prev_iso"
    refresh_native
else
    [ ! -s "$squashfs" ] || echo "1. cached squashfs inputs changed; rebuilding"
    echo "1. mmdebstrap $codename ($mode) → $squashfs"
    prepare_iso_tree_for_hook
    # Acquire::Retries because this is a 1.2 GB download from snapshot.ubuntu.com at a few
    # hundred kB/s — half an hour of connection — and apt's default of no retries meant one
    # dropped connection near the end (2026-09-09: 388 fetches failed at once, after 1181 MB had
    # already arrived) discarded every byte of it and cost the whole half hour again.
    # ForceIPv4: security.ubuntu.com has AAAA records; a hung IPv6 CLOSE_WAIT there stalls
    # apt's acquire pipeline for hours while snapshot.ubuntu.com is IPv4-only (2026-09-11:
    # 110 of ~1505 packages in 3h, dpkg idle after essential unpack).
    # Pin the set so a pool holding three sets yields this one, not the newest.
    # apt-cache: sync-in/out of $apt_archives plus --skip=essential/unlink, as in mmdebstrap(1).
    # Cleanup still strips archives from the image; the host cache keeps the .debs.
    pref="$work/templaros-set.pref"
    printf 'Package: templaros-* templar-files\nPin: version %s\nPin-Priority: 1001\n' "$set_id" > "$pref"
    rm -f "$squashfs" "$cache_stamp"
    squashfs_tmp="$squashfs.tmp"
    rm -f "$squashfs_tmp"
    # TMPDIR on the build volume, never the default /tmp: mmdebstrap assembles the whole rootfs
    # in its tempdir, and /tmp here is a 16 GB tmpfs — RAM. The 2026-09-09 build put 8.4 GB of
    # chroot into it, left the machine 268 MB free and 3.9 GB into swap, and came within a couple
    # of gigabytes of the OOM killer reaching the session running the build. The build volume is
    # the thing that exists for this (ISO-PLAN.md P0.2) and it is not NTFS, so it can hold a root.
    mkdir -p "$work/tmp" "$apt_archives/partial"
    TEMPLAROS_ISO_OUT="$tree" TEMPLAROS_ISO_SET="$set_id" TMPDIR="$work/tmp" \
    mmdebstrap --mode="$mode" --variant=minbase --architectures=amd64 --components="$components" \
        --include="$includes" \
        --skip=essential/unlink \
        --aptopt='Apt::Install-Recommends "true"' \
        --aptopt='Acquire::Retries "8"' \
        --aptopt='Acquire::ForceIPv4 "true"' \
        --keyring="$archive/templaros-dev.gpg" \
        --setup-hook="upload $pref /etc/apt/preferences.d/templaros-set" \
        --setup-hook="mkdir -p \"$apt_archives\" \"\$1/var/cache/apt/archives\"" \
        --setup-hook="sync-in $apt_archives /var/cache/apt/archives/" \
        --customize-hook="upload $root/distro/branding/os-release /usr/lib/os-release.templaros" \
        --customize-hook="upload $root/distro/image/mkosi.postinst /tmp/templaros-postinst" \
        --customize-hook='chroot "$1" sh /tmp/templaros-postinst; rm -f "$1/tmp/templaros-postinst"' \
        --customize-hook="$here/live/customize.sh" \
        --customize-hook="sync-out /var/cache/apt/archives $apt_archives" \
        "$codename" - \
        "deb $mirror $codename main universe restricted" \
        "deb $mirror $codename-updates main universe restricted" \
        "deb $security $codename-security main universe restricted" \
        "deb copy://$archive ./" \
        | mksquashfs - "$squashfs_tmp" -tar -comp zstd -Xcompression-level 15 -noappend -quiet \
        || { rm -f "$squashfs_tmp"; die "the bootstrap failed (see above); the partial squashfs was removed"; }
    mv "$squashfs_tmp" "$squashfs"
    save_ubuntu_layer
    printf '%s\n' "$cache_key" > "$cache_stamp.tmp"
    mv "$cache_stamp.tmp" "$cache_stamp"
    [ -s "$squashfs" ] || die "no squashfs was produced"
fi
for f in casper/vmlinuz casper/initrd efi/BOOTX64.EFI efi/grubx64.efi efi/mmx64.efi casper/filesystem.manifest; do
    [ -s "$tree/$f" ] || die "the customize hook left no $f; delete $squashfs and rebuild"
done
packaged_provision_sha="$(unsquashfs -cat "$squashfs" usr/libexec/templaros/provision 2>/dev/null | sha256sum | awk '{print $1}')"
source_provision_sha="$(sha256sum "$root/installer/provision.sh" | awk '{print $1}')"
[ "$packaged_provision_sha" = "$source_provision_sha" ] \
    || die "the squashfs contains a stale provisioner ($packaged_provision_sha, expected $source_provision_sha); rebuild and publish the package set first"

# --- 2. The set, and what tier each package is -----------------------------------------------------
echo "2. archive → templaros/devrepo, tiers manifest"
rm -rf "$tree/templaros/devrepo"
mkdir -p "$tree/templaros/devrepo"
# Everything but the signing key: a private key has no place on a medium.
for entry in "$archive"/*; do
    [ "$(basename "$entry")" = gnupg ] && continue
    cp -a "$entry" "$tree/templaros/devrepo/"
done
# What the provisioner verifies against on the medium: the public key and the signed index.
for f in templaros-dev.gpg InRelease Release.gpg Packages; do
    [ -s "$tree/templaros/devrepo/$f" ] || die "the archive carries no $f; publish the set again before building a medium"
done
cat > "$tree/templaros/BUILD-ID" <<EOF
set=$set_id
source_commit=$(git -C "$root" rev-parse --short HEAD)
provisioner_sha256=$source_provision_sha
squashfs_inputs_sha256=$cache_key
EOF
native="$(sed -n 's/^Package: //p' "$archive/Packages" | sort -u)"
{
    echo "# package version tier — native: built by this repository; inherited: Ubuntu $codename as is."
    echo "# set $set_id"
    while read -r pkg ver; do
        tier=inherited
        printf '%s\n' "$native" | grep -qx "$pkg" && tier=native
        printf '%s %s %s\n' "$pkg" "$ver" "$tier"
    done < "$tree/casper/filesystem.manifest"
} > "$tree/templaros/TIERS.manifest"
printf 'native %s, inherited %s\n' "$(grep -c ' native$' "$tree/templaros/TIERS.manifest")" \
    "$(grep -c ' inherited$' "$tree/templaros/TIERS.manifest")"

# --- 3. Boot files -------------------------------------------------------------------------------
echo "3. GRUB configuration and the EFI system partition image"
printf 'TemplarOS %s amd64 (%s)\n' "$set_id" "$(date -u +%Y-%m-%d)" > "$tree/.disk/info"
# console=tty1 is the laptop panel's getty. console=ttyS0 is for the QEMU harness
# (iso-qemu.sh greps the serial file). `console=tty0` does *not* spawn getty@tty1 —
# a laptop then has only serial-getty@ttyS0, which fails with I/O errors and no
# login prompt. casper-md5check is masked: this medium ships no md5sum.txt.
console_args="console=tty1 console=ttyS0,115200 systemd.mask=casper-md5check.service"
cat > "$tree/boot/grub/grub.cfg" <<CFG
set default=0
set timeout=8
menuentry "TemplarOS $set_id — live session" {
    linux /casper/vmlinuz boot=casper $console_args quiet ---
    initrd /casper/initrd
}
menuentry "TemplarOS $set_id — live session, loaded into RAM" {
    linux /casper/vmlinuz boot=casper toram $console_args quiet ---
    initrd /casper/initrd
}
# The two rescue entries. Neither is a variant of the first for its own sake: each answers one of
# the two ways a machine fails to reach a session.
#
# Safe graphics does NOT use nomodeset. nomodeset turns off kernel modesetting for every driver,
# including the Intel one the compositor renders on, so the session would come up on a blank
# screen — the very symptom being escaped. What actually goes wrong on a hybrid laptop is the
# NVIDIA stack, so this entry removes exactly that and leaves i915/amdgpu modesetting alone.
menuentry "TemplarOS $set_id — safe graphics (no NVIDIA)" {
    linux /casper/vmlinuz boot=casper modprobe.blacklist=nvidia,nvidia_drm,nvidia_modeset,nvidia_uvm,nouveau $console_args ---
    initrd /casper/initrd
}
# No session at all: multi-user.target stops before greetd, so casper's autologin lands on a text
# console with the preflight report already printed. This is the entry to boot when the graphical
# one shows nothing, and it is where nomodeset belongs, since nothing here needs modesetting.
menuentry "TemplarOS $set_id — text console (no session)" {
    linux /casper/vmlinuz boot=casper systemd.unit=multi-user.target nomodeset $console_args ---
    initrd /casper/initrd
}
# Unattended, for the reference laptop: no session, the autoinstall unit (live/customize.sh)
# finds the OS disk by filesystem label, replays the provisioner's own dry-run plan, and stops
# on tty1 with the reason for any refusal. Last and never the default: a timeout must not install.
menuentry "TemplarOS $set_id — install unattended onto the reference laptop (writes to disk)" {
    linux /casper/vmlinuz boot=casper systemd.unit=multi-user.target templaros.autoinstall=1 $console_args ---
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
# The hook's efi/ stays where it is so a rerun after a failed xorriso finds its inputs; xorriso
# leaves it out of the medium (-m below).

# --- 4. The ISO ----------------------------------------------------------------------------------
echo "4. xorriso → $iso"
rm -f "$iso"
# UEFI only (ADR-0004: no legacy BIOS path). El Torito for optical media; the appended GPT
# partition is what a USB stick's firmware boots.
xorriso -as mkisofs -r -V TEMPLAROS_LIVE -J -joliet-long -l -iso-level 3 \
    -o "$iso" -partition_offset 16 \
    -append_partition 2 0xef "$efi_img" -appended_part_as_gpt \
    -eltorito-alt-boot -e --interval:appended_partition_2:all:: -no-emul-boot \
    -m "$tree/efi" "$tree" 2>&1 | grep -v '^xorriso : UPDATE' | tail -3
[ -s "$iso" ] || die "xorriso produced no ISO"
( cd "$out" && sha256sum "$(basename "$iso")" > "$(basename "$iso").sha256" )
cp "$tree/templaros/TIERS.manifest" "$out/templaros-$set_id.TIERS.manifest"
printf '\n%s\n%s\n' "$iso" "$(cat "$iso.sha256")"
