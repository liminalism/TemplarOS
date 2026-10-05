#!/usr/bin/env bash
# Provision a TemplarOS Developer Edition target from an Ubuntu 26.04 base.
#
# This is a *provisioner*, not an installer: one machine shape, no OEM flow, no arbitrary layouts,
# no graphical pages. It reads distro/deploy/layout.toml (contract C3) so the partition table, the
# subvolumes and the deployment identity are declared once and not restated here. Four ways to
# name the target, one layout: a disk image, a whole disk (zapped), the largest free block of a disk that
# keeps every partition it has (`--free-space`, the reference laptop's case), or two existing
# partitions (`--esp --root`).
#
# It plans by default. Nothing is written until --apply, and then only to the target you name.
# Read `installer/README.md` before running it against anything you care about, and
# `installer/CHECKLIST.md` for the parts a person has to check by hand on real hardware.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
# templaros-cli ships this script as /usr/libexec/templaros/provision with its data under
# /usr/share/templaros, so the live medium can run it with no checkout. A checkout wins when present.
if [ -f "$repo/distro/deploy/layout.toml" ]; then
    data_layout="$repo/distro/deploy/layout.toml"
    data_grub_stub="$repo/distro/deploy/grub.d/09_templaros"
    data_os_release="$repo/distro/branding/os-release"
    data_flatpak_refs="$repo/distro/seeds/flatpak-refs.list"
    data_base="$repo/distro/base.toml"
else
    # The live medium is itself a TemplarOS: templaros-hostd ships the GRUB stub and the identity is
    # the running system's own, so only the layout needs a home of its own (templaros-cli).
    data_layout=/usr/share/templaros/layout.toml
    data_grub_stub=/etc/grub.d/09_templaros
    data_os_release=/usr/lib/os-release
    data_flatpak_refs=/usr/share/templaros/flatpak-refs.list
    data_base=/usr/share/templaros/base.toml
fi

# --- C1: package selection (D1 owns these names; they are variables so a rename is one edit) -----
TEMPLAROS_KEYRING="${TEMPLAROS_KEYRING:-templaros-archive-keyring}"
TEMPLAROS_DESKTOP="${TEMPLAROS_DESKTOP:-templaros-desktop}"
TEMPLAROS_SESSION="${TEMPLAROS_SESSION:-templaros-session-full}"     # built from distro/metapackages/templaros-session-meta
TEMPLAROS_KERNEL="${TEMPLAROS_KERNEL:-templaros-kernel}"
TEMPLAROS_NVIDIA="${TEMPLAROS_NVIDIA:-templaros-nvidia-open}"        # or templaros-nvidia-proprietary
TEMPLAROS_DEV_RUST="${TEMPLAROS_DEV_RUST:-templaros-dev-rust}"
TEMPLAROS_DEV_CUDA="${TEMPLAROS_DEV_CUDA:-templaros-dev-cuda}"
# In the set but in no metapackage; `templar doctor`'s build-set-coherent check fails without it.
TEMPLAROS_FILES="${TEMPLAROS_FILES:-templar-files}"

# --- C2: the local TemplarOS archive (D2 owns this shape) -------------------------------------------
# Where the set is read from: the live medium carries one at /templaros/devrepo; a development host
# has its own. Where it ends up is always the target's /var/lib/templaros/devrepo, so the installed
# system's `templar dev` finds the set it was installed from and reports itself converged.
if [ -z "${DEVREPO_ROOT:-}" ]; then
    DEVREPO_ROOT=/var/lib/templaros/devrepo
    for medium in /cdrom/templaros/devrepo /templaros/devrepo; do   # casper mounts the ISO at /cdrom
        [ -d "$medium" ] && { DEVREPO_ROOT="$medium"; break; }
    done
fi
TARGET_DEVREPO=/var/lib/templaros/devrepo
DEVREPO_MARKER="${DEVREPO_MARKER:-current}"   # current | previous. `staged` is never installable.

# --- Ubuntu base ---------------------------------------------------------------------------------
UBUNTU_SUITE="${UBUNTU_SUITE:-resolute}"
# Ubuntu comes from the snapshot the TemplarOS set was resolved against (distro/base.toml), not the
# moving archive: the set's metapackages pin the kernel's NVIDIA modules and userspace to exact
# versions, and the archive drops those the day Ubuntu publishes the next kernel. (2026-09-30: the
# set pinned 7.0.0-31 and 580.173, the archive had moved to 7.0.0-34 and 580.178, and the install
# stopped at the NVIDIA step.) The installed system keeps the same sources, so what it upgrades to
# is what the set was tested against; a newer base arrives with a newer set. No pin means the archive.
base_snapshot=""
base_components=""
if [ -f "$data_base" ]; then
    base_snapshot="$(sed -n 's/^snapshot_url *= *"\([^"]*\)".*/\1/p' "$data_base" | head -1)"
    base_components="$(sed -n 's/^components *= *\[\([^]]*\)\].*/\1/p' "$data_base" | head -1 | tr -d '",')"
fi
UBUNTU_MIRROR="${UBUNTU_MIRROR:-${base_snapshot:-http://archive.ubuntu.com/ubuntu}}"
UBUNTU_SECURITY_MIRROR="${UBUNTU_SECURITY_MIRROR:-${base_snapshot:-http://security.ubuntu.com/ubuntu}}"
# distro/base.toml names them too: multiverse is there because nvidia-cuda-toolkit, which
# templaros-dev-cuda depends on, is in no other component.
UBUNTU_COMPONENTS="${UBUNTU_COMPONENTS:-$(printf '%s' "${base_components:-main universe restricted multiverse}" | xargs)}"
bootstrap_backend="${TEMPLAROS_BOOTSTRAP_BACKEND:-auto}"
case "$bootstrap_backend" in
    auto) command -v mmdebstrap >/dev/null 2>&1 && bootstrap_backend=mmdebstrap || bootstrap_backend=debootstrap ;;
    mmdebstrap|debootstrap) ;;
    *) die "TEMPLAROS_BOOTSTRAP_BACKEND must be auto, mmdebstrap or debootstrap" ;;
esac

layout="$data_layout"
target=""
target_kind=""          # image | device | free-space | partitions
esp_arg=""
root_arg=""
username=""
read_password=0
apply=0
i_mean_it=0
step_no=0
data_arg=""
format_data=0
reuse=0
resume=0
inspect=0
plan_out=""
plan_in=""
ignore_preflight=0
# The live medium leaves the boot-time hardware report here (distro/iso/live/customize.sh).
PREFLIGHT_REPORT="${PREFLIGHT_REPORT:-/run/templaros/preflight.txt}"


die() { printf 'provision: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }

block_type() { lsblk -dnro TYPE -- "$1" 2>/dev/null | head -1; }
canonical_device() { readlink -f -- "$1" 2>/dev/null || printf '%s' "$1"; }
# `|| true`: in a dry run the partitions may not exist yet, and blkid failing to read a device that
# has not been created is the expected case, not an error.
uuid_of() { blkid -s UUID -o value "$1" 2>/dev/null || true; }
partuuid_of() { blkid -s PARTUUID -o value "$1" 2>/dev/null || true; }
parent_disk() {
    local parent
    parent="$(lsblk -dnro PKNAME -- "$1" 2>/dev/null | head -1)"
    [ -n "$parent" ] && printf '/dev/%s' "$parent" || canonical_device "$1"
}

# Test seam for the safety-critical lsblk query. It exits before argument parsing or any target
# observation, and lets integration tests inject a topology-only lsblk fixture without devices.
if [ -n "${TEMPLAROS_PROVISION_TEST_BLOCK_TYPES:-}" ]; then
    for test_device in $TEMPLAROS_PROVISION_TEST_BLOCK_TYPES; do
        printf '%s=%s\n' "$test_device" "$(block_type "$test_device")"
    done
    exit 0
fi

usage() {
    cat <<'USAGE'
usage: provision.sh --image PATH [--apply] [options]
       provision.sh --free-space /dev/sdX [--apply] [options]
       provision.sh --esp /dev/sdXn --root /dev/sdXm [--apply] [options]
       provision.sh --device /dev/sdX --i-mean-it --apply [options]

  --image PATH        provision a disk image file (created if absent). The safe target.
  --free-space DISK   create the ESP and the root in the largest unallocated block of DISK,
                      wherever on the disk it is, and touch no
                      existing partition. The disk keeps whatever else it boots.
  --esp PART --root PART
                      format these two existing partitions; the partition table is not touched.
  --device /dev/sdX   provision a whole block device. Requires --i-mean-it as well.
  --i-mean-it         acknowledge that --device destroys everything on that disk.
  --archive DIR       the TemplarOS archive to install from (default: /templaros/devrepo on the live
                      medium, /var/lib/templaros/devrepo otherwise). Copied to the target.
  --apply             actually do it. Without this, the run prints the plan and changes nothing.
  --user NAME         the account to create. No password is baked in; see --password-stdin.
  --password-stdin    read the new user's password from stdin during apply. Required with --apply;
                      the password is never put in argv, the plan, or a file.
                      There is no "change it at first login": greetd cannot run PAM's password
                      change, so an expired password locks the account out of the greeter.
  --nvidia PACKAGE    the NVIDIA metapackage variant (default: $TEMPLAROS_NVIDIA).
  --set ID            install a specific TemplarOS build set instead of the `current` marker.
  --layout PATH       an alternative layout declaration (testing).
  --data PART         an independent data filesystem (layout [data]). Reused, never formatted,
                      unless --format-data says otherwise.
  --format-data       make a new filesystem on --data. The explicit, destructive opt-in.
  --reuse             preserve every filesystem that already exists: no mkfs on the ESP or the
                      root, and no GPT type code rewritten. Refuses if a required one is absent.
  --resume            with --esp/--root: continue an install this provisioner started and recorded
                      as bootstrapped (/var/lib/templaros/provision/state on the target). Nothing is
                      formatted and Ubuntu is not bootstrapped again; every later step is repeated,
                      and each is written to be repeatable. Needs root even for the dry run.
  --inspect           with --esp/--root: print what the pair holds and exit, writing nothing:
                      fresh, resumable, rebuild, complete or foreign, then the reason. Needs root.
  --check-packages    resolve the whole package set against the Ubuntu snapshot and the archive in
                      a scratch apt root, say what cannot be installed and why, and exit. No target,
                      no root. Every --apply does the same before its first write.
  --plan-out PATH     write the reviewed operation sequence here (default: alongside the target).
  --ignore-preflight  install even though the live medium's hardware preflight reported a failure.
  --plan PATH         with --apply: run *this* reviewed plan. The machine is observed again first
                      and any drift in topology, filesystem identity or mounted state is refused.
  --help              this text.
USAGE
}

orig_args=("$@")
check_packages=0
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
        --nvidia) TEMPLAROS_NVIDIA="${2:?--nvidia needs a package}"; shift 2 ;;
        --set) DEVREPO_MARKER="${2:?--set needs an id}"; shift 2 ;;
        --layout) layout="${2:?--layout needs a path}"; shift 2 ;;
        --data) data_arg="${2:?--data needs a partition}"; shift 2 ;;
        --format-data) format_data=1; shift ;;
        --reuse) reuse=1; shift ;;
        --resume) resume=1; reuse=1; shift ;;
        --inspect) inspect=1; shift ;;
        --ignore-preflight) ignore_preflight=1; shift ;;
        --check-packages) check_packages=1; shift ;;
        --plan-out) plan_out="${2:?--plan-out needs a path}"; shift 2 ;;
        --plan) plan_in="${2:?--plan needs a path}"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; die "unknown argument $1" ;;
    esac
