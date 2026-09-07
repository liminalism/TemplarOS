#!/usr/bin/env bash
# Provision a LegeOS Developer Edition target from an Ubuntu 26.04 base.
#
# This is a *provisioner*, not an installer: one machine shape, no OEM flow, no arbitrary layouts,
# no graphical pages. It reads distro/deploy/layout.toml (contract C3) so the partition table, the
# subvolumes and the deployment identity are declared once and not restated here. Four ways to
# name the target, one layout: a disk image, a whole disk (zapped), the free tail of a disk that
# keeps every partition it has (`--free-space`, the reference laptop's case), or two existing
# partitions (`--esp --root`).
#
# It plans by default. Nothing is written until --apply, and then only to the target you name.
# Read `installer/README.md` before running it against anything you care about, and
# `installer/CHECKLIST.md` for the parts a person has to check by hand on real hardware.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
# legeos-cli ships this script as /usr/libexec/legeos/provision with its data under
# /usr/share/legeos, so the live medium can run it with no checkout. A checkout wins when present.
if [ -f "$repo/distro/deploy/layout.toml" ]; then
    data_layout="$repo/distro/deploy/layout.toml"
    data_grub_stub="$repo/distro/deploy/grub.d/09_legeos"
    data_os_release="$repo/distro/branding/os-release"
    data_flatpak_refs="$repo/distro/seeds/flatpak-refs.list"
else
    # The live medium is itself a LegeOS: legeos-hostd ships the GRUB stub and the identity is
    # the running system's own, so only the layout needs a home of its own (legeos-cli).
    data_layout=/usr/share/legeos/layout.toml
    data_grub_stub=/etc/grub.d/09_legeos
    data_os_release=/usr/lib/os-release
    data_flatpak_refs=/usr/share/legeos/flatpak-refs.list
fi

# --- C1: package selection (D1 owns these names; they are variables so a rename is one edit) -----
LEGEOS_KEYRING="${LEGEOS_KEYRING:-legeos-archive-keyring}"
LEGEOS_DESKTOP="${LEGEOS_DESKTOP:-legeos-desktop}"
LEGEOS_SESSION="${LEGEOS_SESSION:-legeos-session-meta}"     # D1 may rename to legeos-session-full
LEGEOS_KERNEL="${LEGEOS_KERNEL:-legeos-kernel}"
LEGEOS_NVIDIA="${LEGEOS_NVIDIA:-legeos-nvidia-open}"        # or legeos-nvidia-proprietary
LEGEOS_DEV_RUST="${LEGEOS_DEV_RUST:-legeos-dev-rust}"
LEGEOS_DEV_CUDA="${LEGEOS_DEV_CUDA:-legeos-dev-cuda}"

# --- C2: the local LegeOS archive (D2 owns this shape) -------------------------------------------
# Where the set is read from: the live medium carries one at /legeos/devrepo; a development host
# has its own. Where it ends up is always the target's /var/lib/legeos/devrepo, so the installed
# system's `lege dev` finds the set it was installed from and reports itself converged.
if [ -z "${DEVREPO_ROOT:-}" ]; then
    DEVREPO_ROOT=/var/lib/legeos/devrepo
    for medium in /cdrom/legeos/devrepo /legeos/devrepo; do   # casper mounts the ISO at /cdrom
        [ -d "$medium" ] && { DEVREPO_ROOT="$medium"; break; }
    done
fi
TARGET_DEVREPO=/var/lib/legeos/devrepo
DEVREPO_MARKER="${DEVREPO_MARKER:-current}"   # current | previous. `staged` is never installable.

# --- Ubuntu base ---------------------------------------------------------------------------------
UBUNTU_SUITE="${UBUNTU_SUITE:-resolute}"
UBUNTU_MIRROR="${UBUNTU_MIRROR:-http://archive.ubuntu.com/ubuntu}"
UBUNTU_COMPONENTS="${UBUNTU_COMPONENTS:-main universe restricted}"

layout="$data_layout"
target=""
target_kind=""          # image | device | free-space | partitions
esp_arg=""
root_arg=""
username=""
apply=0
i_mean_it=0
step_no=0
data_arg=""
format_data=0
reuse=0
plan_out=""
plan_in=""
ignore_preflight=0
# The live medium leaves the boot-time hardware report here (distro/iso/live/customize.sh).
PREFLIGHT_REPORT="${PREFLIGHT_REPORT:-/run/legeos/preflight.txt}"


