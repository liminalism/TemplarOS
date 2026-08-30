#!/usr/bin/env bash
# Build the login/session-facing .debs and extract them into an empty root. This catches missing
# package assets and unit-to-binary mismatches without mutating the developer machine.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

if ! command -v cargo-deb >/dev/null 2>&1 || ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "package install: FAILED — cargo-deb and dpkg-deb are required" >&2
    exit 1
fi

packages=(lege-sessiond lege-session lege-comp lege-greeter lege-shell lege-settings)
build_args=()
for package in "${packages[@]}"; do
    build_args+=(-p "$package")
done
cargo build --release "${build_args[@]}"
for package in "${packages[@]}"; do
    cargo deb -p "$package" --no-build -q
done

install_root="$(mktemp -d /tmp/legeos-package-root.XXXXXX)"
cleanup() {
    case "$install_root" in
        /tmp/legeos-package-root.*) rm -rf -- "$install_root" ;;
        *) echo "refusing to remove unexpected package root: $install_root" >&2 ;;
    esac
}
trap cleanup EXIT

for package in legeos-sessiond legeos-session legeos-comp legeos-greeter legeos-shell legeos-settings; do
    deb="$(find target/debian -maxdepth 1 -type f -name "${package}_*.deb" -print -quit)"
    [ -n "$deb" ] || { echo "package install: FAILED — no .deb for $package" >&2; exit 1; }
    dpkg-deb --extract "$deb" "$install_root"
done

required=(
    usr/libexec/lege-sessiond
    usr/bin/lege-session
    usr/bin/lege-comp
    usr/bin/lege-greeter
    usr/bin/lege-shell
    usr/bin/lege-settings
    usr/lib/systemd/user/legeos-session.target
    usr/lib/systemd/user/lege-comp.service
    usr/lib/systemd/user/lege-shell.service
    usr/share/wayland-sessions/legeos.desktop
    usr/share/legeos/greetd/config.toml
    usr/lib/sysusers.d/legeos-greeter.conf
    usr/lib/tmpfiles.d/legeos-greeter.conf
)
for path in "${required[@]}"; do
    [ -e "$install_root/$path" ] || {
        echo "package install: FAILED — package root is missing /$path" >&2
        exit 1
    }
done

grep -q '^ExecStart=/usr/bin/lege-comp ' "$install_root/usr/lib/systemd/user/lege-comp.service"
grep -q '^ExecStart=/usr/bin/lege-shell ' "$install_root/usr/lib/systemd/user/lege-shell.service"
grep -q '^u lege-greeter ' "$install_root/usr/lib/sysusers.d/legeos-greeter.conf"

echo "package install: ok (${#packages[@]} packages staged into a clean root)"