done

# Every mount a privileged run makes lives in a mount namespace of its own, as a slave of the host's:
# the host's mounts reach in, nothing made here reaches out. Binding the host's /dev into a target
# otherwise joins the host's /dev peer group, so a mount or unmount under <target>/dev propagates
# back onto the live system's own /dev — the kind of damage that leaves `sudo` unable to open a
# pty. And when the run ends, however it ends, the kernel takes the namespace's mounts with it.
if [ "$(id -u)" = 0 ] && [ "${TEMPLAROS_PROVISION_MOUNT_NS:-}" != 1 ]; then
    export TEMPLAROS_PROVISION_MOUNT_NS=1
    exec unshare --mount --propagation slave -- bash "$0" "${orig_args[@]}"
fi

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
if [ -n "$plan_in" ]; then
    [ -f "$plan_in" ] || die "no plan at $plan_in"
    deployment_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["deployment"])' "$plan_in" 2>/dev/null)" \
        || die "$plan_in is not a valid provision plan"
fi
esp_dir="${ESP_DIR_TEMPLATE/\{id\}/$deployment_id}"
cmdline="${CMDLINE_TEMPLATE/\{id\}/$deployment_id}"

# --- Refusals ------------------------------------------------------------------------------------
# Everything below this line is destructive. These are the conditions under which it must not run,
# stated once, before any of it.

# The live medium runs `templar doctor --preflight` once at boot and leaves its report behind.
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
[ -n "$target" ] || [ "$check_packages" = 1 ] \
    || { usage >&2; die "no target: pass --image PATH, --free-space DISK, --esp/--root, or --device with --i-mean-it"; }

case "$target_kind" in
    image)
        case "$target" in
            /dev/*) die "$target looks like a device; pass it with --device, --free-space or --esp/--root, not --image" ;;
        esac
        [ -b "$target" ] && die "$target is a block device; pass it with --device, --free-space or --esp/--root, not --image"
        ;;
    device)
        [ "$i_mean_it" = 1 ] || die "--device destroys every partition on $target. Add --i-mean-it if that is what you want."
        [ -b "$target" ] || die "$target is not a block device"
        [ "$(block_type "$target")" = disk ] || die "$target is not TYPE=disk; --device takes a whole disk"
        target="$(canonical_device "$target")"
        root_source="$(findmnt -no SOURCE / 2>/dev/null || true)"
        root_source="${root_source%%\[*}"
        root_disk="$(parent_disk "$root_source")"
        [ "$target" != "$root_disk" ] || die "$target is the disk this system booted from. Refusing."
        ;;
    free-space)
        # Not destructive to what exists, but it still writes a partition table; a regular file
        # (a fixture, or an image with partitions on it already) is the way to rehearse it.
        [ -e "$target" ] || die "$target does not exist"
        if [ -b "$target" ]; then
            [ "$(block_type "$target")" = disk ] \
                || die "$target is TYPE=$(block_type "$target"); --free-space requires TYPE=disk"
            target="$(canonical_device "$target")"
        fi
        ;;
    partitions)
        for part in "$esp_arg" "$root_arg"; do
            [ -b "$part" ] || die "$part is not a block device"
            [ "$(block_type "$part")" = part ] || die "$part is TYPE=$(block_type "$part"); --esp/--root require partitions"
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
[ "$apply" = 0 ] || { [ -n "$username" ] && [ "$read_password" = 1 ]; } \
    || die "--apply requires --user NAME and --password-stdin; a locked installation account cannot log in"
[ "$format_data" = 1 ] && [ -z "$data_arg" ] && die "--format-data without --data formats nothing"
if [ "$reuse" = 1 ]; then
    [ "$target_kind" = partitions ] \
        || die "--reuse preserves filesystems that already exist, so it goes with --esp/--root; \
--image, --device and --free-space create them"
    [ "$format_data" = 1 ] && die "--reuse and --format-data contradict each other"
fi

# A disk with a mounted descendant or active swap is in use. Device names and root-source string
# prefixes are not an identity boundary, so refuse the whole ownership tree.
if [ -b "${target:-}" ] && [ "$target_kind" != partitions ]; then
    in_use="$(lsblk -nrpo NAME,MOUNTPOINTS -- "$target" 2>/dev/null | awk 'NF > 1 { print }')"
    [ -z "$in_use" ] || die "$target or one of its partitions is mounted. Refusing:\n$in_use"
    if command -v swapon >/dev/null 2>&1; then
        while IFS= read -r swapdev; do
            [ -n "$swapdev" ] || continue
            lsblk -nrpo NAME -- "$target" 2>/dev/null | grep -Fxq "$(canonical_device "$swapdev")" \
                && die "$target contains active swap $swapdev. Refusing."
        done < <(swapon --noheadings --raw --output NAME 2>/dev/null || true)
    fi
fi

# Freeze a mutable archive marker to an immutable set before planning. The archive digest below is
# compared again by --apply --plan, so a republished marker or index invalidates the review.
case "$DEVREPO_MARKER" in
    current|previous) selected_set="$(sed -n '1p' "$DEVREPO_ROOT/$DEVREPO_MARKER" 2>/dev/null || true)" ;;
    *) selected_set="$DEVREPO_MARKER" ;;
esac
case "$selected_set" in
    *[!A-Za-z0-9._~+-]*) die "archive set id contains unsafe characters: $selected_set" ;;
esac
archive_digest=missing
if [ -n "$selected_set" ] && [ -d "$DEVREPO_ROOT/sets/$selected_set" ]; then
    archive_digest="$({
        for f in "$DEVREPO_ROOT/sets/$selected_set/manifest.json" \
                 "$DEVREPO_ROOT/Packages" "$DEVREPO_ROOT/Release" "$DEVREPO_ROOT/InRelease"; do
            [ -f "$f" ] && sha256sum "$f"
        done
    } | sha256sum | awk '{print $1}')"
fi

# --- The package set must be installable before anything is written --------------------------------
#
# 2026-09-30: an install partitioned, formatted and bootstrapped the laptop, then stopped at the
# package step: templaros-dev-cuda depends on nvidia-cuda-toolkit, which is only in multiverse, and
# the sources named main, universe and restricted. The same question can be put to the same sources
# before the first write: a scratch apt root with an empty dpkg status, the Ubuntu snapshot and the
# TemplarOS set configured exactly as the target will have them, and `apt-get install --simulate` of
# every package the install asks for. `--check-packages` asks it on its own, with no target, as an
# ordinary user (distro/iso/build.sh does, before it builds a medium).
base_packages=("$TEMPLAROS_KEYRING" "$TEMPLAROS_SESSION" "$TEMPLAROS_DESKTOP" "$TEMPLAROS_DEV_RUST"
    "$TEMPLAROS_DEV_CUDA" "$TEMPLAROS_FILES")
kernel_packages=("$TEMPLAROS_KERNEL" "$TEMPLAROS_NVIDIA")
boot_packages=(shim-signed grub-efi-amd64-signed)

# The target's apt configuration, and the scratch root's: one writer, so they cannot differ.
write_apt_config() { # root archive_dir archive_keyring
    local root="$1"
    mkdir -p "$root/etc/apt/sources.list.d" "$root/etc/apt/preferences.d" "$root/etc/apt/apt.conf.d"
    printf 'Acquire::ForceIPv4 "true";\nAcquire::Retries "8";\n' > "$root/etc/apt/apt.conf.d/01templaros-acquire"
    cat > "$root/etc/apt/sources.list.d/ubuntu.sources" <<EOF
Types: deb
URIs: $UBUNTU_MIRROR
Suites: $UBUNTU_SUITE $UBUNTU_SUITE-updates
Components: $UBUNTU_COMPONENTS
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: $UBUNTU_SECURITY_MIRROR
Suites: $UBUNTU_SUITE-security
Components: $UBUNTU_COMPONENTS
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
    cat > "$root/etc/apt/sources.list.d/templaros.sources" <<EOF
Types: deb
URIs: file://$2
Suites: ./
Signed-By: $3
EOF
    printf 'Package: templaros-* templar-files\nPin: version %s\nPin-Priority: 1001\n' \
        "$selected_set" > "$root/etc/apt/preferences.d/templaros-set"
}

# Which of the requested packages cannot be installed, and the first thing each one needs that no
# configured source has — one line per cause, rather than apt's cascade of everything downstream.
explain_unsolvable() { # apt-get apt-cache packages...
    local get="$1" cache="$2" pkg cand group alt name ver found named=0
    shift 2
    for pkg in "$@"; do
        cand="$($cache policy "$pkg" 2>/dev/null | sed -n 's/^ *Candidate: //p')"
        if [ -z "$cand" ] || [ "$cand" = "(none)" ]; then
            printf '  %s: no configured source has it\n' "$pkg"; named=1; continue
        fi
        $get install -s -q "$pkg" >/dev/null 2>&1 && continue
        found=0
        while IFS= read -r group; do
            [ -n "$group" ] || continue
            local any=0
            IFS='|' read -ra alts <<< "$group"
            for alt in "${alts[@]}"; do
                name="$(printf '%s' "$alt" | sed 's/(.*//; s/:any//' | xargs)"
                ver="$(printf '%s' "$alt" | sed -n 's/.*(= *\([^)]*\)).*/\1/p' | xargs)"
                cand="$($cache policy "$name" 2>/dev/null | sed -n 's/^ *Candidate: //p')"
                [ -n "$cand" ] && [ "$cand" != "(none)" ] || continue
                if [ -z "$ver" ] || $cache madison "$name" 2>/dev/null | awk -F'|' '{ gsub(/ /, "", $2); print $2 }' | grep -Fxq "$ver"; then
                    any=1; break
                fi
            done
            if [ "$any" = 0 ]; then
                printf '  %s needs %s — no configured source has it\n' "$pkg" "$(printf '%s' "$group" | xargs)"
                found=1
            fi
        done < <($cache show --no-all-versions "$pkg" 2>/dev/null \
            | sed -n 's/^\(Pre-\)\{0,1\}Depends: //p' | tr ',' '\n')
        if [ "$found" = 0 ]; then
            printf '  %s: apt cannot install it; in its words:\n' "$pkg"
            $get install -s -q "$pkg" 2>&1 | grep -E '^(E:| [a-z0-9].* : )' | head -6 | sed 's/^/    /'
        fi
        named=1
    done
    [ "$named" = 1 ] || printf '  every package installs alone; together they conflict:\n'
}