die() { printf 'provision: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }

usage() {
    cat <<'USAGE'
usage: provision.sh --image PATH [--apply] [options]
       provision.sh --free-space /dev/sdX [--apply] [options]
       provision.sh --esp /dev/sdXn --root /dev/sdXm [--apply] [options]
       provision.sh --device /dev/sdX --i-mean-it --apply [options]

  --image PATH        provision a disk image file (created if absent). The safe target.
  --free-space DISK   create the ESP and the root in the unallocated tail of DISK and touch no
                      existing partition. The disk keeps whatever else it boots.
  --esp PART --root PART
                      format these two existing partitions; the partition table is not touched.
  --device /dev/sdX   provision a whole block device. Requires --i-mean-it as well.
  --i-mean-it         acknowledge that --device destroys everything on that disk.
  --archive DIR       the LegeOS archive to install from (default: /legeos/devrepo on the live
                      medium, /var/lib/legeos/devrepo otherwise). Copied to the target.
  --apply             actually do it. Without this, the run prints the plan and changes nothing.
  --user NAME         the account to create. No password is baked in; see --password-stdin.
  --password-stdin    read the new user's password from stdin. Otherwise the account is created
                      with an expired password and the first login has to set one.
  --nvidia PACKAGE    the NVIDIA metapackage variant (default: $LEGEOS_NVIDIA).
  --set ID            install a specific LegeOS build set instead of the `current` marker.
  --layout PATH       an alternative layout declaration (testing).
  --data PART         an independent data filesystem (layout [data]). Reused, never formatted,
                      unless --format-data says otherwise.
  --format-data       make a new filesystem on --data. The explicit, destructive opt-in.
  --reuse             preserve every filesystem that already exists: no mkfs on the ESP or the
                      root, and no GPT type code rewritten. Refuses if a required one is absent.
  --plan-out PATH     write the reviewed operation sequence here (default: alongside the target).
  --ignore-preflight  install even though the live medium's hardware preflight reported a failure.
  --plan PATH         with --apply: run *this* reviewed plan. The machine is observed again first
                      and any drift in topology, filesystem identity or mounted state is refused.
  --help              this text.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --image) target="${2:?--image needs a path}"; target_kind=image; shift 2 ;;
        --device) target="${2:?--device needs a path}"; target_kind=device; shift 2 ;;
        --free-space) target="${2:?--free-space needs a disk}"; target_kind=free-space; shift 2 ;;
        --esp) esp_arg="${2:?--esp needs a partition}"; shift 2 ;;
        --root) root_arg="${2:?--root needs a partition}"; shift 2 ;;
        --archive) DEVREPO_ROOT="${2:?--archive needs a directory}"; shift 2 ;;
        --i-mean-it) i_mean_it=1; shift ;;
        --apply) apply=1; shift ;;
        --user) username="${2:?--user needs a name}"; shift 2 ;;
        --password-stdin) read_password=1; shift ;;
        --nvidia) LEGEOS_NVIDIA="${2:?--nvidia needs a package}"; shift 2 ;;
        --set) DEVREPO_MARKER="${2:?--set needs an id}"; shift 2 ;;
        --layout) layout="${2:?--layout needs a path}"; shift 2 ;;
        --data) data_arg="${2:?--data needs a partition}"; shift 2 ;;
        --format-data) format_data=1; shift ;;
        --reuse) reuse=1; shift ;;
        --ignore-preflight) ignore_preflight=1; shift ;;
        --plan-out) plan_out="${2:?--plan-out needs a path}"; shift 2 ;;
        --plan) plan_in="${2:?--plan needs a path}"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; die "unknown argument $1" ;;
    esac
done

