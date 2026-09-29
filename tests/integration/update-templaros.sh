#!/usr/bin/env bash
# Identity gates of scripts/update-templaros.sh: Lubuntu overlay vs live ISO vs installed OS.
#
# Does not build packages and does not touch apt. Every case uses a fake os-release
# (and, for the live case, a fake casper.conf) so it is safe on the development host.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
script="$root/scripts/update-templaros.sh"

fail() { echo "update-templaros: FAILED — $*" >&2; exit 1; }
ok() { echo "update-templaros: $*"; }

[ -x "$script" ] || fail "scripts/update-templaros.sh is missing or not executable"

work="$(mktemp -d /tmp/templaros-update-os.XXXXXX)"
cleanup() {
    case "$work" in
        /tmp/templaros-update-os.*) rm -rf -- "$work" ;;
        *) echo "refusing to remove unexpected work directory: $work" >&2 ;;
    esac
}
trap cleanup EXIT

# --- --help works on any host ---------------------------------------------------------------------

help_out="$("$script" --help)"
printf '%s\n' "$help_out" | grep -q 'installed TemplarOS' \
    || fail "--help did not describe the installed-OS path"
printf '%s\n' "$help_out" | grep -q 'reinstall-templaros-desktop.sh' \
    || fail "--help did not name the Lubuntu overlay script as the other path"
ok "--help names both the installed-OS path and the Lubuntu overlay"

# --- Lubuntu / Ubuntu overlay is refused ----------------------------------------------------------

printf 'ID=ubuntu\nID_LIKE=debian\n' >"$work/ubuntu-os-release"
ubuntu_out="$(
    TEMPLAROS_OS_RELEASE="$work/ubuntu-os-release" \
        TEMPLAROS_CASPER="$work/no-such-casper" \
        "$script" --check-host 2>&1
)" && fail "ID=ubuntu was accepted as an installed TemplarOS:
$ubuntu_out"
printf '%s\n' "$ubuntu_out" | grep -q 'reinstall-templaros-desktop.sh' \
    || fail "the Lubuntu refusal did not name scripts/reinstall-templaros-desktop.sh:
$ubuntu_out"
printf '%s\n' "$ubuntu_out" | grep -q 'not an installed TemplarOS' \
    || fail "the Lubuntu refusal did not say this is not an installed TemplarOS:
$ubuntu_out"
ok "ID=ubuntu is refused and the overlay script is named"

# --- live medium is refused -----------------------------------------------------------------------

printf 'ID=templaros\nID_LIKE="ubuntu debian"\n' >"$work/templaros-os-release"
: >"$work/casper.conf"
live_out="$(
    TEMPLAROS_OS_RELEASE="$work/templaros-os-release" \
        TEMPLAROS_CASPER="$work/casper.conf" \
        "$script" --check-host 2>&1
)" && fail "a casper live session was accepted as an installed TemplarOS:
$live_out"
printf '%s\n' "$live_out" | grep -q 'live installation medium' \
    || fail "the live refusal did not name the live medium:
$live_out"
ok "ID=templaros with casper.conf is refused as the live medium"

# --- installed TemplarOS is accepted --------------------------------------------------------------

installed_out="$(
    TEMPLAROS_OS_RELEASE="$work/templaros-os-release" \
        TEMPLAROS_CASPER="$work/no-such-casper" \
        "$script" --check-host 2>&1
)" || fail "ID=templaros without casper was refused:
$installed_out"
printf '%s\n' "$installed_out" | grep -q 'installed TemplarOS' \
    || fail "the success line did not say installed TemplarOS:
$installed_out"
ok "ID=templaros without casper is accepted as the installed OS"

echo "update-templaros: ok"