solve_check() {
    local root out installs status=0
    root="$(mktemp -d "${TMPDIR:-/tmp}/templaros-solve.XXXXXX")"
    chmod 0755 "$root"
    mkdir -p "$root/var/lib/apt/lists/partial" "$root/var/cache/apt/archives/partial" "$root/var/lib/dpkg"
    : > "$root/var/lib/dpkg/status"
    write_apt_config "$root" "$DEVREPO_ROOT" "$DEVREPO_ROOT/templaros-dev.gpg"
    # APT_CONFIG replaces the host's /etc/apt entirely: its sources, pins and proxies stay out of it.
    printf 'Dir "%s/";\nDir::State::status "%s/var/lib/dpkg/status";\nAPT::Architecture "amd64";\nAPT::Architectures { "amd64"; };\nAcquire::Languages "none";\nDebug::NoLocking "true";\n' \
        "$root" "$root" > "$root/apt.conf"
    local get="env APT_CONFIG=$root/apt.conf apt-get" cache="env APT_CONFIG=$root/apt.conf apt-cache"
    local all=("${base_packages[@]}" "${kernel_packages[@]}" "${boot_packages[@]}")
    if ! out="$($get update -q 2>&1)" || printf '%s\n' "$out" | grep -q '^[EW]: .*\(Failed to fetch\|not signed\|NO_PUBKEY\)'; then
        printf '%s\n' "$out" | grep -E '^[EW]:' | head -8 >&2
        rm -rf "$root"
        die "cannot read the package indexes ($UBUNTU_MIRROR: $UBUNTU_COMPONENTS; TemplarOS set $selected_set at $DEVREPO_ROOT). Nothing has been written."
    fi
    if out="$($get install -s -q "${all[@]}" 2>&1)"; then
        installs="$(printf '%s\n' "$out" | grep -c '^Inst ' || true)"
        note "all ${#all[@]} requested packages resolve from $UBUNTU_MIRROR ($UBUNTU_COMPONENTS) and set $selected_set: $installs to install"
    else
        {
            printf 'provision: the package set cannot be installed from these sources:\n'
            explain_unsolvable "$get" "$cache" "${all[@]}"
            printf '  sources: %s (%s), set %s\n' "$UBUNTU_MIRROR" "$UBUNTU_COMPONENTS" "$selected_set"
        } >&2
        status=1
    fi
    rm -rf "$root"
    [ "$status" = 0 ] || die "refusing before anything is written."
}

if [ "$check_packages" = 1 ]; then
    [ -n "$selected_set" ] && [ -f "$DEVREPO_ROOT/sets/$selected_set/manifest.json" ] \
        || die "cannot resolve archive marker $DEVREPO_MARKER at $DEVREPO_ROOT"
    solve_check
    exit 0
fi

# Two filesystems with the same label are two filesystems the boot path cannot tell apart, and
# `root=LABEL=templaros-root` would mount whichever the kernel saw first. Identity in fstab and in
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