# --- The layout declaration ----------------------------------------------------------------------
# A deliberately small reader: the declaration is flat, and a provisioner that needs a TOML library
# to find out where to put a partition has one more thing that can be missing on a rescue shell.
layout_value() { # section key
    awk -v section="[$1]" -v key="$2" '
        /^\[/ { in_section = ($0 == section); next }
        in_section && $0 ~ "^" key "[ =]" {
            sub("^" key " *= *", ""); gsub(/^["\047]|["\047]$/, ""); print; exit
        }' "$layout"
}
layout_list() { # section key -> whitespace-separated items
    layout_value "$1" "$2" | tr -d '[]",'
}

[ -f "$layout" ] || die "no layout declaration at $layout"
ESP_FORMAT="$(layout_value esp format)"
ESP_LABEL="$(layout_value esp label)"
ESP_SIZE_MAX="$(layout_value esp size_max)"
ESP_MOUNT="$(layout_value esp mount)"
ROOT_FORMAT="$(layout_value root format)"
ROOT_LABEL="$(layout_value root label)"
SUBVOLUMES="$(layout_list root subvolumes)"
ID_PREFIX="$(layout_value deployment install_id_prefix)"
ESP_DIR_TEMPLATE="$(layout_value deployment esp_dir_template)"
CMDLINE_TEMPLATE="$(layout_value deployment.cmdline template)"
GRUB_ENTRIES="$(layout_value boot.grub entries_file)"
GRUB_STUB="$(layout_value boot.grub stub)"
GRUB_DEFAULT_MODE="$(layout_value boot.grub default)"
DATA_FORMAT="$(layout_value data format)"
DATA_LABEL="$(layout_value data label)"
DATA_MOUNT="$(layout_value data mount)"
DATA_MOUNT_OPTIONS="$(layout_value data mount_options)"
for required in ESP_FORMAT ESP_LABEL ROOT_FORMAT ROOT_LABEL SUBVOLUMES CMDLINE_TEMPLATE; do
    [ -n "${!required}" ] || die "$layout does not declare $required"
done

deployment_id="${ID_PREFIX}$(date -u +%Y%m%dT%H%M%SZ)"
esp_dir="${ESP_DIR_TEMPLATE/\{id\}/$deployment_id}"
cmdline="${CMDLINE_TEMPLATE/\{id\}/$deployment_id}"

# --- Refusals ------------------------------------------------------------------------------------
# Everything below this line is destructive. These are the conditions under which it must not run,
# stated once, before any of it.

# The live medium runs `lege doctor --preflight` once at boot and leaves its report behind.
# Installing onto a machine whose display or network the kernel could not drive is exactly how
# somebody ends up with a system that boots to nothing, so a failed line refuses to write. Nothing
# is probed again here: the report is the observation, and --ignore-preflight is the override.
if [ "$apply" = 1 ] && [ "$ignore_preflight" = 0 ] && [ -r "$PREFLIGHT_REPORT" ]; then
    preflight_failures="$(grep '^fail ' "$PREFLIGHT_REPORT" || true)"
    [ -z "$preflight_failures" ] || die "the hardware preflight failed on this machine:
$preflight_failures
  (report: $PREFLIGHT_REPORT)
Boot the medium's safe-graphics or text-console entry and report this, or pass --ignore-preflight
to install anyway."
fi
if [ -n "$esp_arg" ] || [ -n "$root_arg" ]; then
    [ -n "$esp_arg" ] && [ -n "$root_arg" ] || die "--esp and --root go together"
    [ -z "$target" ] || die "--esp/--root name the target; do not also pass --image, --device or --free-space"
    target="$esp_arg+$root_arg"; target_kind=partitions
fi
[ -n "$target" ] || { usage >&2; die "no target: pass --image PATH, --free-space DISK, --esp/--root, or --device with --i-mean-it"; }

case "$target_kind" in
    image)
        case "$target" in
            /dev/*) die "$target looks like a device; pass it with --device, --free-space or --esp/--root, not --image" ;;
        esac
        [ -b "$target" ] && die "$target is a block device; pass it with --device, --free-space or --esp/--root, not --image"
        ;;
    device)
        [ "$i_mean_it" = 1 ] || die "--device destroys every partition on $target. Add --i-mean-it if that is what you want."
        root_source="$(findmnt -no SOURCE / 2>/dev/null || true)"
        root_disk="$(lsblk -no PKNAME "$root_source" 2>/dev/null | head -1 || true)"
        case "$target" in
            *"${root_disk:-__none__}"*) die "$target is the disk this system booted from. Refusing." ;;
        esac
        ;;
    free-space)
        # Not destructive to what exists, but it still writes a partition table; a regular file
        # (a fixture, or an image with partitions on it already) is the way to rehearse it.
        [ -e "$target" ] || die "$target does not exist"
        if [ -b "$target" ]; then
            [ -n "$(lsblk -no PKNAME "$target" 2>/dev/null)" ] && die "$target is a partition; --free-space takes the whole disk it is on"
        fi
        ;;
    partitions)
        for part in "$esp_arg" "$root_arg"; do
            [ -b "$part" ] || die "$part is not a block device"
            [ -n "$(lsblk -no PKNAME "$part" 2>/dev/null)" ] || die "$part is a whole disk, not a partition. --esp/--root format partitions; --device takes a disk."
            findmnt -S "$part" >/dev/null 2>&1 && die "$part is mounted. Refusing to format a filesystem that is in use."
        done
        [ "$esp_arg" != "$root_arg" ] || die "--esp and --root name the same partition"
        ;;
esac
if [ -n "$data_arg" ]; then
    [ -n "$DATA_LABEL" ] || die "$layout declares no [data] role, so --data has nothing to mean"
    [ -b "$data_arg" ] || [ -f "$data_arg" ] || die "$data_arg is neither a block device nor a file"
    findmnt -S "$data_arg" >/dev/null 2>&1 && die "$data_arg is mounted. Refusing to touch a filesystem that is in use."
    for other in "$esp_arg" "$root_arg"; do
        [ -n "$other" ] && [ "$data_arg" = "$other" ] && die "--data names the same partition as --esp or --root"
    done
fi
[ "$format_data" = 1 ] && [ -z "$data_arg" ] && die "--format-data without --data formats nothing"
if [ "$reuse" = 1 ]; then
    [ "$target_kind" = partitions ] \
        || die "--reuse preserves filesystems that already exist, so it goes with --esp/--root; \
--image, --device and --free-space create them"
    [ "$format_data" = 1 ] && die "--reuse and --format-data contradict each other"
fi

# Two filesystems with the same label are two filesystems the boot path cannot tell apart, and
# `root=LABEL=legeos-root` would mount whichever the kernel saw first. Identity in fstab and in
# the boot entry is by UUID for exactly this reason, but a duplicate label is still a machine
# nobody should walk away from believing is fine.
duplicate_label() { # label
    [ -n "$1" ] || return 1
    command -v blkid >/dev/null 2>&1 || return 1
    [ "$(blkid -o device -t "LABEL=$1" 2>/dev/null | wc -l)" -gt 1 ]
}
for pair in "ESP:$ESP_LABEL" "root:$ROOT_LABEL" "data:$DATA_LABEL"; do
    role="${pair%%:*}"; label="${pair#*:}"
    if duplicate_label "$label"; then
        die "more than one filesystem on this machine is already labelled $label (the $role role).
     Two filesystems with one label are two the boot path cannot tell apart. Relabel or
     disconnect one before provisioning."
    fi
done

real_disk=0
case "$target_kind" in
    device) real_disk=1 ;;
    free-space|partitions) [ -b "${esp_arg:-$target}" ] && real_disk=1 ;;
esac
if [ "$real_disk" = 1 ] && [ "${LEGEOS_PROVISION_ALLOW_DEVICE:-0}" != 1 ]; then
    die "writing to a real disk also needs LEGEOS_PROVISION_ALLOW_DEVICE=1 in the environment.
     That second switch exists so a copied-and-pasted command line cannot reach a disk on its own."
fi
# The privilege check is deliberately *not* here, above the plan comparison: a stale plan is a
# stale plan whoever is running it, and being told "needs root" when the real problem is that the
# disk has changed underneath the review sends a person to sudo instead of to a re-read. It runs
# just below, still before anything destructive.

# --- The reviewed operation plan -----------------------------------------------------------------
#
# One sequence, walked identically whether it is being printed or performed: `run` is the single
# gate, so an operation that a dry run did not record is an operation an apply cannot perform.
# Every call is appended to plan.json as it goes past, with the step it belongs to and the exact
# argv — the thing a person reviews, and the thing `--apply --plan` re-runs.
#
# The `observed` block is the state the plan was made against: the partition table, the filesystem
# identities and what is mounted. `--apply --plan` observes the machine again and refuses on any
# difference, because a plan reviewed against one disk must not be applied to another, and a
# partition that has been mounted since is a partition somebody is using.

plan_out="${plan_out:-}"
if [ -z "$plan_out" ]; then
    case "$target_kind" in
        image) plan_out="$target.plan.json" ;;
        *) plan_out="${TMPDIR:-/tmp}/legeos-provision-plan.json" ;;
    esac
fi
plan_ops=""

# JSON string escaping for a shell that has no json library. Backslash first, then quote.
json_escape() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

# What this machine looks like right now, as sorted `key=value` lines. Sorted so the comparison is
# about content and never about the order two tools happened to list things in.
observe() {
    {
        # Topology: the partition table of every disk this run names.
        for device in "$target" "$esp_arg" "$root_arg" "$data_arg"; do
            [ -n "$device" ] || continue
            [ -e "$device" ] || { printf 'topology:%s=absent\n' "$device"; continue; }
            local disk="$device"
            if [ -b "$device" ] && [ -n "$(lsblk -no PKNAME "$device" 2>/dev/null)" ]; then
                disk="/dev/$(lsblk -no PKNAME "$device")"
            fi
            printf 'topology:%s=%s\n' "$disk" \
                "$(sgdisk -p "$disk" 2>/dev/null | sed -n 's/^ *\([0-9]\+\) *\([0-9]\+\) *\([0-9]\+\).*/\1:\2:\3/p' | tr '\n' ',')"
        done
        # Identity: the UUID and label of every filesystem this run names.
        for part in "$esp_arg" "$root_arg" "$data_arg"; do
            [ -n "$part" ] || continue
            printf 'identity:%s=%s/%s/%s\n' "$part" \
                "$(blkid -s UUID -o value "$part" 2>/dev/null)" \
                "$(blkid -s LABEL -o value "$part" 2>/dev/null)" \
                "$(blkid -s TYPE -o value "$part" 2>/dev/null)"
            printf 'mounted:%s=%s\n' "$part" \
                "$(findmnt -no TARGET -S "$part" 2>/dev/null | tr '\n' ',')"
        done
    } | LC_ALL=C sort
}

