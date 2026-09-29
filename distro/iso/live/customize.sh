#!/bin/sh
# mmdebstrap customize hook for the live medium. $1 is the rootfs; the environment carries
# TEMPLAROS_ISO_OUT (the ISO tree being assembled) and TEMPLAROS_ISO_SET (the build-set id).
#
# Everything here is LIVE MEDIUM ONLY. The installed system gets none of it: the provisioner
# creates a real account with no password baked in, locks root, and leaves the greeter asking.
set -eu
rootfs="$1"
out="${TEMPLAROS_ISO_OUT:?}"
set_id="${TEMPLAROS_ISO_SET:?}"

# casper creates this account at boot (25adduser), in the sudo group with NOPASSWD, and hostname.
cat > "$rootfs/etc/casper.conf" <<CONF
# LIVE MEDIUM ONLY — casper reads this in the initramfs; it never reaches an installed system.
export USERNAME="templaros"
export USERFULLNAME="TemplarOS live session"
export HOST="templaros-live"
export BUILD_SYSTEM="Ubuntu"
export FLAVOUR="TemplarOS"
CONF

# greetd logs that account straight into the TemplarOS session; the greeter still exists for a
# logout. The installed system's config.toml is the package's, without this block.
# env= is required: initial_session does not go through the wayland-session desktop file,
# so XDG_CURRENT_DESKTOP would otherwise be unset (systemctl import-environment ignores it).
# Drop a previous live-only append so a native-set refresh does not stack [initial_session].
if [ -f "$rootfs/etc/greetd/config.toml" ]; then
    sed -i '/LIVE MEDIUM ONLY — appended by distro\/iso\/live\/customize.sh/,$d' \
        "$rootfs/etc/greetd/config.toml"
fi
cat >> "$rootfs/etc/greetd/config.toml" <<CONF

# LIVE MEDIUM ONLY — appended by distro/iso/live/customize.sh. Not in the templaros-greeter package.
[initial_session]
command = "env XDG_CURRENT_DESKTOP=TemplarOS XDG_SESSION_DESKTOP=templaros XDG_SESSION_TYPE=wayland templar-session"
user = "templaros"
CONF

# casper's useradd copies skel. Without these directories, templar-sessiond's
# ProtectHome=read-only / ReadWritePaths bind fails (226/NAMESPACE) and the live
# session has no providers, no Settings, no terminal, no poweroff.
mkdir -p "$rootfs/etc/skel/.config/templaros" "$rootfs/etc/skel/.local/share/applications"
chmod 0700 "$rootfs/etc/skel/.config/templaros"

cat > "$rootfs/etc/motd" <<'MOTD'

TemplarOS live medium — this is not an installed system.
There is no graphical installer. The install downloads Ubuntu, so plug in the
USB-C ethernet adapter first and check it: nmcli device (wired: connected).
On the reference laptop the last GRUB entry installs unattended. By hand:

  sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --free-space /dev/DISK --user YOURNAME

First run lsblk -o NAME,MODEL,SERIAL,SIZE,FSTYPE,LABEL,PARTLABEL,MOUNTPOINTS.
DISK is the OS drive with lubuntu_2504 and the expected free extent. The disk
containing label Second is the data drive and must never be selected. NVMe names
can swap between boots. Read and save the dry plan before using --apply --plan.

If the graphical session cannot open Files, Settings or a terminal, sessiond
did not start: pick the GRUB entry "text console (no session)" and install
from here.

MOTD

# The sources mmdebstrap wrote name the build host's paths (copy://…); the live system gets the
# ordinary Ubuntu archive. Its TemplarOS archive is the medium itself, which the provisioner finds at
# /cdrom/templaros/devrepo — no apt source is needed for that.
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
rm -f "$rootfs/etc/apt/preferences.d/templaros-set"

# casper-md5check wants /cdrom/md5sum.txt, which this medium does not ship. The unit
# fails and, on a laptop, the failure is the last line on the screen so it looks like
# a hang. Mask it: integrity of a developer ISO is the .sha256 next to the image.
ln -sf /dev/null "$rootfs/etc/systemd/system/casper-md5check.service"

# console=tty0 does not spawn a VT getty. Pin tty1 so the text-console GRUB entry
# has a login on the panel, not only serial-getty@ttyS0 (which I/O-errors on a laptop).
mkdir -p "$rootfs/etc/systemd/system/getty.target.wants"
ln -sf /usr/lib/systemd/system/getty@.service \
    "$rootfs/etc/systemd/system/getty.target.wants/getty@tty1.service"

