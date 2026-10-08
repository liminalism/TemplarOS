#!/usr/bin/env bash
set -euo pipefail

# Hotfix the *installed operating system*. This is not the Lubuntu overlay tester
# (`scripts/reinstall-templaros-desktop.sh`): that path overlays TemplarOS packages onto an
# Ubuntu/Lubuntu host and must keep that host's session, display manager and kernel.
# This script refuses to run there, refuses the live ISO (casper is ephemeral), and
# queues a newly built native set through hostd (`templar dev deploy`): the live root,
# session and application profiles stay in place until a verified trial boot.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "$script_dir/.." && pwd -P)"

skip_build=0
dry_run=0
check_host=0

os_release_path="${TEMPLAROS_OS_RELEASE:-/etc/os-release}"
casper_path="${TEMPLAROS_CASPER:-/etc/casper.conf}"
archive_root="${TEMPLAROS_DEVREPO:-/var/lib/templaros/devrepo}"

usage() {
    cat <<'EOF'
Build the native TemplarOS package set and queue a hostd boot candidate.

Usage:
  scripts/update-templaros.sh [OPTIONS]

Options:
  --skip-build       Reuse target/debian from a previous `just deb`.
  --dry-run          Identity checks, then print the deploy plan; change nothing.
  --check-host       Only verify this is an installed TemplarOS, then exit.
  -h, --help         Show this help.

This script refuses:
  - a host whose /etc/os-release is not ID=templaros (use
    scripts/reinstall-templaros-desktop.sh on Lubuntu)
  - the live installation medium (casper); install first, then hotfix

Every native package change, including inherited Ubuntu packages, goes through
`templar update` or this script's hostd candidate. Never run live-root apt upgrade.

Run as your desktop user; sudo is asked only for publishing/queuing the set.
Inspect `templar update status` / `templar update history`; reboot only after the
transaction reaches awaiting-reboot. The existing desktop is never restarted.
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

if ((skip_build == 0)); then
    require_command cargo
    require_command just
    if ! command -v cargo-deb >/dev/null 2>&1; then
        die "cargo-deb is not on PATH. On TemplarOS: cargo install cargo-deb
     (the dev-rust metapackage ships rustup, not cargo-deb — selection.toml says so)"
    fi
fi

cd -- "$repo_root"
export CARGO_TARGET_DIR="$("$repo_root/scripts/native-target-dir.sh")"

if ((skip_build == 0)); then
    section "Build the native package set"
    run just deb
fi

build_set="$repo_root/target/debian/build-set.json"
if ((dry_run == 0 || skip_build == 1)); then
    [[ -f "$build_set" ]] || die "missing $build_set; rerun without --skip-build"
fi

templar_bin=""
if [[ -x "$CARGO_TARGET_DIR/release/templar" ]]; then
    templar_bin="$CARGO_TARGET_DIR/release/templar"
elif command -v templar >/dev/null 2>&1; then
    templar_bin="$(command -v templar)"
elif [[ -x /usr/bin/templar ]]; then
    templar_bin=/usr/bin/templar
fi
[[ -n "$templar_bin" ]] || die "templar CLI not found; just deb should have built $CARGO_TARGET_DIR/release/templar"

deploy_args=(dev --repo "$archive_root" deploy --build-set "$build_set")

section "Queue the signed set as a native boot candidate"
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
fi

if ((dry_run == 1)); then
    printf '\nDry run complete; no package was installed.\n'
else
    cat <<'EOF'

TemplarOS native set queued, not yet installed or promoted.
The running root and desktop were preserved. Inspect `templar update status`
and `templar update history` for staging failures; reboot only after awaiting-reboot.
Boot-health validates the trial and owns promotion/automatic rollback.

For a boot-side rollback, inspect `templar update deployments`, then use `templar update rollback ID`.
EOF
fi