# --- Plan / apply --------------------------------------------------------------------------------
step() { step_no=$((step_no + 1)); printf '\n%2d. %s\n' "$step_no" "$*"; current_step="$*"; }
current_step="start"
# What kind of thing an operation is, worked out from the command itself rather than restated at
# every call site — so a step added later is classified without anyone having to remember to.
# `create`, `format` and `reuse` are the three the review is actually about: they are the ones
# that decide whether an existing filesystem survives.
classify() {
    case "$1" in
        sgdisk|truncate|losetup|partx) printf 'create' ;;
        mkfs.*) printf 'format' ;;
        mount|umount) printf 'mount' ;;
        grub-install|update-grub|efibootmgr) printf 'grub' ;;
        __reuse__) printf 'reuse' ;;
        *) printf 'configure' ;;
    esac
}

# Record a filesystem this run deliberately leaves alone. It performs nothing — that is the point
# — but a plan that shows only the things it *does* cannot be reviewed for what it preserves.
reused() { # part role type
    record __reuse__ "$1" "$2" "$3"
}
record() { # argv...
    local op argv="" program="$1"
    # `in_target` runs everything through `chroot <mnt>`, so the command that decides what kind of
    # operation this is sits two words in. Classifying `chroot` itself would file every step
    # performed inside the target under one meaningless heading.
    [ "$program" = chroot ] && program="${3:-chroot}"
    op="$(classify "$program")"
    for word in "$@"; do argv="$argv$([ -n "$argv" ] && printf ', ')\"$(json_escape "$word")\""; done
    plan_ops="$plan_ops$([ -n "$plan_ops" ] && printf ',')
    {\"step\": $step_no, \"description\": \"$(json_escape "$current_step")\", \"op\": \"$op\", \"target\": \"$(json_escape "$target")\", \"argv\": [$argv]}"
}
run() {
    record "$@"
    if [ "$apply" = 1 ]; then
        "$@"
    else
        printf '     $ %s\n' "$*"
    fi
}
in_target() { run chroot "$mnt" "$@"; }

write_plan() {
    {
        printf '{\n  \"version\": 1,\n'
        printf '  \"target\": \"%s\",\n  \"target_kind\": \"%s\",\n' "$(json_escape "$target")" "$target_kind"
        printf '  \"deployment\": \"%s\",\n  \"cmdline\": \"%s\",\n' "$deployment_id" "$(json_escape "$cmdline")"
        printf '  \"reuse\": %s,\n  \"format_data\": %s,\n' \
            "$([ "$reuse" = 1 ] && echo true || echo false)" \
            "$([ "$format_data" = 1 ] && echo true || echo false)"
        printf '  \"observed\": ['
        local first=1
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            [ $first -eq 1 ] || printf ','
            first=0
            printf '\n    \"%s\"' "$(json_escape "$line")"
        done <<< "$observed"
        printf '\n  ],\n'
        printf '  \"operations\": [%s\n  ]\n}\n' "$plan_ops"
    } > "$plan_out"
}

observed="$(observe)"

# `--apply --plan` is the only path that mutates something a person has already reviewed. Observe
# the machine again and refuse on any difference: a plan made against one disk must not be applied
# to another, and a partition mounted since the review is a partition somebody is using.
if [ -n "$plan_in" ]; then
    [ "$apply" = 1 ] || die "--plan is for --apply; a dry run makes a plan rather than consuming one"
    [ -f "$plan_in" ] || die "no plan at $plan_in"
    reviewed="$(sed -n '/"observed": \[/,/\]/p' "$plan_in" | sed -n 's/^ *"\(.*\)",\?$/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')"
    if [ "$reviewed" != "$observed" ]; then
        printf 'provision: the machine is not what this plan was reviewed against.\n\n' >&2
        diff <(printf '%s\n' "$reviewed") <(printf '%s\n' "$observed") \
            | sed 's/^/    /' >&2 || true
        die "refusing to apply. Re-run the dry run, read the new plan, and apply that one."
    fi
    note "the machine still matches the plan reviewed in $plan_in"
fi

# Everything above this line reads. Everything below it can write, so this is where privilege is
# required — after the plan has been checked and before the first `run` can perform anything.
if [ "$apply" = 1 ] && [ "$(id -u)" != 0 ]; then
    die "--apply needs root (loop devices, mkfs, mount, chroot). Run it in a disposable VM or with sudo, never on a machine you are using."
fi

mnt="${LEGEOS_PROVISION_MOUNT:-/mnt/legeos-provision}"

printf 'LegeOS Developer Edition provisioner\n'
printf '  layout      %s\n' "$layout"
printf '  target      %s (%s)\n' "$target" "$target_kind"
printf '  mode        %s\n' "$([ "$apply" = 1 ] && echo APPLY || echo 'dry run — nothing is written')"
printf '  deployment  %s\n' "$deployment_id"
printf '  cmdline     %s\n' "$cmdline"

# The partition node for number $2 on disk $1: nvme0n1p3, mmcblk0p3, but sda3.
part_node() {
    case "$1" in *[0-9]) printf '%sp%s' "$1" "$2" ;; *) printf '%s%s' "$1" "$2" ;; esac
}
attach_loop() { # a regular file gets a loop device; the plan shows the placeholder
    if [ "$apply" = 1 ]; then
        loop="$(losetup --find --partscan --show "$target")"
    else
        note "the file is attached with losetup --partscan; its partitions are \${loop}p<n>"
        run losetup --find --partscan --show "$target"
        loop='${loop}'
    fi
}

