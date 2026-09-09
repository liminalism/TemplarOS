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

packages=(templar-sessiond templar-session templar-comp templar-greeter templar-shell templar-settings)
build_args=()
for package in "${packages[@]}"; do
    build_args+=(-p "$package")
done
cargo build --release "${build_args[@]}"
for package in "${packages[@]}"; do
    cargo deb -p "$package" --no-build -q
done

install_root="$(mktemp -d /tmp/templaros-package-root.XXXXXX)"
cleanup() {
    case "$install_root" in
        /tmp/templaros-package-root.*) rm -rf -- "$install_root" ;;
        *) echo "refusing to remove unexpected package root: $install_root" >&2 ;;
    esac
}
trap cleanup EXIT

for package in templaros-sessiond templaros-session templaros-comp templaros-greeter templaros-shell templaros-settings; do
    deb="$(find target/debian -maxdepth 1 -type f -name "${package}_*.deb" -print -quit)"
    [ -n "$deb" ] || { echo "package install: FAILED — no .deb for $package" >&2; exit 1; }
    dpkg-deb --extract "$deb" "$install_root"
done

required=(
    usr/libexec/templar-sessiond
    usr/bin/templar-session
    usr/bin/templar-comp
    usr/bin/templar-greeter
    usr/bin/templar-shell
    usr/bin/templar-settings
    usr/lib/systemd/user/templaros-session.target
    usr/lib/systemd/user/templar-comp.service
    usr/lib/systemd/user/templar-shell.service
    usr/share/wayland-sessions/templaros.desktop
    usr/share/templaros/greetd/config.toml
    usr/lib/sysusers.d/templaros-greeter.conf
    usr/lib/tmpfiles.d/templaros-greeter.conf
)
for path in "${required[@]}"; do
    [ -e "$install_root/$path" ] || {
        echo "package install: FAILED — package root is missing /$path" >&2
        exit 1
    }
done

grep -q '^ExecStart=/usr/bin/templar-comp ' "$install_root/usr/lib/systemd/user/templar-comp.service"
grep -q '^ExecStart=/usr/bin/templar-shell ' "$install_root/usr/lib/systemd/user/templar-shell.service"
grep -q '^u templar-greeter ' "$install_root/usr/lib/sysusers.d/templaros-greeter.conf"

echo "package install: ok (${#packages[@]} packages staged into a clean root)"
