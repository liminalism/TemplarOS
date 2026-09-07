#!/bin/sh
# mmdebstrap customize hook for the live medium. $1 is the rootfs; the environment carries
# LEGEOS_ISO_OUT (the ISO tree being assembled) and LEGEOS_ISO_SET (the build-set id).
#
# Everything here is LIVE MEDIUM ONLY. The installed system gets none of it: the provisioner
# creates a real account with no password baked in, locks root, and leaves the greeter asking.
set -eu
rootfs="$1"
out="${LEGEOS_ISO_OUT:?}"
set_id="${LEGEOS_ISO_SET:?}"

# casper creates this account at boot (25adduser), in the sudo group with NOPASSWD, and hostname.
cat > "$rootfs/etc/casper.conf" <<CONF
# LIVE MEDIUM ONLY — casper reads this in the initramfs; it never reaches an installed system.
export USERNAME="legeos"
export USERFULLNAME="LegeOS live session"
export HOST="legeos-live"
export BUILD_SYSTEM="Ubuntu"
export FLAVOUR="LegeOS"
CONF

# greetd logs that account straight into the LegeOS session; the greeter still exists for a
# logout. The installed system's config.toml is the package's, without this block.
cat >> "$rootfs/etc/greetd/config.toml" <<CONF

# LIVE MEDIUM ONLY — appended by distro/iso/live/customize.sh. Not in the legeos-greeter package.
[initial_session]
command = "lege-session"
user = "legeos"
CONF

# The sources mmdebstrap wrote name the build host's paths (copy://…); the live system gets the
# ordinary Ubuntu archive. Its LegeOS archive is the medium itself, which the provisioner finds at
# /cdrom/legeos/devrepo — no apt source is needed for that.
rm -f "$rootfs"/etc/apt/sources.list.d/0000main.list "$rootfs"/etc/apt/sources.list
cat > "$rootfs/etc/apt/sources.list.d/ubuntu.sources" <<CONF
Types: deb
URIs: http://archive.ubuntu.com/ubuntu
Suites: resolute resolute-updates
Components: main universe restricted
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: http://security.ubuntu.com/ubuntu
Suites: resolute-security
Components: main universe restricted
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
CONF
rm -f "$rootfs/etc/apt/preferences.d/legeos-set"

# The hardware preflight, once, at boot. The report is what `legeos-install` refuses on
# (installer/provision.sh), and journal+console puts it on the serial console as well, which is
# how tests/integration/iso-qemu.sh reads it. The unit never fails: a hardware failure is a
# finding for the person at the machine, not a boot failure that hides itself.
mkdir -p "$rootfs/usr/lib/systemd/system" "$rootfs/etc/systemd/system/multi-user.target.wants"
cat > "$rootfs/usr/lib/systemd/system/legeos-preflight.service" <<CONF
# LIVE MEDIUM ONLY — written by distro/iso/live/customize.sh.
[Unit]
Description=LegeOS live hardware preflight
After=NetworkManager.service systemd-udevd.service
Before=greetd.service
ConditionPathExists=/usr/bin/lege

[Service]
Type=oneshot
RemainAfterExit=yes
StandardOutput=journal+console
StandardError=journal+console
ExecStart=/bin/sh -c 'mkdir -p /run/legeos; lege doctor --preflight > /run/legeos/preflight.txt 2>&1 || true; cat /run/legeos/preflight.txt'

[Install]
WantedBy=multi-user.target
CONF
ln -sf ../../../../usr/lib/systemd/system/legeos-preflight.service \
    "$rootfs/etc/systemd/system/multi-user.target.wants/legeos-preflight.service"

# The live initramfs: casper's hooks go in when initramfs-tools runs with casper installed.
chroot "$rootfs" update-initramfs -u -k all

# Out to the ISO tree: kernel, initramfs, the Canonical-signed boot chain, the manifests.
mkdir -p "$out/casper" "$out/efi" "$out/legeos"
kernel="$(ls "$rootfs"/boot/vmlinuz-* | sort -V | tail -1)"
initrd="$(ls "$rootfs"/boot/initrd.img-* | sort -V | tail -1)"
cp "$kernel" "$out/casper/vmlinuz"
cp "$initrd" "$out/casper/initrd"
cp "$rootfs/usr/lib/shim/shimx64.efi.signed" "$out/efi/BOOTX64.EFI"
cp "$rootfs/usr/lib/shim/mmx64.efi" "$out/efi/mmx64.efi"
cp "$rootfs/usr/lib/grub/x86_64-efi-signed/gcdx64.efi.signed" "$out/efi/grubx64.efi"
chroot "$rootfs" dpkg-query -W --showformat='${Package} ${Version}\n' > "$out/casper/filesystem.manifest"
# Ubuntu installs the kernel 0600 root-only, and inside mmdebstrap's user namespace that root is a
# subordinate uid the host user cannot read as. xorriso runs as the host user, so every file that
# leaves the namespace here is made world-readable before it does (2026-09-07: the first build's
# xorriso failed on exactly this, "Access to file is not allowed" for casper/vmlinuz).
chmod 0644 "$out/casper/vmlinuz" "$out/casper/initrd" "$out/casper/filesystem.manifest" \
    "$out/efi/BOOTX64.EFI" "$out/efi/mmx64.efi" "$out/efi/grubx64.efi"
# The kernel's own version line, for the record and for `lege doctor` on the medium.
basename "$kernel" | sed 's/^vmlinuz-//' > "$out/legeos/kernel"