loop=""
case "$target_kind" in
image|device)
    step "Partition $target: GPT, ESP ($ESP_FORMAT, up to $ESP_SIZE_MAX, label $ESP_LABEL) + root ($ROOT_FORMAT, label $ROOT_LABEL)"
    if [ "$target_kind" = image ] && [ ! -e "$target" ]; then
        run truncate -s "${LEGEOS_IMAGE_SIZE:-32G}" "$target"
    fi
    run sgdisk --zap-all "$target"
    run sgdisk --new=1:0:+"${ESP_SIZE_MAX}" --typecode=1:ef00 --change-name=1:LEGEOS_ESP "$target"
    run sgdisk --new=2:0:0 --typecode=2:8304 --change-name=2:legeos-root "$target"
    if [ "$target_kind" = image ]; then
        attach_loop
        esp_part="${loop}p1"; root_part="${loop}p2"
    else
        esp_part="$(part_node "$target" 1)"; root_part="$(part_node "$target" 2)"
    fi
    ;;
free-space)
    step "Partition the free tail of $target: ESP ($ESP_FORMAT, $ESP_SIZE_MAX, label $ESP_LABEL) + root ($ROOT_FORMAT, the rest, label $ROOT_LABEL); every existing partition stays"
    table="$(sgdisk -p "$target" 2>/dev/null || true)"
    [ -n "$table" ] || die "cannot read the partition table of $target (run as root, or is it not GPT?)"
    sector="$(printf '%s\n' "$table" | sed -n 's/^Sector size (logical\/physical): \([0-9]*\)\/.*/\1/p')"
    last_usable="$(sgdisk -E "$target")"
    free_first="$(sgdisk -F "$target")"
    # sgdisk -F is the first sector of the *largest* free block. It is the tail only if no
    # partition starts at or after it; anything else means the space is a hole, not a tail.
    existing="$(printf '%s\n' "$table" | awk '$1 ~ /^[0-9]+$/ { print $1, $2, $3, $6, $7, $8 }')"
    printf '%s\n' "$existing" | awk -v first="$free_first" '$2 >= first { exit 1 }' \
        || die "the largest free block of $target is a hole between partitions, not the tail; --free-space only extends the end of the disk"
    free_bytes=$(( (last_usable - free_first + 1) * ${sector:-512} ))
    need_bytes=$(( $(numfmt --from=iec "$ESP_SIZE_MAX") + $(numfmt --from=iec "$(layout_value root size_min)") ))
    [ "$free_bytes" -ge "$need_bytes" ] \
        || die "$target has $(numfmt --to=iec "$free_bytes") free at its tail; the layout needs at least $(numfmt --to=iec "$need_bytes") (ESP $ESP_SIZE_MAX + root $(layout_value root size_min))"
    highest="$(printf '%s\n' "$existing" | awk 'BEGIN { n = 0 } $1 > n { n = $1 } END { print n }')"
    n_esp=$((highest + 1)); n_root=$((highest + 2))
    note "free tail: sectors $free_first..$last_usable ($(numfmt --to=iec "$free_bytes")); new partitions $n_esp (ESP) and $n_root (root)"
    note "kept untouched:"
    printf '%s\n' "$existing" | while read -r num start end _ _ _; do
        [ -n "$num" ] && note "  partition $num (sectors $start..$end)"
    done
    run sgdisk --new="$n_esp:0:+${ESP_SIZE_MAX}" --typecode="$n_esp:ef00" --change-name="$n_esp:LEGEOS_ESP" "$target"
    run sgdisk --new="$n_root:0:0" --typecode="$n_root:8304" --change-name="$n_root:legeos-root" "$target"
    if [ -b "$target" ]; then
        run partx --update "$target"
        esp_part="$(part_node "$target" "$n_esp")"; root_part="$(part_node "$target" "$n_root")"
    else
        attach_loop
        esp_part="${loop}p${n_esp}"; root_part="${loop}p${n_root}"
    fi
    ;;
partitions)
    esp_part="$esp_arg"; root_part="$root_arg"
    if [ "$reuse" = 1 ]; then
        # Preservation means preservation. The type code is metadata another operating system may
        # depend on, and rewriting it is a change to the disk however harmless it looks — which is
        # why @legeos.decision.independent-storage-roles says the current mode "is not
        # preservation".
        step "Preserve the existing partitions $esp_arg (ESP) and $root_arg (root): no type code is rewritten and no filesystem is made"
        for pair in "ESP:$esp_arg:vfat" "root:$root_arg:btrfs"; do
            role="${pair%%:*}"; rest="${pair#*:}"; part="${rest%%:*}"; want="${rest##*:}"
            found="$(blkid -s TYPE -o value "$part" 2>/dev/null || true)"
            [ -n "$found" ] || die "--reuse needs an existing filesystem on $part for the $role role, and there is none. Drop --reuse to make one."
            [ "$found" = "$want" ] || die "$part holds a $found filesystem; the $role role needs $want. Drop --reuse to reformat it, deliberately."
            reused "$part" "$role" "$found"
            note "  $role: reusing the $found filesystem already on $part"
        done
    else
        step "Use the existing partitions $esp_arg (ESP) and $root_arg (root); the partition table is not touched"
        run sgdisk --typecode="$(lsblk -no PARTN "$esp_arg" 2>/dev/null || echo N):ef00" "/dev/$(lsblk -no PKNAME "$esp_arg" 2>/dev/null || echo DISK)"
        run sgdisk --typecode="$(lsblk -no PARTN "$root_arg" 2>/dev/null || echo N):8304" "/dev/$(lsblk -no PKNAME "$root_arg" 2>/dev/null || echo DISK)"
    fi
    ;;
