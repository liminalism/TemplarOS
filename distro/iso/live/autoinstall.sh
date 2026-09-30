#!/usr/bin/env bash
# LIVE MEDIUM ONLY — installed as /usr/lib/templaros/live-autoinstall (and `templaros-autoinstall`)
# by distro/iso/live/customize.sh. The unattended install for the reference laptop, and nothing
# else: every choice the manual flow asks for is fixed here, because on this machine none of them
# is a choice.
#
#   disk      the one carrying the filesystem labelled lubuntu_2504 (NVMe names swap between boots,
#             so never by name); the disk carrying `Second` is the data disk and is never written
#   target    the TemplarOS pair already on that disk (partition names TEMPLAR_ESP and
#             templaros-root) when there is one, otherwise its largest free block
#   progress  the provisioner's own verdict on that pair: fresh or rebuild -> reformat only those
#             two partitions and install; resumable -> continue after the bootstrap; complete ->
#             nothing to do; anything else -> stop
#   account   dk, password `templaros` — change it with `passwd` after the first login (greetd
#             cannot run a forced first-login change, so the password is not expired)
#
# Run by templaros-autoinstall.service from the last GRUB entry, or by hand from the live session:
# `sudo templaros-autoinstall`. Running it again after any failure is always the next step — it
# works out for itself whether to resume or rebuild. It refuses loudly and never guesses a disk.
set -u
user="${TEMPLAROS_AUTOINSTALL_USER:-dk}"
os_label="${TEMPLAROS_AUTOINSTALL_OS_LABEL:-lubuntu_2504}"
data_label="${TEMPLAROS_AUTOINSTALL_DATA_LABEL:-Second}"
initial_password="${TEMPLAROS_AUTOINSTALL_PASSWORD:-templaros}"
esp_name=TEMPLAR_ESP
root_name=templaros-root
plan=/run/templaros/autoinstall-plan.json

# A checkout's provisioner wins over the medium's, the same rule provision.sh applies to its data:
# this is how a fix reaches an existing stick without a rebuild (run this file from the checkout).
here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
if [ -f "$here/../../../installer/provision.sh" ]; then
    provision=(bash "$(cd "$here/../../.." && pwd)/installer/provision.sh")
    again="sudo $(readlink -f "$0")"
else
    provision=(templaros-install)
    again="sudo templaros-autoinstall (or reboot into the unattended GRUB entry)"
fi

log() { printf '\nautoinstall: %s\n' "$*"; }
stop() {
    log "STOPPED — $*"
    log "journalctl -u templaros-autoinstall has this output when it ran from the GRUB entry; log in on tty1 as templaros (no password) to inspect."
    exit 1
}

[ "$(id -u)" = 0 ] || stop "run as root: sudo $0"
mkdir -p "${plan%/*}"
log "reference-laptop unattended install: user $user, OS disk by label $os_label, data disk by label $data_label"

# The install downloads Ubuntu, so the network comes first. On the reference laptop it is wired: a
# USB-C ethernet adapter (r8152/ax88179/cdc_ncm, all in linux-modules; Realtek firmware in
# linux-firmware-realtek), which NetworkManager manages. It is given 90 s to get an address.
log "network: waiting up to 90 s for a connection"
nm-online -q -t 90 || true
nmcli -t -f DEVICE,TYPE,STATE,CONNECTION device 2>&1 | sed 's/^/autoinstall: network: /'
ip -br addr 2>&1 | sed 's/^/autoinstall: address: /'
ip route show default | grep -q . \
    || stop "no network: no default route. Plug in the USB-C ethernet adapter, check 'nmcli device' shows it connected, then run this again"

os_part="$(blkid -L "$os_label")" || stop "no filesystem labelled $os_label; this is not the reference laptop"
data_part="$(blkid -L "$data_label")" || stop "no filesystem labelled $data_label; this is not the reference laptop"
os_disk="/dev/$(lsblk -dnro PKNAME "$os_part")"
data_disk="/dev/$(lsblk -dnro PKNAME "$data_part")"
[ -b "$os_disk" ] || stop "cannot resolve the disk behind $os_part"
[ "$os_disk" != "$data_disk" ] || stop "$os_label and $data_label sit on the same disk $os_disk; the layout assumption is wrong"
log "OS disk $os_disk ($os_part); data disk $data_disk ($data_part) will not be touched"
lsblk -o NAME,SIZE,FSTYPE,LABEL,PARTLABEL,MOUNTPOINTS "$os_disk" "$data_disk"