# The hardware preflight, once, at boot. The report is what `templaros-install` refuses on
# (installer/provision.sh), and journal+console puts it on the serial console as well, which is
# how tests/integration/iso-qemu.sh reads it. The unit never fails: a hardware failure is a
# finding for the person at the machine, not a boot failure that hides itself.
mkdir -p "$rootfs/usr/lib/systemd/system" "$rootfs/etc/systemd/system/multi-user.target.wants"
cat > "$rootfs/usr/lib/systemd/system/templaros-preflight.service" <<CONF
# LIVE MEDIUM ONLY — written by distro/iso/live/customize.sh.
[Unit]
Description=TemplarOS live hardware preflight
After=NetworkManager.service systemd-udevd.service
Before=greetd.service
ConditionPathExists=/usr/bin/templar

[Service]
Type=oneshot
RemainAfterExit=yes
StandardOutput=journal+console
StandardError=journal+console
ExecStart=/bin/sh -c 'mkdir -p /run/templaros; templar doctor --preflight > /run/templaros/preflight.txt 2>&1 || true; cat /run/templaros/preflight.txt; /usr/lib/templaros/live-report'

[Install]
WantedBy=multi-user.target
CONF
ln -sf ../../../../usr/lib/systemd/system/templaros-preflight.service \
    "$rootfs/etc/systemd/system/multi-user.target.wants/templaros-preflight.service"
# The same report, plus the kernel command line and the firmware's Secure Boot byte, onto the
# serial port when there is one. `quiet` keeps the kernel's own "Command line:" and "Secure boot
# enabled" lines off the console, and journal+console goes to /dev/console, which is tty0 on the
# medium — so tests/integration/iso-qemu.sh reads this, not the kernel log. A laptop has no ttyS0
# and this prints nothing.
mkdir -p "$rootfs/usr/lib/templaros"
cat > "$rootfs/usr/lib/templaros/live-report" <<'REPORT'
#!/bin/sh
# LIVE MEDIUM ONLY — written by distro/iso/live/customize.sh.
[ -c /dev/ttyS0 ] || exit 0
{
    printf 'templaros-live cmdline: %s\n' "$(cat /proc/cmdline)"
    sb=/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c
    if [ -r "$sb" ]; then
        printf 'templaros-live secure-boot: %s\n' "$(od -An -tu1 -j4 -N1 "$sb" | tr -d ' ')"
    else
        printf 'templaros-live secure-boot: no-efivar\n'
    fi
    cat /run/templaros/preflight.txt
} > /dev/ttyS0 2>/dev/null || true
REPORT
chmod 0755 "$rootfs/usr/lib/templaros/live-report"

# Unattended install, for the reference laptop only. Selected by the GRUB entry that puts
# templaros.autoinstall=1 on the command line; every other boot leaves this unit dormant. The
# script guesses nothing: the OS disk is the one carrying the filesystem labelled
# TEMPLAROS_AUTOINSTALL_OS_LABEL, the data disk (label Second) must exist and be a different
# disk, the preflight must have passed, and the provisioner's own dry run is replayed with
# --plan so any drift refuses. A refusal stays on tty1 with its reason; only success powers off.
cat > "$rootfs/usr/lib/templaros/live-autoinstall" <<'AUTO'
#!/bin/sh
# LIVE MEDIUM ONLY — written by distro/iso/live/customize.sh. Runs as root from
# templaros-autoinstall.service. Refuses loudly; never picks a disk by name.
set -u
user="${TEMPLAROS_AUTOINSTALL_USER:-dk}"
os_label="${TEMPLAROS_AUTOINSTALL_OS_LABEL:-lubuntu_2504}"
data_label="${TEMPLAROS_AUTOINSTALL_DATA_LABEL:-Second}"
initial_password="${TEMPLAROS_AUTOINSTALL_PASSWORD:-templaros}"
plan=/run/templaros/autoinstall-plan.json
log() { printf '\nautoinstall: %s\n' "$*"; }
stop() {
    log "STOPPED — $*"
    log "nothing was written unless a step above is marked APPLY. Log in on this console (user templaros, no password) to inspect; journalctl -u templaros-autoinstall has this output."
    exit 1
}

log "reference-laptop unattended install: user $user, OS disk by label $os_label, data disk by label $data_label"