esac

if [ "$reuse" = 1 ]; then
    note "--reuse: no mkfs runs. The subvolume creation below is additive and leaves existing ones alone."
else
    step "Make filesystems"
    run mkfs.vfat -F32 -n "$ESP_LABEL" "$esp_part"
    run mkfs.btrfs -f -L "$ROOT_LABEL" "$root_part"
fi

if [ -n "$data_arg" ]; then
    if [ "$format_data" = 1 ]; then
        step "Make the data filesystem on $data_arg ($DATA_FORMAT, label $DATA_LABEL) — you asked for this with --format-data"
        run "mkfs.$DATA_FORMAT" -f -L "$DATA_LABEL" "$data_arg"
    else
        step "Reuse the data filesystem on $data_arg; it is never assumed expendable"
        found="$(blkid -s TYPE -o value "$data_arg" 2>/dev/null || true)"
        [ -n "$found" ] || die "$data_arg holds no filesystem. Add --format-data to make one, deliberately."
        reused "$data_arg" data "$found"
        note "  reusing the $found filesystem already on $data_arg (--format-data would replace it)"
    fi
fi

step "Create the declared subvolumes: $SUBVOLUMES"
run mount "$root_part" "$mnt"
for subvol in $SUBVOLUMES; do
    run btrfs subvolume create "$mnt/${subvol#/}"
done
run umount "$mnt"

step "Mount the target at $mnt"
run mount -o "subvol=@,$(layout_value root mount_options)" "$root_part" "$mnt"
for subvol in $SUBVOLUMES; do
    case "$subvol" in
        /@) continue ;;
        /@home) dir=home ;;
        /@var) dir=var ;;
        /@snapshots) dir=.snapshots ;;
        /@deployments) dir=.deployments ;;
        *) dir="${subvol#/@}" ;;
    esac
    run mkdir -p "$mnt/$dir"
    run mount -o "subvol=${subvol#/}" "$root_part" "$mnt/$dir"
done
run mkdir -p "$mnt$ESP_MOUNT"
run mount "$esp_part" "$mnt$ESP_MOUNT"
if [ -n "$data_arg" ]; then
    run mkdir -p "$mnt$DATA_MOUNT"
    run mount ${DATA_MOUNT_OPTIONS:+-o "$DATA_MOUNT_OPTIONS"} "$data_arg" "$mnt$DATA_MOUNT"
fi

# --- Identity: /etc/fstab and /etc/legeos/install.toml, both by UUID ----------------------------
#
# Two things were missing here and they are the same thing. There was no /etc/fstab at all — the
# mounts above are made for the chroot and were never written down, so a provisioned machine had
# nothing telling it how to mount @home, @var or the ESP on its own. And identity was by label:
# `root=LABEL=legeos-root` is the same string on every LegeOS disk, so a machine with two of them
# attached mounts whichever the kernel enumerated first.
#
# A UUID is per filesystem and is minted by mkfs, so it cannot collide. install.toml is where
# lege-hostd reads it back (`boot_entries.rs`), and it falls back to the label when the file is
# absent — which is every machine provisioned before this and every mkosi image.
step "Filesystem identity: /etc/fstab and /etc/legeos/install.toml, both by UUID"
# `|| true` throughout: in a dry run the partitions do not exist yet, and blkid failing to read a
# device that has not been created is the expected case, not an error.
uuid_of() { blkid -s UUID -o value "$1" 2>/dev/null || true; }
root_uuid="$(uuid_of "$root_part")"
esp_partuuid="$(blkid -s PARTUUID -o value "$esp_part" 2>/dev/null || true)"
data_uuid=""
[ -n "$data_arg" ] && data_uuid="$(uuid_of "$data_arg")" || true
if [ "$apply" = 1 ]; then
    [ -n "$root_uuid" ] || die "the root filesystem has no UUID; blkid could not read $root_part"
fi
note "root=UUID=${root_uuid:-<read at apply>}  esp PARTUUID=${esp_partuuid:-<read at apply>}"
note "writing $mnt/etc/fstab"
if [ "$apply" = 1 ]; then
    mkdir -p "$mnt/etc"
    {
        printf '# GENERATED by installer/provision.sh. Identity is by UUID, never by label:\n'
        printf '# every LegeOS root carries the same label, and a machine with two of them\n'
        printf '# attached would mount whichever the kernel saw first.\n'
        printf 'UUID=%s  /  %s  %s,subvol=@  0 1\n' "$root_uuid" "$ROOT_FORMAT" "$(layout_value root mount_options)"
        for subvol in $SUBVOLUMES; do
            case "$subvol" in
                /@) continue ;;
                /@home) dir=/home ;;
                /@var) dir=/var ;;
                /@snapshots) dir=/.snapshots ;;
                /@deployments) dir=/.deployments ;;
                *) dir="/${subvol#/@}" ;;
            esac
            printf 'UUID=%s  %s  %s  %s,subvol=%s  0 0\n' \
                "$root_uuid" "$dir" "$ROOT_FORMAT" "$(layout_value root mount_options)" "${subvol#/}"
        done
        printf 'PARTUUID=%s  %s  vfat  umask=0077  0 2\n' "$esp_partuuid" "$ESP_MOUNT"
        [ -n "$data_uuid" ] && printf 'UUID=%s  %s  %s  %s  0 2\n' \
            "$data_uuid" "$DATA_MOUNT" "$DATA_FORMAT" "${DATA_MOUNT_OPTIONS:-defaults}"
    } > "$mnt/etc/fstab"
