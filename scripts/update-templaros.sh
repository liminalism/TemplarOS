#!/usr/bin/env bash
set -euo pipefail

# Hotfix the *installed operating system*. This is not the Lubuntu overlay tester
# (`scripts/reinstall-templaros-desktop.sh`): that path overlays TemplarOS packages onto an
# Ubuntu/Lubuntu host and must keep that host's session, display manager and kernel.
# This script refuses to run there, refuses the live ISO (casper is ephemeral), and
# installs a newly built native set through `templar dev deploy` onto a TemplarOS
# root (btrfs snapshot, one pinned apt transaction, previous set retained).

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "$script_dir/.." && pwd -P)"

skip_build=0
dry_run=0
check_host=0
auto_rollback=0
snapshot_state=1

os_release_path="${TEMPLAROS_OS_RELEASE:-/etc/os-release}"
casper_path="${TEMPLAROS_CASPER:-/etc/casper.conf}"
archive_root="${TEMPLAROS_DEVREPO:-/var/lib/templaros/devrepo}"

usage() {
    cat <<'EOF'
Build the native TemplarOS package set and install it onto this operating system.

Usage:
  scripts/update-templaros.sh [OPTIONS]

Options:
  --skip-build       Reuse target/debian from a previous `just deb`.
  --auto-rollback    Put the previous set back if a health gate fails.
  --no-snapshot      Do not copy session state stores before installing.
  --dry-run          Identity checks, then print the deploy plan; change nothing.
  --check-host       Only verify this is an installed TemplarOS, then exit.
  -h, --help         Show this help.

This script refuses:
  - a host whose /etc/os-release is not ID=templaros (use
    scripts/reinstall-templaros-desktop.sh on Lubuntu)
  - the live installation medium (casper); install first, then hotfix

Kernel, NVIDIA modules and inherited Ubuntu packages are not this script.
Use `sudo apt-get update && sudo apt-get upgrade` for inherited packages and
`templar update` for kernel/driver transactions.

Run as your desktop user; sudo is asked only for the deploy. Log out to
activate the compositor; reboot only for kernel or driver changes.
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

section() {
    printf '\n== %s ==\n' "$*"
}

run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    if ((dry_run == 0)); then
        "$@"
    fi
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

os_release_id() {
    local path="$1"
    [[ -f "$path" ]] || return 0
    # ID= is the identity line; ID_LIKE= is not.
    sed -n 's/^ID=//p' "$path" | head -n1 | tr -d '"'
}

require_installed_templaros() {
    local id
    id="$(os_release_id "$os_release_path")"
    if [[ "$id" != "templaros" ]]; then
        die "this is not an installed TemplarOS (os-release ID=${id:-missing} at $os_release_path).
     For the Lubuntu overlay session, use scripts/reinstall-templaros-desktop.sh.
     This script updates the operating system, and will not overlay packages onto another one."
    fi
    if [[ -e "$casper_path" ]]; then
        die "this is the live installation medium ($casper_path is present).
     The live session is ephemeral. Install with templaros-install, boot the new
     partitions, then run this script from that installed system."
    fi
}

while (($# > 0)); do
    case "$1" in
        --skip-build)
            skip_build=1
            ;;
        --auto-rollback)
            auto_rollback=1
            ;;
        --no-snapshot)
            snapshot_state=0
            ;;
        --dry-run)
            dry_run=1
            ;;
        --check-host)
            check_host=1
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (use --help)"
            ;;
    esac
    shift
done

require_installed_templaros

if ((check_host == 1)); then
    printf 'host: installed TemplarOS (ID=templaros, not live)\n'
    exit 0
fi

((EUID != 0)) || die "run this script as your desktop user, not with sudo"

[[ -f "$repo_root/Cargo.toml" && -f "$repo_root/justfile" ]] ||
    die "could not locate the TemplarOS repository from $script_dir"

require_command sudo
require_command apt-get

if ((skip_build == 0)); then
    require_command cargo
    require_command just
    if ! command -v cargo-deb >/dev/null 2>&1; then
        die "cargo-deb is not on PATH. On TemplarOS: cargo install cargo-deb
     (the dev-rust metapackage ships rustup, not cargo-deb — selection.toml says so)"
    fi
fi

cd -- "$repo_root"

if ((skip_build == 0)); then
    section "Build the native package set"
    run just deb
fi

build_set="$repo_root/target/debian/build-set.json"
if ((dry_run == 0 || skip_build == 1)); then
    [[ -f "$build_set" ]] || die "missing $build_set; rerun without --skip-build"
fi

templar_bin=""
if [[ -x "$repo_root/target/release/templar" ]]; then
    templar_bin="$repo_root/target/release/templar"
elif command -v templar >/dev/null 2>&1; then
    templar_bin="$(command -v templar)"
elif [[ -x /usr/bin/templar ]]; then
    templar_bin=/usr/bin/templar
fi
[[ -n "$templar_bin" ]] || die "templar CLI not found; just deb should have built target/release/templar"

deploy_args=(dev --repo "$archive_root" deploy --build-set "$build_set")
if ((snapshot_state == 1)); then
    deploy_args+=(--snapshot-state)
fi
if ((auto_rollback == 1)); then
    deploy_args+=(--auto-rollback)
fi

section "Deploy the set onto this operating system"
if ((dry_run == 1)); then
    printf '+ %q' sudo "$templar_bin"
    printf ' %q' "${deploy_args[@]}" --dry-run
    printf '\n'
    # Dry-run the deploy itself when the CLI is present; it prints the pipeline
    # and changes nothing. Missing archive or an unpublished set is still a plan.
    if [[ -x "$templar_bin" && -f "$build_set" ]]; then
        "$templar_bin" "${deploy_args[@]}" --dry-run ||
            warn "templar dev deploy --dry-run exited non-zero; the printed plan above is still the intended command"
    fi
else
    run sudo -v
    run sudo "$templar_bin" "${deploy_args[@]}"
    if ! run /usr/bin/templar doctor --root /; then
        die "the installed doctor check failed; do not log out until its failures are fixed"
    fi
fi

if ((dry_run == 1)); then
    printf '\nDry run complete; no package was installed.\n'
else
    cat <<'EOF'

TemplarOS native set installed.
The compositor was deliberately left running (Restart=no). Log out and back in
to activate a new templar-comp. Shell and most daemons restart as part of
deploy; kernel and NVIDIA changes are not this script — reboot after those.

Roll back this set with: sudo templar dev rollback
EOF
fi
