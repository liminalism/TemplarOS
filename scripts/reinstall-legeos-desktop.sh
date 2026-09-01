#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "$script_dir/.." && pwd -P)"
nm_tray2_dir="${NM_TRAY2_DIR:-${repo_root}/../not-rust-projects/nm-tray-2}"

skip_build=0
skip_nm_tray2=0
dry_run=0
assume_yes=0
offline=0

usage() {
    cat <<'EOF'
Build, reinstall, and reload the LegeOS desktop-environment packages.

Usage:
  scripts/reinstall-legeos-desktop.sh [OPTIONS]

Options:
  --skip-build          Reuse the existing CMake build and Debian packages.
  --skip-nm-tray2       Do not build or install nm-tray2.
  --nm-tray2-dir PATH   Use a different nm-tray2 source directory.
  --yes                 Let apt repair dependencies without prompting.
  --offline             Build Rust packages without network access.
  --dry-run             Validate inputs and print commands without changing anything.
  -h, --help            Show this help.

The default nm-tray2 path is:
  ../not-rust-projects/nm-tray-2

NM_TRAY2_DIR can also override that path. Run this script as your normal desktop
user; it asks sudo only for the system-wide install steps. It does not terminate
or restart the currently running desktop session.
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

while (($# > 0)); do
    case "$1" in
        --skip-build)
            skip_build=1
            ;;
        --skip-nm-tray2)
            skip_nm_tray2=1
            ;;
        --nm-tray2-dir)
            (($# >= 2)) || die "--nm-tray2-dir requires a path"
            nm_tray2_dir=$2
            shift
            ;;
        --nm-tray2-dir=*)
            nm_tray2_dir=${1#*=}
            ;;
        --yes)
            assume_yes=1
            ;;
        --offline)
            offline=1
            ;;
        --dry-run)
            dry_run=1
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

((EUID != 0)) || die "run this script as your desktop user, not with sudo"
[[ -f "$repo_root/Cargo.toml" && -f "$repo_root/justfile" ]] ||
    die "could not locate the LegeOS repository from $script_dir"

for command_name in sudo dpkg apt-get systemctl; do
    require_command "$command_name"
done

if ((skip_build == 0)); then
    for command_name in cargo cargo-deb just; do
        require_command "$command_name"
    done
fi

cmake_bin=""
nm_tray2_build=""
if ((skip_nm_tray2 == 0)); then
    require_command cmake
    cmake_bin="$(command -v cmake)"
    if ! nm_tray2_dir="$(cd -- "$nm_tray2_dir" 2>/dev/null && pwd -P)"; then
        die "nm-tray2 source directory not found; pass --nm-tray2-dir PATH or --skip-nm-tray2"
    fi
    [[ -f "$nm_tray2_dir/CMakeLists.txt" ]] ||
        die "nm-tray2 CMakeLists.txt not found in $nm_tray2_dir"
    nm_tray2_build="$nm_tray2_dir/build"
fi

cd -- "$repo_root"

if ((skip_build == 0)); then
    if ((skip_nm_tray2 == 0)); then
        section "Build nm-tray2"
        run "$cmake_bin" -S "$nm_tray2_dir" -B "$nm_tray2_build" -DCMAKE_BUILD_TYPE=Release
        run "$cmake_bin" --build "$nm_tray2_build" --parallel
    fi

    section "Build all LegeOS Debian packages"
    if ((offline == 1)); then
        run env CARGO_NET_OFFLINE=true just deb
    else
        run just deb
    fi
else
    section "Reuse existing build artifacts"
fi

if ((skip_nm_tray2 == 0 && dry_run == 0)); then
    [[ -f "$nm_tray2_build/cmake_install.cmake" ]] ||
        die "nm-tray2 is not configured in $nm_tray2_build; rerun without --skip-build"
fi

# The package list is not written down here. distro/deploy/components.toml is the one place it
# lives; cargo xtask, lege doctor and lege dev all read the same file, so a package added to the
# build set cannot be silently left out of a reinstall.
components_toml="$repo_root/distro/deploy/components.toml"
[[ -f "$components_toml" ]] || die "component matrix not found at $components_toml"
mapfile -t expected_packages < <(sed -n 's/^package = "\(.*\)"$/\1/p' "$components_toml")
((${#expected_packages[@]} > 0)) || die "no packages listed in $components_toml"

deb_dir="$repo_root/target/debian"
debs=()
shopt -s nullglob
for package_name in "${expected_packages[@]}"; do
    candidates=("$deb_dir/${package_name}_"*.deb)
    if ((${#candidates[@]} == 0)); then
        if ((dry_run == 1 && skip_build == 0)); then
            debs+=("$deb_dir/${package_name}_<fresh-build>.deb")
            continue
        fi
        die "missing package for $package_name in $deb_dir; rerun without --skip-build"
    fi

    selected=${candidates[0]}
    for candidate in "${candidates[@]:1}"; do
        if [[ "$candidate" -nt "$selected" ]]; then
            selected=$candidate
        fi
    done
    if ((${#candidates[@]} > 1)); then
        warn "multiple builds found for $package_name; using newest: $(basename -- "$selected")"
    fi
    debs+=("$selected")
done
shopt -u nullglob

section "Install nm-tray2 and LegeOS"
run sudo -v
if ((skip_nm_tray2 == 0)); then
    run sudo "$cmake_bin" --install "$nm_tray2_build"
fi

if ! run sudo dpkg -i "${debs[@]}"; then
    warn "dpkg reported an incomplete install; apt will now repair dependencies"
fi

apt_command=(sudo apt-get -f install)
if ((assume_yes == 1)); then
    apt_command+=(-y)
fi
run "${apt_command[@]}"
run sudo dpkg -i "${debs[@]}"

section "Reload user units and verify the installed session"
if ! run systemctl --user daemon-reload; then
    warn "the current user manager was unavailable; the next login will load the new units"
fi

if ((dry_run == 0)) && [[ ! -x /usr/bin/lege ]]; then
    die "/usr/bin/lege was not installed"
fi
if ! run /usr/bin/lege doctor --root /; then
    die "the installed doctor check failed; do not log out until its failures are fixed"
fi

if ((dry_run == 1)); then
    printf '\nDry run complete; no build or installation command was executed.\n'
else
    cat <<'EOF'

LegeOS desktop environment reinstalled successfully.
The live compositor and shell were deliberately left running. Log out, choose
LegeOS in the session menu, and log back in to run the new binaries.
EOF
fi