fi
note "writing $mnt/etc/legeos/install.toml — what lege-hostd reads to build a boot entry"
if [ "$apply" = 1 ]; then
    mkdir -p "$mnt/etc/legeos"
    {
        printf '# GENERATED by installer/provision.sh. Read by services/lege-hostd/src/boot_entries.rs;\n'
        printf '# absent means fall back to LABEL=%s, which is what every earlier install has.\n' "$ROOT_LABEL"
        printf 'root_uuid = "%s"\n' "$root_uuid"
        printf 'esp_partuuid = "%s"\n' "$esp_partuuid"
        [ -n "$data_uuid" ] && printf 'data_uuid = "%s"\n' "$data_uuid"
    } > "$mnt/etc/legeos/install.toml"
fi
# The command line follows the same identity. Everything downstream — the GRUB entry seeded below
# and every one lege-hostd writes after it — reads it from install.toml.
if [ -n "$root_uuid" ]; then
    cmdline="${cmdline/root=LABEL=$ROOT_LABEL/root=UUID=$root_uuid}"
    note "boot command line: $cmdline"
fi

step "Lend the target the host's /dev, /proc, /sys and DNS for the chroot steps"
for dir in dev dev/pts proc sys sys/firmware/efi/efivars; do
    run mkdir -p "$mnt/$dir"
    run mount --bind "/$dir" "$mnt/$dir"
done
run install -D -m 0644 /etc/resolv.conf "$mnt/etc/resolv.conf"

step "Debootstrap Ubuntu $UBUNTU_SUITE ($UBUNTU_COMPONENTS) from $UBUNTU_MIRROR"
if command -v mmdebstrap >/dev/null 2>&1; then
    run mmdebstrap --components="${UBUNTU_COMPONENTS// /,}" "$UBUNTU_SUITE" "$mnt" "$UBUNTU_MIRROR"
else
    note "mmdebstrap is absent; falling back to debootstrap"
    run debootstrap --components="${UBUNTU_COMPONENTS// /,}" "$UBUNTU_SUITE" "$mnt" "$UBUNTU_MIRROR"
fi

step "Sources: Canonical archive plus the LegeOS archive ($DEVREPO_ROOT, marker $DEVREPO_MARKER)"
note "the LegeOS archive is signed; the target trusts it through its key and nothing else"
run lege dev --repo "$DEVREPO_ROOT" repo verify --set "$DEVREPO_MARKER"
note "the whole archive is copied to $TARGET_DEVREPO on the target: the set it installs from is"
note "the set its own \`lege dev status\` will report, and \`lege dev rollback\` has something to go to"
run mkdir -p "$mnt$(dirname "$TARGET_DEVREPO")"
run cp -a "$DEVREPO_ROOT" "$mnt$TARGET_DEVREPO"
run install -D -m 0644 "$DEVREPO_ROOT/legeos-dev.gpg" \
    "$mnt/usr/share/keyrings/legeos-archive-keyring.gpg"
note "writing $mnt/etc/apt/sources.list.d/legeos.sources"
if [ "$apply" = 1 ]; then
    cat > "$mnt/etc/apt/sources.list.d/legeos.sources" <<EOF
Types: deb
URIs: file://$TARGET_DEVREPO/sets/$DEVREPO_MARKER
Suites: ./
Signed-By: /usr/share/keyrings/legeos-archive-keyring.gpg
EOF
fi

step "Install the LegeOS package set (contract C1)"
in_target apt-get update
in_target apt-get install -y "$LEGEOS_KEYRING" "$LEGEOS_SESSION" "$LEGEOS_DESKTOP" \
    "$LEGEOS_DEV_RUST" "$LEGEOS_DEV_CUDA"

step "Kernel and NVIDIA modules: Ubuntu-signed only, previous kernel retained"
note "$LEGEOS_KERNEL pulls Ubuntu's supported GA/HWE kernel; $LEGEOS_NVIDIA pulls the matching"
note "Canonical-signed linux-modules-nvidia LRM. No DKMS, no self-built module, no MOK."
in_target apt-get install -y "$LEGEOS_KERNEL" "$LEGEOS_NVIDIA"
note "keeping the previous kernel installable and selectable is policy, not luck:"
if [ "$apply" = 1 ]; then
    printf 'APT::NeverAutoRemove { "^linux-image-.*"; "^linux-modules-nvidia-.*"; };\n' \
        > "$mnt/etc/apt/apt.conf.d/01legeos-keep-kernels"
fi

step "OS identity"
run install -D -m 0644 "$data_os_release" "$mnt/usr/lib/os-release"

step "Account: ${username:-<--user was not given>}"
[ -n "$username" ] || note "no --user given; the plan stops short of creating an account"
if [ -n "$username" ]; then
    in_target useradd --create-home --shell /bin/bash --groups sudo,video,render "$username"
    if [ "${read_password:-0}" = 1 ]; then
        note "reading the new password from stdin: never echoed, never written to a file, and"
        note "never part of an argument list another process could read out of /proc."
        if [ "$apply" = 1 ]; then
            IFS= read -r new_password
            printf '%s:%s\n' "$username" "$new_password" | chroot "$mnt" chpasswd
            unset new_password
        fi
    else
        note "no password given: the account is created with an expired password and the first"
        note "login must set one. Nothing here bakes in a release password."
        in_target passwd --expire "$username"
    fi
fi
note "root is locked either way: this machine is administered through sudo, not a root login."
in_target passwd --lock root

step "Session units and greeter (the same steps distro/image/mkosi.postinst performs in the image)"
in_target systemctl --global mask xdg-desktop-autostart.target
in_target systemctl --global enable legeos-session.target
in_target systemctl enable lege-hostd.service lege-boot-health.service
in_target install -D -m 0644 /usr/share/legeos/greetd/config.toml /etc/greetd/config.toml
in_target systemd-sysusers /usr/lib/sysusers.d/legeos-greeter.conf
in_target systemd-tmpfiles --create /usr/lib/tmpfiles.d/legeos-greeter.conf
in_target systemctl disable getty@tty1.service
in_target systemctl enable greetd.service