# --- What an existing TemplarOS pair holds ---------------------------------------------------------
#
# The install records its progress on the target itself, in @var at $state_file: `formatted` once
# the filesystems and subvolumes exist, `bootstrapped` once Ubuntu is in @, `complete` at the end.
# It names the root UUID and the ESP PARTUUID it was written for, so a record copied onto another
# filesystem, or a filesystem recreated under the same labels, is not mistaken for this install.
#
# A pair is only ever judged resumable from that record. A populated @ without one is either an
# earlier provisioner's partial install (its install.toml names this very filesystem, and nothing
# recorded how far it got, so it is rebuilt from the start) or something the provisioner did not
# write at all, which is refused.
state_file=/var/lib/templaros/provision/state
verdict=""
verdict_reason=""
state_phase=""
state_deployment=""
inspect_target() {
    local esp_pl root_pl rtype rlabel ruuid esp_pu top state="" legacy_uuid="" populated=0 known_good=0
    local phase state_suite state_set
    verdict=foreign
    esp_pl="$(lsblk -dnro PARTLABEL -- "$esp_arg" 2>/dev/null || true)"
    root_pl="$(lsblk -dnro PARTLABEL -- "$root_arg" 2>/dev/null || true)"
    if [ "$esp_pl" != "$ESP_LABEL" ] || [ "$root_pl" != "$ROOT_LABEL" ]; then
        verdict_reason="the partition names are '$esp_pl' and '$root_pl', not the $ESP_LABEL and $ROOT_LABEL this provisioner gives the partitions it creates"
        return 0
    fi
    rtype="$(blkid -s TYPE -o value "$root_arg" 2>/dev/null || true)"
    if [ -z "$rtype" ]; then
        verdict=fresh
        verdict_reason="the partitions exist but $root_arg holds no filesystem yet"
        return 0
    fi
    if [ "$rtype" != "$ROOT_FORMAT" ]; then
        verdict_reason="$root_arg holds a $rtype filesystem, not $ROOT_FORMAT"
        return 0
    fi
    rlabel="$(blkid -s LABEL -o value "$root_arg" 2>/dev/null || true)"
    if [ "$rlabel" != "$ROOT_LABEL" ]; then
        verdict_reason="$root_arg is labelled '$rlabel', not $ROOT_LABEL"
        return 0
    fi
    ruuid="$(uuid_of "$root_arg")"
    esp_pu="$(partuuid_of "$esp_arg")"

    top="$(mktemp -d "${TMPDIR:-/tmp}/templaros-inspect.XXXXXX")"
    mount -o ro,subvolid=5 "$root_arg" "$top" \
        || { rmdir "$top"; die "cannot mount $root_arg read-only to read its install state"; }
    if [ -f "$top/@var${state_file#/var}" ]; then state="$(cat "$top/@var${state_file#/var}")"; fi
    if [ -f "$top/@/etc/templaros/install.toml" ]; then
        legacy_uuid="$(sed -n 's/^root_uuid = "\(.*\)"$/\1/p' "$top/@/etc/templaros/install.toml")"
    fi
    if [ -d "$top/@" ] && [ -n "$(find "$top/@" -mindepth 1 -maxdepth 1 -print -quit)" ]; then populated=1; fi
    if [ -f "$top/@var/lib/templaros/hostd/grub/known-good" ]; then known_good=1; fi
    umount "$top" && rmdir "$top"

    if [ -n "$state" ]; then
        state_value() { printf '%s\n' "$state" | sed -n "s/^$1=//p" | head -1; }
        if [ "$(state_value root_uuid)" != "$ruuid" ]; then
            verdict_reason="the install state names root filesystem $(state_value root_uuid), and $root_arg is $ruuid"
            return 0
        fi
        if [ "$(state_value esp_partuuid)" != "$esp_pu" ]; then
            verdict_reason="the install state names ESP partition $(state_value esp_partuuid), and $esp_arg is $esp_pu"
            return 0
        fi
        phase="$(state_value phase)"
        state_phase="$phase"
        state_deployment="$(state_value deployment)"
        state_suite="$(state_value suite)"
        state_set="$(state_value archive_set)"
        state_mirror="$(state_value mirror)"
        case "$phase" in
            complete)
                verdict=complete
                verdict_reason="deployment $state_deployment was installed here completely; there is nothing to resume" ;;
            bootstrapped)
                if [ "$state_suite" != "$UBUNTU_SUITE" ]; then
                    verdict=rebuild
                    verdict_reason="Ubuntu $state_suite was bootstrapped here, and this run installs $UBUNTU_SUITE"
                elif [ "$state_mirror" != "$UBUNTU_MIRROR" ]; then
                    verdict=rebuild
                    verdict_reason="Ubuntu was bootstrapped from ${state_mirror:-an unrecorded mirror}, and this run installs from $UBUNTU_MIRROR"
                else
                    # The bootstrap is plain Ubuntu; the set only matters from the sources step on,
                    # which a resume repeats — and the pin moves installed packages to this set.
                    verdict=resumable
                    verdict_reason="Ubuntu $state_suite is bootstrapped for deployment $state_deployment; every later step is repeated with set $selected_set"
                    [ "$state_set" = "$selected_set" ] \
                        || verdict_reason="$verdict_reason (it was begun with $state_set)"
                fi ;;
            *)
                verdict=rebuild
                verdict_reason="the install stopped before Ubuntu finished bootstrapping (recorded phase: ${phase:-none})" ;;
        esac
    elif [ "$populated" = 0 ]; then
        verdict=fresh
        verdict_reason="$root_arg carries no installed system"
    elif [ "$known_good" = 1 ]; then
        verdict=complete
        verdict_reason="an earlier provisioner finished an install here (hostd's known-good record is present)"
    elif [ -n "$legacy_uuid" ] && [ "$legacy_uuid" = "$ruuid" ]; then
        verdict=rebuild
        verdict_reason="an earlier provisioner stopped partway through an install here and recorded no progress, so it cannot be resumed"
    else
        verdict_reason="@ on $root_arg holds a system this provisioner has no record of writing"
    fi
}

if [ "$inspect" = 1 ] || [ "$resume" = 1 ]; then
    [ "$target_kind" = partitions ] \
        || die "--inspect and --resume read an existing TemplarOS pair, so they go with --esp/--root"
    [ "$(id -u)" = 0 ] \
        || die "--inspect and --resume need root: $root_arg is mounted read-only to read its install state"
    inspect_target
    if [ "$inspect" = 1 ]; then
        [ "$apply" = 0 ] || die "--inspect only reads; it does not go with --apply"
        printf '%s: %s\n' "$verdict" "$verdict_reason"
        exit 0
    fi
    [ "$verdict" = resumable ] \
        || die "--resume refused: $esp_arg + $root_arg is $verdict — $verdict_reason"
    # The deployment the bootstrapped target was begun as keeps its name: its ESP directory and
    # its command line are derived from it.
    [ -n "$plan_in" ] || deployment_id="$state_deployment"
    [ "$deployment_id" = "$state_deployment" ] \
        || die "the plan is for deployment $deployment_id, and the target records $state_deployment"
    esp_dir="${ESP_DIR_TEMPLATE/\{id\}/$deployment_id}"
    cmdline="${CMDLINE_TEMPLATE/\{id\}/$deployment_id}"
fi

real_disk=0
case "$target_kind" in
    device) real_disk=1 ;;
    free-space|partitions) [ -b "${esp_arg:-$target}" ] && real_disk=1 ;;
esac
if [ "$real_disk" = 1 ] && [ "${TEMPLAROS_PROVISION_ALLOW_DEVICE:-0}" != 1 ]; then
    die "writing to a real disk also needs TEMPLAROS_PROVISION_ALLOW_DEVICE=1 in the environment.
     That second switch exists so a copied-and-pasted command line cannot reach a disk on its own."
fi
[ "$apply" = 0 ] || [ -n "$plan_in" ] \
    || die "--apply requires --plan PATH. Run the dry plan, read it, then apply that exact reviewed request."
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
        *) plan_out="${TMPDIR:-/tmp}/templaros-provision-plan.json" ;;
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
            [ -b "$device" ] && disk="$(parent_disk "$device")"
            if [ -b "$disk" ]; then
                printf 'disk:%s=%s\n' "$disk" \
                    "$(lsblk -dnro 'MAJ:MIN,TYPE,SIZE,MODEL,SERIAL,WWN' -- "$disk" 2>/dev/null | xargs)"
                printf 'children:%s=%s\n' "$disk" \
                    "$(lsblk -nrpo 'NAME,TYPE,SIZE,FSTYPE,LABEL,PARTUUID,UUID,MOUNTPOINTS' -- "$disk" 2>/dev/null | tr '\n' '|')"
            fi
            printf 'topology:%s=%s\n' "$disk" \
                "$(sgdisk -p "$disk" 2>/dev/null | sed -n -e 's/^Disk identifier (GUID): /guid:/p' -e 's/^ *\([0-9]\+\) *\([0-9]\+\) *\([0-9]\+\).*/\1:\2:\3/p' | tr '\n' ',')"
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

request_state() {
    printf '%s\n' \
        "target=$target" "target_kind=$target_kind" "esp=$esp_arg" "root=$root_arg" \
        "data=$data_arg" "reuse=$reuse" "resume=$resume" "format_data=$format_data" "username=$username" \
        "password_stdin=$read_password" "nvidia=$TEMPLAROS_NVIDIA" "suite=$UBUNTU_SUITE" \
        "mirror=$UBUNTU_MIRROR" "security_mirror=$UBUNTU_SECURITY_MIRROR" \
        "components=$UBUNTU_COMPONENTS" "bootstrap_backend=$bootstrap_backend" \
        "layout_sha256=$(sha256sum "$layout" | awk '{print $1}')" \
        "provisioner_sha256=$(sha256sum "$0" | awk '{print $1}')" \
        "archive=$(canonical_device "$DEVREPO_ROOT")" "archive_set=$selected_set" \
        "archive_digest=$archive_digest" "mount=$mnt"
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
# Set once this run has written to the target, so a failure can say whether it left a partial
# install behind or stopped with the target as it found it.
target_modified=0
run() {
    record "$@"
    if [ "$apply" = 1 ]; then
        case "$(classify "$1")" in create|format) target_modified=1 ;; esac
        "$@"
    else
        printf '     $ %s\n' "$*"
    fi
}
in_target() { run chroot "$mnt" "$@"; }