[ -r /run/templaros/preflight.txt ] || stop "no hardware preflight report at /run/templaros/preflight.txt"
if grep -q '^fail ' /run/templaros/preflight.txt; then
    grep '^fail ' /run/templaros/preflight.txt
    stop "the hardware preflight failed; install by hand with --ignore-preflight if that is intended"
fi

# The TemplarOS pair, by the partition names the provisioner gives the two it creates.
named() { # disk name -> every partition on disk with that GPT name
    lsblk -nrpo NAME,TYPE,PARTLABEL -- "$1" | awk -v want="$2" '$2 == "part" && $3 == want { print $1 }'
}
[ -z "$(named "$data_disk" "$esp_name")$(named "$data_disk" "$root_name")" ] \
    || stop "the data disk $data_disk carries a partition named $esp_name or $root_name; refusing to guess which pair is the install"
esps="$(named "$os_disk" "$esp_name")"
roots="$(named "$os_disk" "$root_name")"
[ "$(printf '%s' "$esps" | grep -c .)" -le 1 ] && [ "$(printf '%s' "$roots" | grep -c .)" -le 1 ] \
    || stop "$os_disk has more than one partition named $esp_name or $root_name"

export TEMPLAROS_PROVISION_ALLOW_DEVICE=1
export TEMPLAROS_PROVISION_RETRY_HINT="$again"
if [ -z "$esps" ] && [ -z "$roots" ]; then
    log "no TemplarOS partitions yet: install into the largest free block of $os_disk"
    target=(--free-space "$os_disk")
elif [ -n "$esps" ] && [ -n "$roots" ]; then
    verdict_line="$("${provision[@]}" --esp "$esps" --root "$roots" --inspect)" \
        || stop "the provisioner could not inspect $esps + $roots (its reason is above)"
    verdict="${verdict_line%%:*}"
    log "TemplarOS pair $esps (ESP) + $roots (root): $verdict_line"
    case "$verdict" in
        fresh|rebuild)
            log "reformatting only $esps and $roots, then installing from the start"
            target=(--esp "$esps" --root "$roots") ;;
        resumable)
            log "resuming: Ubuntu is kept, every step after the bootstrap runs again"
            target=(--esp "$esps" --root "$roots" --resume) ;;
        complete)
            log "DONE already — TemplarOS is installed on $os_disk. Nothing was written. Remove the stick and boot it from the firmware menu."
            exit 0 ;;
        *)
            stop "not continuing on a pair the provisioner does not recognise as its own" ;;
    esac
else
    stop "$os_disk has only one of the two TemplarOS partitions ($esps$roots); refusing to guess"
fi

args=("${target[@]}" --user "$user" --password-stdin)
log "1/2 dry run: the plan is written to $plan and nothing is touched"
"${provision[@]}" "${args[@]}" --plan-out "$plan" </dev/null \
    || stop "the provisioner refused to plan (its reason is above)"

log "2/2 APPLY: that plan, refused on any drift"
printf '%s\n' "$initial_password" | "${provision[@]}" "${args[@]}" --apply --plan "$plan" \
    || stop "the install did not finish (the step and the target's state are printed above). Run it again — it resumes or rebuilds by itself: $again"

log "DONE. TemplarOS is installed on $os_disk and appended to the firmware boot menu."
log "First login: user $user, password '$initial_password'. Change it with 'passwd' once you are in."
if [ "${TEMPLAROS_AUTOINSTALL_POWEROFF:-0}" = 1 ]; then
    log "Remove the USB stick. This machine powers off in 60 seconds (Ctrl+C on this console to stay)."
    sleep 60
    systemctl poweroff
fi