# The install downloads Ubuntu, so the network comes first. On the reference laptop it is wired: a
# USB-C ethernet adapter (r8152/ax88179/cdc_ncm, all in linux-modules; Realtek firmware in
# linux-firmware-realtek). NetworkManager manages it because mkosi.postinst names it the netplan
# renderer. Plug the adapter in before choosing this entry; it is given 90 s to get an address.
log "network: waiting up to 90 s for a connection"
nm-online -q -t 90 || true
nmcli -t -f DEVICE,TYPE,STATE,CONNECTION device 2>&1 | sed 's/^/autoinstall: network: /'
ip -br addr 2>&1 | sed 's/^/autoinstall: address: /'
ip route show default | grep -q . \
    || stop "no network: no default route. Plug in the USB-C ethernet adapter, check 'nmcli device' shows it connected, then reboot into this entry"
os_part="$(blkid -L "$os_label")" || stop "no filesystem labelled $os_label; this is not the reference laptop"
data_part="$(blkid -L "$data_label")" || stop "no filesystem labelled $data_label; this is not the reference laptop"
os_disk="/dev/$(lsblk -dnro PKNAME "$os_part")"
data_disk="/dev/$(lsblk -dnro PKNAME "$data_part")"
[ -b "$os_disk" ] || stop "cannot resolve the disk behind $os_part"
[ "$os_disk" != "$data_disk" ] || stop "$os_label and $data_label sit on the same disk $os_disk; the layout assumption is wrong"
log "OS disk $os_disk ($os_part), data disk $data_disk ($data_part) will not be touched"
lsblk -o NAME,MODEL,SERIAL,SIZE,FSTYPE,LABEL,MOUNTPOINTS "$os_disk" "$data_disk"

[ -r /run/templaros/preflight.txt ] || stop "no hardware preflight report at /run/templaros/preflight.txt"
if grep -q '^fail ' /run/templaros/preflight.txt; then
    grep '^fail ' /run/templaros/preflight.txt
    stop "the hardware preflight failed; install by hand with --ignore-preflight if that is intended"
fi

export TEMPLAROS_PROVISION_ALLOW_DEVICE=1
log "1/2 dry run: the plan is written to $plan and nothing is touched"
templaros-install --free-space "$os_disk" --user "$user" --password-stdin --expire-password \
    --plan-out "$plan" </dev/null || stop "the provisioner refused to plan (its reason is above)"
grep -q 'TEMPLAR_ESP' "$plan" || stop "the plan names no new ESP; not applying it"

log "2/2 APPLY: the reviewed plan, refused on any drift"
printf '%s\n' "$initial_password" \
    | templaros-install --free-space "$os_disk" --user "$user" --password-stdin --expire-password \
        --apply --plan "$plan" || stop "apply refused or failed (its reason is above); the machine may be partly written — read the steps marked APPLY"

log "DONE. TemplarOS is installed on $os_disk and appended to the firmware boot menu."
log "First login: user $user, password '$initial_password' — it must be changed immediately."
log "Remove the USB stick. This machine powers off in 60 seconds (Ctrl+C on this console to stay)."
sleep 60
systemctl poweroff
AUTO
chmod 0755 "$rootfs/usr/lib/templaros/live-autoinstall"
cat > "$rootfs/usr/lib/systemd/system/templaros-autoinstall.service" <<CONF
# LIVE MEDIUM ONLY — written by distro/iso/live/customize.sh.
[Unit]
Description=TemplarOS unattended install (reference laptop)
ConditionKernelCommandLine=templaros.autoinstall=1
After=templaros-preflight.service network-online.target
Wants=templaros-preflight.service network-online.target
Conflicts=greetd.service

[Service]
Type=oneshot
RemainAfterExit=yes
StandardOutput=journal+console
StandardError=journal+console
Environment=TEMPLAROS_AUTOINSTALL_USER=${TEMPLAROS_AUTOINSTALL_USER:-dk}
ExecStart=/usr/lib/templaros/live-autoinstall

[Install]
WantedBy=multi-user.target
CONF
ln -sf ../../../../usr/lib/systemd/system/templaros-autoinstall.service \
    "$rootfs/etc/systemd/system/multi-user.target.wants/templaros-autoinstall.service"

# The live initramfs: casper's hooks go in when initramfs-tools runs with casper installed.
chroot "$rootfs" update-initramfs -u -k all

# Out to the ISO tree: kernel, initramfs, the Canonical-signed boot chain, the manifests.
mkdir -p "$out/casper" "$out/efi" "$out/templaros"
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
# The kernel's own version line, for the record and for `templar doctor` on the medium.
basename "$kernel" | sed 's/^vmlinuz-//' > "$out/templaros/kernel"