step "Flatpak applications (ADR-0011): the browser is a Flatpak, so apt installs nothing for it"
flatpak_refs="$data_flatpak_refs"
if [ ! -f "$flatpak_refs" ]; then
    note "no $flatpak_refs — run 'cargo xtask seeds' to generate it. Nothing is seeded."
else
    in_target flatpak remote-add --if-not-exists --system \
        flathub https://dl.flathub.org/repo/flathub.flatpakrepo
    # `<remote> <ref>` per line, comments and blanks ignored (the format D1's generator writes).
    while read -r remote ref; do
        [ -n "${ref:-}" ] || continue
        in_target flatpak install -y --noninteractive --system "$remote" "$ref"
    done < <(sed 's/#.*//' "$flatpak_refs")
fi

step "Boot path: Canonical-signed shim -> Canonical-signed GRUB -> Ubuntu-signed kernel (ADR-0004)"
in_target apt-get install -y shim-signed grub-efi-amd64-signed
note "--no-nvram: the firmware entry is registered below, on the host side, and appended rather"
note "than put first. The firmware menu is how the first boots happen; making LegeOS the default"
note "is a person's decision after installer/CHECKLIST.md passes (efibootmgr -o)."
in_target grub-install --target=x86_64-efi --efi-directory="$ESP_MOUNT" \
    --bootloader-id=LegeOS --uefi-secure-boot --no-nvram
if [ "$target_kind" = image ] || [ -n "$loop" ]; then
    note "no firmware entry for a file: the QEMU harness boots it through OVMF's menu (EFI/LegeOS/shimx64.efi)"
else
    esp_disk="/dev/$(lsblk -no PKNAME "$esp_part" 2>/dev/null || echo DISK)"
    esp_partn="$(lsblk -no PARTN "$esp_part" 2>/dev/null || echo N)"
    run efibootmgr --create-only --disk "$esp_disk" --part "$esp_partn" --label LegeOS \
        --loader '\EFI\LegeOS\shimx64.efi'
    if [ "$apply" = 1 ]; then
        entry="$(efibootmgr | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\)\*\{0,1\} LegeOS.*/\1/p' | tail -1)"
        order="$(efibootmgr | sed -n 's/^BootOrder: //p')"
        [ -n "$entry" ] || die "efibootmgr created no LegeOS entry"
        case ",$order," in *",$entry,"*) ;; *) efibootmgr --bootorder "${order:+$order,}$entry" >/dev/null ;; esac
        note "firmware entry Boot$entry appended to the boot order: $order,$entry"
    else
        note "then: efibootmgr --bootorder <current order>,<the new LegeOS entry> — appended, not first"
    fi
fi
note "GRUB_DEFAULT=$GRUB_DEFAULT_MODE is what lets hostd select a deployment by name"
if [ "$apply" = 1 ]; then
    sed -i "s/^GRUB_DEFAULT=.*/GRUB_DEFAULT=$GRUB_DEFAULT_MODE/" "$mnt/etc/default/grub"
    grep -q "^GRUB_DEFAULT=$GRUB_DEFAULT_MODE" "$mnt/etc/default/grub" \
        || printf 'GRUB_DEFAULT=%s\n' "$GRUB_DEFAULT_MODE" >> "$mnt/etc/default/grub"
fi
run install -D -m 0755 "$data_grub_stub" "$mnt$GRUB_STUB"

step "Seed deployment $deployment_id"
note "kernel and initramfs are copied into the ESP directory hostd owns, so Ubuntu's initramfs"
note "hooks cannot rewrite this deployment's boot files: $esp_dir"
run mkdir -p "$mnt$ESP_MOUNT$esp_dir"
run sh -c "cp \"$mnt\"/boot/vmlinuz-* \"$mnt$ESP_MOUNT$esp_dir/vmlinuz\""
run sh -c "cp \"$mnt\"/boot/initrd.img-* \"$mnt$ESP_MOUNT$esp_dir/initrd.img\""
note "record it as known-good so hostd's first staged candidate does not replace the only entry"
run mkdir -p "$mnt/var/lib/legeos/hostd/grub"
note "writing $GRUB_ENTRIES, in the exact shape lege-hostd regenerates it:"
note "    menuentry 'LegeOS ($deployment_id) — known good' --id '$deployment_id'"
note "    linux $esp_dir/vmlinuz $cmdline"
if [ "$apply" = 1 ]; then
    printf '%s\n' "$deployment_id" > "$mnt/var/lib/legeos/hostd/grub/known-good"
    mkdir -p "$mnt$(dirname "$GRUB_ENTRIES")"
    cat > "$mnt$GRUB_ENTRIES" <<EOF
# GENERATED by installer/provision.sh — lege-hostd rewrites this file from here on.
menuentry 'LegeOS ($deployment_id) — known good' --id '$deployment_id' {
    search --no-floppy --set=root --file $esp_dir/vmlinuz
    linux $esp_dir/vmlinuz $cmdline
    initrd $esp_dir/initrd.img
}
EOF
fi
in_target update-grub

step "Diagnostics"
run lege doctor --root "$mnt"
note "then, after the first real boot: lege doctor --hardware, and installer/CHECKLIST.md for the"
note "Secure Boot, module-trust and rollback checks that only a person at the machine can make."

if [ "$apply" = 1 ]; then
    step "Unmount"
    run umount -R "$mnt"
    if [ -n "$loop" ]; then
        run losetup -d "$loop"
    fi
    write_plan
    printf '\nprovisioned %s as deployment %s\n' "$target" "$deployment_id"
    printf 'the sequence that was performed: %s\n' "$plan_out"
else
    write_plan
    printf '\nplan only — %s was not touched. Add --apply to run it.\n' "$target"
    printf 'reviewed sequence written to %s\n' "$plan_out"
    printf 'apply exactly this one with: %s --apply --plan %s <the same target arguments>\n' \
        "$0" "$plan_out"
fi