owned_mounts=()
loop=""
mnt_created=0
cleanup_enabled=0
mount_owned() {
    local mountpoint="${*: -1}"
    record mount "$@"
    if [ "$apply" = 1 ]; then
        mount "$@"
        owned_mounts+=("$mountpoint")
    else
        printf '     $ mount %s\n' "$*"
    fi
}
unmount_owned() {
    local mountpoint="$1" i
    run umount "$mountpoint"
    if [ "$apply" = 1 ]; then
        for ((i=${#owned_mounts[@]}-1; i>=0; i--)); do
            [ "${owned_mounts[$i]}" = "$mountpoint" ] && unset 'owned_mounts[i]' && break
        done
    fi
}
cleanup() {
    local status=$? i cleanup_failed=0
    [ "$cleanup_enabled" = 1 ] || return "$status"
    trap - EXIT INT TERM
    set +e
    for ((i=${#owned_mounts[@]}-1; i>=0; i--)); do
        mountpoint -q "${owned_mounts[$i]}" && umount "${owned_mounts[$i]}" || true
        mountpoint -q "${owned_mounts[$i]}" && cleanup_failed=1
    done
    if [ -n "$loop" ] && [ "$loop" != '${loop}' ]; then
        losetup "$loop" >/dev/null 2>&1 && losetup -d "$loop" || true
        losetup "$loop" >/dev/null 2>&1 && cleanup_failed=1
    fi
    [ "$mnt_created" = 1 ] && rmdir "$mnt" 2>/dev/null || true
    [ "$cleanup_failed" = 0 ] || printf 'provision: cleanup left a mount or loop attached; inspect before retrying\n' >&2
    [ "$status" = 0 ] || failure_report "$status"
    return "$status"
}
# What a failed apply leaves behind and the one command that carries on from it. Unmounting is
# still right — a half-installed target must not stay lent to the live system — but a person must
# not be left to work out from the mounts what state the disk is in.
failure_report() { # status
    local again esp="${esp_part:-$esp_arg}" root="${root_part:-$root_arg}"
    {
        printf '\nInstallation stopped at step %d (%s), exit %s.\n' "$step_no" "$current_step" "$1"
        if [ "$target_modified" = 0 ]; then
            printf 'Nothing on the target was changed.\n'
            return 0
        fi
        printf 'The target has been modified: %s (ESP) and %s (root) hold a partial TemplarOS install.\n' "$esp" "$root"
        printf 'Recorded install state: %s.\n' "${state_phase:-none — it stopped before the filesystems were ready}"
        if [ -n "${TEMPLAROS_PROVISION_RETRY_HINT:-}" ]; then
            printf 'Next: %s\n' "$TEMPLAROS_PROVISION_RETRY_HINT"
            return 0
        fi
        [ "$target_kind" != image ] || { printf 'Next: run the same --image command again; the image is rebuilt from the start.\n'; return 0; }
        again="templaros-install --esp $esp --root $root"
        [ "$real_disk" = 0 ] || again="TEMPLAROS_PROVISION_ALLOW_DEVICE=1 $again"
        if [ "$state_phase" = bootstrapped ] || [ "$state_phase" = complete ]; then
            printf 'Resume with (Ubuntu is kept; every step after the bootstrap is repeated):\n'
            again="$again --resume"
        else
            printf 'Start again on the same two partitions (only they are reformatted; no other partition is touched):\n'
        fi
        again="$again --user $username --password-stdin"
        printf '  %s --plan-out /tmp/templaros-resume.json\n' "$again"
        printf '  %s --apply --plan /tmp/templaros-resume.json\n' "$again"
    } >&2
}
# The install's own record of how far it got (see inspect_target). Written to a temporary name and
# renamed, so an interrupted write leaves the previous phase rather than half a file.
write_state() { # phase
    local ruuid epu
    state_phase="$1"
    note "install state: $1 (recorded in $mnt$state_file)"
    [ "$apply" = 1 ] || return 0
    ruuid="$(uuid_of "$root_part")"; epu="$(partuuid_of "$esp_part")"
    [ -n "$ruuid" ] && [ -n "$epu" ] || die "cannot read the root UUID or ESP PARTUUID to record the install state"
    mkdir -p "$mnt${state_file%/*}"
    printf 'version=1\nphase=%s\nroot_uuid=%s\nesp_partuuid=%s\ndeployment=%s\narchive_set=%s\nsuite=%s\nmirror=%s\nusername=%s\nrecorded=%s\n' \
        "$1" "$ruuid" "$epu" "$deployment_id" "$selected_set" "$UBUNTU_SUITE" "$UBUNTU_MIRROR" "$username" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        > "$mnt$state_file.new"
    mv "$mnt$state_file.new" "$mnt$state_file"
}

write_plan() {
    {
        printf '{\n  \"version\": 2,\n'
        printf '  \"target\": \"%s\",\n  \"target_kind\": \"%s\",\n' "$(json_escape "$target")" "$target_kind"
        printf '  \"deployment\": \"%s\",\n  \"cmdline\": \"%s\",\n' "$deployment_id" "$(json_escape "$cmdline")"
        printf '  \"reuse\": %s,\n  \"format_data\": %s,\n' \
            "$([ "$reuse" = 1 ] && echo true || echo false)" \
            "$([ "$format_data" = 1 ] && echo true || echo false)"
        printf '  \"request\": ['
        local request_first=1
        while IFS= read -r line; do
            [ $request_first -eq 1 ] || printf ','
            request_first=0
            printf '\n    \"%s\"' "$(json_escape "$line")"
        done <<< "$request"
        printf '\n  ],\n'
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

mnt="${TEMPLAROS_PROVISION_MOUNT:-/mnt/templaros-provision}"
case "$mnt" in /*) ;; *) die "TEMPLAROS_PROVISION_MOUNT must be absolute" ;; esac
[ ! -L "$mnt" ] || die "$mnt is a symlink; refusing it as a provisioning mount root"
[ ! -e "$mnt" ] || ! mountpoint -q "$mnt" || die "$mnt is already mounted"

observed="$(observe)"
request="$(request_state)"

# `--apply --plan` is the only path that mutates something a person has already reviewed. Observe
# the machine again and refuse on any difference: a plan made against one disk must not be applied
# to another, and a partition mounted since the review is a partition somebody is using.
if [ -n "$plan_in" ]; then
    [ "$apply" = 1 ] || die "--plan is for --apply; a dry run makes a plan rather than consuming one"
    [ -f "$plan_in" ] || die "no plan at $plan_in"
    reviewed_version="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("version", 0))' "$plan_in")"
    [ "$reviewed_version" = 2 ] || die "$plan_in is not a current version 2 plan"
    reviewed_request="$(python3 -c 'import json,sys; print("\n".join(json.load(open(sys.argv[1]))["request"]))' "$plan_in")"
    if [ "$reviewed_request" != "$request" ]; then
        printf 'provision: the requested operation is not the one this plan reviewed.\n\n' >&2
        diff <(printf '%s\n' "$reviewed_request") <(printf '%s\n' "$request") \
            | sed 's/^/    /' >&2 || true
        die "refusing to apply. Use the exact reviewed options and inputs."
    fi
    reviewed="$(python3 -c 'import json,sys; print("\n".join(json.load(open(sys.argv[1]))["observed"]))' "$plan_in")"
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
if [ "$apply" = 1 ]; then
    [ -n "$selected_set" ] && [ -d "$DEVREPO_ROOT/sets/$selected_set" ] \
        || die "cannot resolve archive marker $DEVREPO_MARKER at $DEVREPO_ROOT"
    [ -f "$DEVREPO_ROOT/Packages" ] && [ -f "$DEVREPO_ROOT/templaros-dev.gpg" ] \
        || die "$DEVREPO_ROOT is not a signed apt archive"
    templar dev --repo "$DEVREPO_ROOT" repo verify --set "$selected_set"
fi

# The target is bootstrapped from the Ubuntu mirror and its packages come from there too, so an
# install with no route to it would partition and format and then fail at the bootstrap, leaving a
# half-written disk. Ask the mirror for this suite's Release file *before* the first write: the same
# DNS, route and HTTP path the bootstrap takes, through apt's own client (the medium has no curl).
# On the reference laptop the network is wired (a USB-C adapter: r8152/ax88179/cdc_ncm are in
# linux-modules, their firmware in linux-firmware-realtek) — WiFi needs a desktop to join.
mirror_reachable() {
    local probe
    probe="$(mktemp)"
    /usr/lib/apt/apt-helper -o Acquire::ForceIPv4=true -o Acquire::Retries=2 \
        download-file "$UBUNTU_MIRROR/dists/$UBUNTU_SUITE/Release" "$probe" >/dev/null 2>&1
    local status=$?
    rm -f "$probe"
    return "$status"
}
if [ "$apply" = 1 ] && [ "${TEMPLAROS_PROVISION_SKIP_NETWORK_CHECK:-0}" != 1 ]; then
    mirror_reachable || die "cannot reach $UBUNTU_MIRROR/dists/$UBUNTU_SUITE/Release, and the install
downloads Ubuntu from there. Nothing has been written. Connect a network first:
  nmcli device            # the wired adapter should say 'connected'
  ip -br addr             # it should have an address
then run this again."
    note "the Ubuntu mirror $UBUNTU_MIRROR answers for $UBUNTU_SUITE"
    solve_check
fi

printf 'TemplarOS Developer Edition provisioner\n'
printf '  layout      %s\n' "$layout"
printf '  target      %s (%s)\n' "$target" "$target_kind"
printf '  mode        %s\n' "$([ "$apply" = 1 ] && echo APPLY || echo 'dry run — nothing is written')"
printf '  deployment  %s\n' "$deployment_id"
printf '  cmdline     %s\n' "$cmdline"
if [ "$real_disk" = 1 ]; then
    display_disk="${target}"
    [ "$target_kind" = partitions ] && display_disk="$(parent_disk "$esp_arg")"
    printf '\nINSTALL TARGET\n'
    lsblk -dnro 'PATH,MODEL,SERIAL,SIZE,TYPE' -- "$display_disk" 2>/dev/null \
        | awk '{ print "  path/model/serial/size/type: " $0 }'
    printf '\nEXISTING PARTITIONS PRESERVED\n'
    lsblk -nrpo 'NAME,SIZE,FSTYPE,LABEL,PARTLABEL,MOUNTPOINTS' -- "$display_disk" 2>/dev/null \
        | sed '1d; s/^/  /'
    printf '\nOTHER PHYSICAL DISKS — WILL NOT BE TOUCHED\n'
    while IFS= read -r other; do
        case "$other" in "$display_disk "*|"$display_disk") continue ;; esac
        printf '  %s\n' "$other"
    done < <(lsblk -dnro 'PATH,SIZE,MODEL,SERIAL' --nodeps 2>/dev/null)
    printf '\n'
fi

# The partition node for number $2 on disk $1: nvme0n1p3, mmcblk0p3, but sda3.
part_node() {
    case "$1" in *[0-9]) printf '%sp%s' "$1" "$2" ;; *) printf '%s%s' "$1" "$2" ;; esac
}
attach_loop() { # a regular file gets a loop device; the plan shows the placeholder
    if [ "$apply" = 1 ]; then
        record losetup --find --partscan --show "$target"
        loop="$(losetup --find --partscan --show "$target")"
    else
        note "the file is attached with losetup --partscan; its partitions are \${loop}p<n>"
        run losetup --find --partscan --show "$target"
        loop='${loop}'
    fi
}

if [ "$apply" = 1 ]; then
    cleanup_enabled=1
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    # A resumed target is a partial install before this run writes anything to it.
    [ "$resume" = 0 ] || target_modified=1
fi
case "$target_kind" in
image|device)
    step "Partition $target: GPT, ESP ($ESP_FORMAT, up to $ESP_SIZE_MAX, label $ESP_LABEL) + root ($ROOT_FORMAT, label $ROOT_LABEL)"
    if [ "$target_kind" = image ] && [ ! -e "$target" ]; then
        run truncate -s "${TEMPLAROS_IMAGE_SIZE:-32G}" "$target"
    fi
    run sgdisk --zap-all "$target"
    run sgdisk --new=1:0:+"${ESP_SIZE_MAX}" --typecode=1:ef00 --change-name=1:"$ESP_LABEL" "$target"
    run sgdisk --new=2:0:0 --typecode=2:8304 --change-name=2:"$ROOT_LABEL" "$target"
    if [ "$target_kind" = image ]; then
        attach_loop
        esp_part="${loop}p1"; root_part="${loop}p2"
    else
        esp_part="$(part_node "$target" 1)"; root_part="$(part_node "$target" 2)"
    fi
    ;;
free-space)
    step "Partition the largest free block of $target: ESP ($ESP_FORMAT, $ESP_SIZE_MAX, label $ESP_LABEL) + root ($ROOT_FORMAT, the rest, label $ROOT_LABEL); every existing partition stays"
    table="$(sgdisk -p "$target" 2>/dev/null || true)"
    [ -n "$table" ] || die "cannot read the partition table of $target (run as root, or is it not GPT?)"
    sector="$(printf '%s\n' "$table" | sed -n 's/^Sector size (logical\/physical): \([0-9]*\)\/.*/\1/p')"
    # -F and -E are the first and last sector of the same thing: the largest free block. Neither
    # says anything about where on the disk that block is, and neither needs to. A gap left by a
    # deleted operating system is usually at the *front*, with every surviving partition above it,
    # and this refused to use one until 2026-09-09 on the theory that a hole was more dangerous
    # than a tail. It is not: `--new=N:0:+size` starts at the first free sector of that block and
    # `--new=N:0:0` ends at its last, so sgdisk confines both new partitions to the block by
    # construction and cannot reach a neighbour whatever its position.
    free_first="$(sgdisk -F "$target")"
    free_last="$(sgdisk -E "$target")"
    existing="$(printf '%s\n' "$table" | awk '$1 ~ /^[0-9]+$/ { print $1, $2, $3, $6, $7, $8 }')"
    free_bytes=$(( (free_last - free_first + 1) * ${sector:-512} ))
    need_bytes=$(( $(numfmt --from=iec "$ESP_SIZE_MAX") + $(numfmt --from=iec "$(layout_value root size_min)") ))
    [ "$free_bytes" -ge "$need_bytes" ] \
        || die "the largest free block of $target is $(numfmt --to=iec "$free_bytes"); the layout needs at least $(numfmt --to=iec "$need_bytes") (ESP $ESP_SIZE_MAX + root $(layout_value root size_min))"
    highest="$(printf '%s\n' "$existing" | awk 'BEGIN { n = 0 } $1 > n { n = $1 } END { print n }')"
    n_esp=$((highest + 1)); n_root=$((highest + 2))
    note "free block: sectors $free_first..$free_last ($(numfmt --to=iec "$free_bytes")); new partitions $n_esp (ESP) and $n_root (root)"
    note "kept untouched:"
    printf '%s\n' "$existing" | while read -r num start end _ _ _; do
        [ -n "$num" ] && note "  partition $num (sectors $start..$end)"
    done
    esp_sectors=$(( $(numfmt --from=iec "$ESP_SIZE_MAX") / ${sector:-512} ))
    esp_first="$free_first"
    esp_last=$((esp_first + esp_sectors - 1))
    root_first=$(( ((esp_last + 1 + 2047) / 2048) * 2048 ))
    root_last="$free_last"
    [ "$root_first" -le "$root_last" ] || die "the reviewed free block cannot fit aligned ESP and root partitions"
    run sgdisk --new="$n_esp:$esp_first:$esp_last" --typecode="$n_esp:ef00" --change-name="$n_esp:$ESP_LABEL" "$target"
    run sgdisk --new="$n_root:$root_first:$root_last" --typecode="$n_root:8304" --change-name="$n_root:$ROOT_LABEL" "$target"
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
        # why @templaros.decision.independent-storage-roles says the current mode "is not
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
        run sgdisk --typecode="$(lsblk -no PARTN "$esp_arg" 2>/dev/null || echo N):ef00" "/dev/$(lsblk -dno PKNAME "$esp_arg" 2>/dev/null || echo DISK)"
        run sgdisk --typecode="$(lsblk -no PARTN "$root_arg" 2>/dev/null || echo N):8304" "/dev/$(lsblk -dno PKNAME "$root_arg" 2>/dev/null || echo DISK)"
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

data_fs_type=""
if [ -n "$data_arg" ]; then
    if [ "$format_data" = 1 ]; then
        step "Make the data filesystem on $data_arg ($DATA_FORMAT, label $DATA_LABEL) — you asked for this with --format-data"
        run "mkfs.$DATA_FORMAT" -f -L "$DATA_LABEL" "$data_arg"
        data_fs_type="$DATA_FORMAT"
    else
        step "Reuse the data filesystem on $data_arg; it is never assumed expendable"
        found="$(blkid -s TYPE -o value "$data_arg" 2>/dev/null || true)"
        [ -n "$found" ] || die "$data_arg holds no filesystem. Add --format-data to make one, deliberately."
        [ "$found" = "$DATA_FORMAT" ] || die "$data_arg is $found; the declared data role requires $DATA_FORMAT"
        data_fs_type="$found"
        reused "$data_arg" data "$found"
        note "  reusing the $found filesystem already on $data_arg (--format-data would replace it)"
    fi
fi

step "Create the declared subvolumes: $SUBVOLUMES"
[ -e "$mnt" ] || mnt_created=1
run mkdir -p "$mnt"
mount_owned -o subvolid=5 "$root_part" "$mnt"
for subvol in $SUBVOLUMES; do
    subvol_path="$mnt/${subvol#/}"
    if [ "$apply" = 1 ] && btrfs subvolume show "$subvol_path" >/dev/null 2>&1; then
        [ "$reuse" = 1 ] || die "$subvol already exists on a newly formatted root"
        note "  reusing existing subvolume $subvol"
        record __reuse__ "$subvol_path" subvolume btrfs
    else
        run btrfs subvolume create "$subvol_path"
    fi
done
# --reuse keeps filesystems, not installs: a populated @ is an install, and only one this
# provisioner recorded as bootstrapped is continued — by --resume, which checked it above.
if [ "$apply" = 1 ] && [ "$reuse" = 1 ] && [ "$resume" = 0 ] \
    && [ -n "$(find "$mnt/@" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    die "--reuse found a populated @ root. If an install stopped partway here, \`--inspect\` says whether
     it can be resumed (--resume) or must be rebuilt (the same --esp/--root without --reuse)"
fi
unmount_owned "$mnt"

step "Mount the target at $mnt"
mount_owned -o "subvol=@,$(layout_value root mount_options)" "$root_part" "$mnt"
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
    mount_owned -o "subvol=${subvol#/}" "$root_part" "$mnt/$dir"
done
run mkdir -p "$mnt$ESP_MOUNT"
mount_owned "$esp_part" "$mnt$ESP_MOUNT"
if [ -n "$data_arg" ]; then
    run mkdir -p "$mnt$DATA_MOUNT"
    mount_owned ${DATA_MOUNT_OPTIONS:+-o "$DATA_MOUNT_OPTIONS"} "$data_arg" "$mnt$DATA_MOUNT"
fi
if [ "$resume" = 0 ]; then
    write_state formatted
fi

# Bootstrap owns its own temporary /dev, /proc, /sys and resolver setup. The target contains only
# the intentional Btrfs/ESP mountpoints at this point; target-owned files and host binds come later.
step "Bootstrap Ubuntu $UBUNTU_SUITE ($UBUNTU_COMPONENTS) from $UBUNTU_MIRROR"
if [ "$resume" = 1 ]; then
    note "already done: the install state records Ubuntu $UBUNTU_SUITE bootstrapped for $deployment_id; not repeated"
else
    [ "$apply" = 0 ] || target_modified=1
    if [ "$bootstrap_backend" = mmdebstrap ]; then
        # IPv4 and retries for the same reason distro/iso/build.sh has them: snapshot.ubuntu.com
        # answers on IPv4 only, and a hung IPv6 connection stalls apt for hours. ca-certificates
        # because every later apt run is inside the target, over HTTPS to that snapshot.
        run mmdebstrap --skip=check/empty --components="${UBUNTU_COMPONENTS// /,}" \
            --include=ca-certificates \
            --aptopt='Acquire::ForceIPv4 "true"' --aptopt='Acquire::Retries "8"' \
            "$UBUNTU_SUITE" "$mnt" "$UBUNTU_MIRROR"
    else
        note "mmdebstrap is absent; falling back to debootstrap"
        run debootstrap --include=ca-certificates --components="${UBUNTU_COMPONENTS// /,}" \
            "$UBUNTU_SUITE" "$mnt" "$UBUNTU_MIRROR"
    fi
    write_state bootstrapped
fi

# --- Identity: /etc/fstab and /etc/templaros/install.toml, both by UUID ----------------------------
#
# Two things were missing here and they are the same thing. There was no /etc/fstab at all — the
# mounts above are made for the chroot and were never written down, so a provisioned machine had
# nothing telling it how to mount @home, @var or the ESP on its own. And identity was by label:
# `root=LABEL=templaros-root` is the same string on every TemplarOS disk, so a machine with two of them
# attached mounts whichever the kernel enumerated first.
#
# A UUID is per filesystem and is minted by mkfs, so it cannot collide. install.toml is where
# templar-hostd reads it back (`boot_entries.rs`), and it falls back to the label when the file is
# absent — which is every machine provisioned before this and every mkosi image.
step "Filesystem identity: /etc/fstab and /etc/templaros/install.toml, both by UUID"
root_uuid="$(uuid_of "$root_part")"
esp_partuuid="$(partuuid_of "$esp_part")"
data_uuid=""
[ -n "$data_arg" ] && data_uuid="$(uuid_of "$data_arg")" || true
if [ "$apply" = 1 ]; then
    [ -n "$root_uuid" ] || die "the root filesystem has no UUID; blkid could not read $root_part"
    [ -n "$esp_partuuid" ] || die "the ESP has no PARTUUID; blkid could not read $esp_part"
    [ -z "$data_arg" ] || [ -n "$data_uuid" ] || die "the data filesystem has no UUID"
fi
note "root=UUID=${root_uuid:-<read at apply>}  esp PARTUUID=${esp_partuuid:-<read at apply>}"
note "writing $mnt/etc/fstab"
if [ "$apply" = 1 ]; then
    mkdir -p "$mnt/etc"
    {
        printf '# GENERATED by installer/provision.sh. Identity is by UUID, never by label:\n'
        printf '# every TemplarOS root carries the same label, and a machine with two of them\n'
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
        [ -n "$data_uuid" ] && printf 'UUID=%s  %s  %s  %s,nofail  0 2\n' \
            "$data_uuid" "$DATA_MOUNT" "$data_fs_type" "${DATA_MOUNT_OPTIONS:-defaults}"
    } > "$mnt/etc/fstab"
fi
note "writing $mnt/etc/templaros/install.toml — what templar-hostd reads to build a boot entry"
if [ "$apply" = 1 ]; then
    mkdir -p "$mnt/etc/templaros"
    {
        printf '# GENERATED by installer/provision.sh. Read by services/templar-hostd/src/boot_entries.rs;\n'
        printf '# absent means fall back to LABEL=%s, which is what every earlier install has.\n' "$ROOT_LABEL"
        printf 'root_uuid = "%s"\n' "$root_uuid"
        printf 'esp_partuuid = "%s"\n' "$esp_partuuid"
        [ -n "$data_uuid" ] && printf 'data_uuid = "%s"\n' "$data_uuid"
    } > "$mnt/etc/templaros/install.toml"
fi
# The command line follows the same identity. Everything downstream — the GRUB entry seeded below
# and every one templar-hostd writes after it — reads it from install.toml.
if [ -n "$root_uuid" ]; then
    cmdline="${cmdline/root=LABEL=$ROOT_LABEL/root=UUID=$root_uuid}"
    note "boot command line: $cmdline"
fi

step "Lend the bootstrapped target the host's /dev, /proc, /sys and temporary DNS for chroot steps"
for dir in dev dev/pts proc sys; do
    run mkdir -p "$mnt/$dir"
    mount_owned --bind "/$dir" "$mnt/$dir"
done
if [ -d /sys/firmware/efi/efivars ]; then
    run mkdir -p "$mnt/sys/firmware/efi/efivars"
    mount_owned --bind /sys/firmware/efi/efivars "$mnt/sys/firmware/efi/efivars"
fi
# Ubuntu's systemd-resolved makes the target's /etc/resolv.conf a symlink to
# ../run/systemd/resolve/stub-resolv.conf, and in a target that has never booted /run is an empty
# directory: `touch` and `mount --bind` both follow the dangling link and fail (the first install on
# the reference laptop stopped here). The chroot gets a /run of its own instead — a tmpfs, so
# nothing written there reaches @, and it is gone when the target is unmounted — and the file the
# link names is written into it from the host's *upstream* resolvers, not the host's 127.0.0.53
# stub, which answers only while the live system's resolved does. A package that re-creates the
# link mid-install keeps resolving, because the file it points at is there.
run mkdir -p "$mnt/run"
mount_owned -t tmpfs -o mode=0755,nosuid,nodev tmpfs "$mnt/run"
host_resolv=/run/systemd/resolve/resolv.conf
[ -s "$host_resolv" ] || host_resolv=/etc/resolv.conf
record __chroot_resolver__ "$host_resolv" "$mnt/etc/resolv.conf"
if [ "$apply" = 1 ]; then
    grep -q '^nameserver' "$host_resolv" \
        || die "the live system names no nameserver in $host_resolv, and the chroot steps download packages"
    if [ -L "$mnt/etc/resolv.conf" ]; then
        resolv_link="$(readlink "$mnt/etc/resolv.conf")"
        case "$resolv_link" in /*) ;; *) resolv_link="/etc/$resolv_link" ;; esac
        resolv_path="$(realpath -ms "$resolv_link")"
        case "$resolv_path" in
            /run/*) ;;
            *) die "the target's /etc/resolv.conf points at $resolv_path, outside /run; refusing to write through it" ;;
        esac
        install -D -m 0644 "$host_resolv" "$mnt$resolv_path"
        note "/etc/resolv.conf -> $resolv_path, written into the chroot's own /run from $host_resolv"
    else
        [ -e "$mnt/etc/resolv.conf" ] || : > "$mnt/etc/resolv.conf"
        mount_owned --bind "$host_resolv" "$mnt/etc/resolv.conf"
    fi
else
    printf '     $ __chroot_resolver__ %s %s\n' "$host_resolv" "$mnt/etc/resolv.conf"
    note "a link into /run has its target written into the chroot's /run; a plain file is bind-mounted over"
fi
# Resolved from inside, through the target's own resolver stack, before anything depends on it.
mirror_host="${UBUNTU_MIRROR#*://}"; mirror_host="${mirror_host%%/*}"
in_target getent ahostsv4 "$mirror_host"

step "Sources: Canonical archive plus TemplarOS set $selected_set from $DEVREPO_ROOT"
note "the TemplarOS archive is signed; the target trusts it through its key and nothing else"
run templar dev --repo "$DEVREPO_ROOT" repo verify --set "$selected_set"
note "the whole archive is copied to $TARGET_DEVREPO on the target: the set it installs from is"
note "the set its own \`templar dev status\` will report, and \`templar dev rollback\` has something to go to"
run mkdir -p "$mnt$TARGET_DEVREPO"
if [ -d "$DEVREPO_ROOT" ]; then
    for entry in "$DEVREPO_ROOT"/*; do
        [ -e "$entry" ] || continue
        [ "$(basename "$entry")" = gnupg ] && continue
        run cp -a "$entry" "$mnt$TARGET_DEVREPO/"
    done
else
    note "archive is absent during this dry run; apply will refuse before touching storage"
fi
run install -D -m 0644 "$DEVREPO_ROOT/templaros-dev.gpg" \
    "$mnt/usr/share/keyrings/templaros-archive-keyring.gpg"
note "writing canonical Ubuntu sources, an immutable set pin, and the signed archive-root source"
if [ "$apply" = 1 ]; then
    # The bootstrap's one-line sources.list names the same archive; keeping both makes apt warn
    # about every target twice.
    rm -f "$mnt/etc/apt/sources.list"
    write_apt_config "$mnt" "$TARGET_DEVREPO" /usr/share/keyrings/templaros-archive-keyring.gpg
    printf '%s\n' "$selected_set" > "$mnt$TARGET_DEVREPO/current"
fi

step "Install the TemplarOS package set (contract C1)"
if [ "$resume" = 1 ]; then
    note "resuming: finish whatever dpkg was doing when the earlier attempt stopped"
    in_target dpkg --configure -a
fi
in_target apt-get update
in_target apt-get install -y "${base_packages[@]}"

step "Kernel and NVIDIA modules: Ubuntu-signed only, previous kernel retained"
note "$TEMPLAROS_KERNEL pulls Ubuntu's supported GA/HWE kernel; $TEMPLAROS_NVIDIA pulls the matching"
note "Canonical-signed linux-modules-nvidia LRM. No DKMS, no self-built module, no MOK."
in_target apt-get install -y "${kernel_packages[@]}"
note "keeping the previous kernel installable and selectable is policy, not luck:"
if [ "$apply" = 1 ]; then
    printf 'APT::NeverAutoRemove { "^linux-image-.*"; "^linux-modules-nvidia-.*"; };\n' \
        > "$mnt/etc/apt/apt.conf.d/01templaros-keep-kernels"
fi

step "OS identity"
run install -D -m 0644 "$data_os_release" "$mnt/usr/lib/os-release"

step "Network: NetworkManager manages every device, wired included"
note "Ubuntu's network-manager leaves ethernet unmanaged (10-globally-managed-devices.conf) unless netplan"
note "names NetworkManager as the renderer; without this a USB-C ethernet adapter never gets an address."
in_target sh -c 'install -d -m 0755 /etc/netplan && umask 077 && printf "network:\n  version: 2\n  renderer: NetworkManager\n" > /etc/netplan/01-network-manager-all.yaml'

step "Account: ${username:-<--user was not given>}"
[ -n "$username" ] || note "no --user given; the plan stops short of creating an account"
if [ -n "$username" ]; then
    # Repeatable: a resumed install may already have made the account.
    # `adm` is what reads the system journal and /var/log (journald's ACL grants it, as on Ubuntu):
    # without it `journalctl` shows a user only their own session, never the kernel, so a frozen
    # desktop leaves no evidence its owner can read (`templar doctor freeze`).
    in_target sh -c 'id -u "$1" >/dev/null 2>&1 || useradd --create-home --shell /bin/bash --groups sudo,video,render,adm "$1"' \
        useradd "$username"
    note "reading the new password from stdin during apply; it is never echoed, planned, logged or placed in argv"
    record __password_stdin__ "$username"
    [ "$apply" = 1 ] || printf '     $ __password_stdin__ %s\n' "$username"
    if [ "$apply" = 1 ]; then
        IFS= read -rs new_password
        printf '\n'
        [ -n "$new_password" ] || die "the initial password may not be empty"
        printf '%s:%s\n' "$username" "$new_password" | chroot "$mnt" chpasswd
        unset new_password
    fi
fi
note "root is locked either way: this machine is administered through sudo, not a root login."
in_target passwd --lock root

step "Session units and greeter (the same steps distro/image/mkosi.postinst performs in the image)"
in_target systemctl --global mask xdg-desktop-autostart.target
in_target systemctl --global enable templaros-session.target
in_target systemctl enable templar-hostd.service templar-boot-health.service
in_target install -D -m 0644 /usr/share/templaros/greetd/config.toml /etc/greetd/config.toml
in_target install -D -m 0644 /usr/share/templaros/greetd/pam-login /etc/pam.d/greetd
in_target systemd-sysusers /usr/lib/sysusers.d/templaros-greeter.conf
in_target systemd-tmpfiles --create /usr/lib/tmpfiles.d/templaros-greeter.conf
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
        in_target flatpak install -y --noninteractive --or-update --system "$remote" "$ref"
    done < <(sed 's/#.*//' "$flatpak_refs")
fi

step "Boot path: Canonical-signed shim -> Canonical-signed GRUB -> Ubuntu-signed kernel (ADR-0004)"
in_target apt-get install -y "${boot_packages[@]}"
note "--no-nvram: the firmware entry is registered below, on the host side, and appended rather"
note "than put first. The firmware menu is how the first boots happen; making TemplarOS the default"
note "is a person's decision after installer/CHECKLIST.md passes (efibootmgr -o)."
in_target grub-install --target=x86_64-efi --efi-directory="$ESP_MOUNT" \
    --bootloader-id=TemplarOS --uefi-secure-boot --no-nvram
if [ "$target_kind" = image ] || [ -n "$loop" ]; then
    note "no firmware entry for a file: the QEMU harness boots it through OVMF's menu (EFI/TemplarOS/shimx64.efi)"
else
    esp_disk="/dev/$(lsblk -dno PKNAME "$esp_part" 2>/dev/null || echo DISK)"
    esp_partn="$(lsblk -no PARTN "$esp_part" 2>/dev/null || echo N)"
    # One entry per ESP: an earlier attempt at this install that got this far already made it.
    entry=""
    if [ "$apply" = 1 ]; then
        entry="$(efibootmgr -v | grep -Fi '\EFI\TemplarOS\shimx64.efi' | grep -Fi "$esp_partuuid" \
            | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p' | head -1 || true)"
    fi
    if [ -n "$entry" ]; then
        note "firmware entry Boot$entry already starts this ESP's TemplarOS shim; kept, not duplicated"
    else
        before_entries=""
        [ "$apply" = 0 ] || before_entries="$(efibootmgr | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p' | sort)"
        run efibootmgr --create-only --disk "$esp_disk" --part "$esp_partn" --label TemplarOS \
            --loader '\EFI\TemplarOS\shimx64.efi'
        if [ "$apply" = 1 ]; then
            after_entries="$(efibootmgr | sed -n 's/^Boot\([0-9A-Fa-f]\{4\}\).*/\1/p' | sort)"
            entry="$(comm -13 <(printf '%s\n' "$before_entries") <(printf '%s\n' "$after_entries"))"
            [ "$(printf '%s\n' "$entry" | grep -c .)" = 1 ] \
                || die "cannot identify the one firmware entry this invocation created"
            created="$(efibootmgr -v | grep -i "^Boot${entry}" || true)"
            printf '%s' "$created" | grep -Fqi '\EFI\TemplarOS\shimx64.efi' \
                || die "new Boot$entry does not point at the TemplarOS shim"
            printf '%s' "$created" | grep -Fqi "$esp_partuuid" \
                || die "new Boot$entry does not identify the target ESP $esp_partuuid"
        fi
    fi
    if [ "$apply" = 1 ]; then
        order="$(efibootmgr | sed -n 's/^BootOrder: //p')"
        case ",$order," in
            *",$entry,"*) note "firmware entry Boot$entry is in the boot order: $order" ;;
            *) efibootmgr --bootorder "${order:+$order,}$entry" >/dev/null
               note "firmware entry Boot$entry appended to the boot order: ${order:+$order,}$entry" ;;
        esac
    else
        note "then: efibootmgr --bootorder <current order>,<the new TemplarOS entry> — appended, not first"
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
record __copy_kernel_pair__ "$mnt/boot" "$mnt$ESP_MOUNT$esp_dir"
if [ "$apply" = 1 ]; then
    kernel_version="$(find "$mnt/boot" -maxdepth 1 -type f -name 'vmlinuz-*' -printf '%f\n' \
        | sed 's/^vmlinuz-//' | sort -V | tail -1)"
    [ -n "$kernel_version" ] || die "the target has no installed kernel image"
    [ -f "$mnt/boot/initrd.img-$kernel_version" ] \
        || die "kernel $kernel_version has no matching initramfs"
    cp "$mnt/boot/vmlinuz-$kernel_version" "$mnt$ESP_MOUNT$esp_dir/vmlinuz"
    cp "$mnt/boot/initrd.img-$kernel_version" "$mnt$ESP_MOUNT$esp_dir/initrd.img"
    note "selected kernel/initramfs pair $kernel_version"
else
    printf '     $ __copy_kernel_pair__ %s %s\n' "$mnt/boot" "$mnt$ESP_MOUNT$esp_dir"
    note "select the newest installed kernel with its exact matching initramfs; globs never overwrite one destination"
fi
note "record it as known-good so hostd's first staged candidate does not replace the only entry"
run mkdir -p "$mnt/var/lib/templaros/hostd/grub"
note "writing $GRUB_ENTRIES, in the exact shape templar-hostd regenerates it:"
note "    menuentry 'TemplarOS ($deployment_id) — known good' --id '$deployment_id'"
note "    linux $esp_dir/vmlinuz $cmdline"
if [ "$apply" = 1 ]; then
    printf '%s\n@\n' "$deployment_id" > "$mnt/var/lib/templaros/hostd/grub/known-good"
    mkdir -p "$mnt$(dirname "$GRUB_ENTRIES")"
    cat > "$mnt$GRUB_ENTRIES" <<EOF
# GENERATED by installer/provision.sh — templar-hostd rewrites this file from here on.
menuentry 'TemplarOS ($deployment_id) — known good' --id '$deployment_id' {
    search --no-floppy --set=root --file $esp_dir/vmlinuz
    linux $esp_dir/vmlinuz $cmdline
    initrd $esp_dir/initrd.img
}
EOF
fi
in_target update-grub

step "Diagnostics"
run templar doctor --root "$mnt"
note "then, after the first real boot: templar doctor --hardware, and installer/CHECKLIST.md for the"
note "Secure Boot, module-trust and rollback checks that only a person at the machine can make."

write_state complete

if [ "$apply" = 1 ]; then
    step "Unmount"
    cleanup
    cleanup_enabled=0
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
