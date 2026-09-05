#!/usr/bin/env bash
# Build the native package set inside an Ubuntu 26.04 (resolute) chroot, without root.
#
# Why: `just deb` on the development host links against the host's libraries. The host is not
# resolute, and the first bootstrap of the medium found the consequence: legeos-comp depended on
# libdisplay-info2, which resolute does not have (it ships libdisplay-info3). A native package is a
# package for the target release, so it is built in the target release. This is also the shape a
# self-hosting LegeOS uses for the rebuilt tier later (ISO-PLAN.md §5).
#
# The chroot is made once by mmdebstrap in unshare mode and re-entered with its --unshare-helper;
# the source tree (and its sibling path dependencies) are bind-mounted at their host paths, an
# empty directory on the build volume is mounted over target/ so nothing built on the host is
# reused and nothing built here lands on the host's NTFS target, and the results are copied out.
#
#   scripts/build-in-resolute.sh [--work DIR] [--keep-target]
#
# Needs: mmdebstrap (unshare mode working: `mmdebstrap --mode=unshare --simulate resolute /dev/null`).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
projects="$(cd "$root/.." && pwd)"   # ../pixelkit, ../bpg-rs, ../jpegXL-rs, ../Lege-ecosystem live here

work="${LEGEOS_BUILD_DIR:-/var/tmp/legeos-build/@build}"
keep_target=0
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="${2:?}"; shift 2 ;;
        --keep-target) keep_target=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "build-in-resolute: unknown argument $1" >&2; exit 1 ;;
    esac
done
die() { printf 'build-in-resolute: %s\n' "$*" >&2; exit 1; }
command -v mmdebstrap >/dev/null || die "mmdebstrap is not installed"
mkdir -p "$work" || die "cannot create $work (mount the build volume: ISO-PLAN.md P0.2)"

chroot_dir="$work/buildroot"
target_dir="$work/target-resolute"
cargo_home="$work/cargo-home"
rustup_home="$work/rustup-home"
out_dir="$work/debs"
mkdir -p "$target_dir" "$cargo_home" "$rustup_home" "$out_dir"
[ "$keep_target" = 1 ] || rm -rf "${target_dir:?}"/* 

# Everything the workspace links, as -dev packages. debian/control's Build-Depends names only
# sqlite; the rest are what the built packages' own Depends resolve to, as -dev.
dev_packages="build-essential pkgconf rustup just git curl ca-certificates libsqlite3-dev
    libdisplay-info-dev libgbm-dev libinput-dev libpixman-1-dev libseat-dev libudev-dev
    libxkbcommon-dev libegl-dev libgles-dev libwayland-dev"

if [ ! -x "$chroot_dir/usr/bin/apt-get" ]; then
    echo "build-in-resolute: creating $chroot_dir"
    # shellcheck disable=SC2086
    mmdebstrap --mode=unshare --variant=apt --architectures=amd64 \
        --components=main,universe,restricted \
        --include="$(echo $dev_packages | tr ' ' ',')" resolute "$chroot_dir" \
        "deb http://archive.ubuntu.com/ubuntu resolute main universe restricted" \
        "deb http://archive.ubuntu.com/ubuntu resolute-updates main universe restricted" \
        "deb http://security.ubuntu.com/ubuntu resolute-security main universe restricted"
fi

inner="$work/build-inner.sh"
cat > "$inner" <<INNER
#!/bin/sh
# Runs as (unshared) root inside the resolute chroot.
set -eu
export DEBIAN_FRONTEND=noninteractive
export CARGO_HOME=/build/cargo-home RUSTUP_HOME=/build/rustup-home
export PATH=/build/cargo-home/bin:\$PATH
apt-get -qq update
apt-get -qq install -y --no-install-recommends $(echo $dev_packages | tr '\n' ' ')
cd "$root"
# The toolchain the tree pins (rust-toolchain.toml); Ubuntu's rustup fetches it once into RUSTUP_HOME.
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install "\$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)" --profile minimal -c rustfmt,clippy
command -v cargo-deb >/dev/null || cargo install --locked cargo-deb
# The preview-codecs helper re-resolves its lockfile offline; populate the registry first.
cargo fetch --manifest-path helpers/lege-preview-codecs/Cargo.toml
echo "build-in-resolute: \$(rustc --version) on \$(. /etc/os-release && echo \$PRETTY_NAME)"
just deb
cp -a target/debian/. /build/debs/
INNER
chmod 755 "$inner"

echo "build-in-resolute: entering $chroot_dir (tree at $root, target on $target_dir)"
mmdebstrap --unshare-helper /bin/sh -c '
    set -eu
    chroot_dir="$1"; projects="$2"; root="$3"; target_dir="$4"; work="$5"
    mkdir -p "$chroot_dir$projects" "$chroot_dir/build"
    mount --bind "$projects" "$chroot_dir$projects"
    mount --bind "$target_dir" "$chroot_dir$root/target"
    mount --bind "$work" "$chroot_dir/build"
    mount -t proc proc "$chroot_dir/proc"
    mount --bind /sys "$chroot_dir/sys"
    mount --bind /dev "$chroot_dir/dev"
    mount -t devpts devpts "$chroot_dir/dev/pts" 2>/dev/null || true
    cp /etc/resolv.conf "$chroot_dir/etc/resolv.conf"
    exec chroot "$chroot_dir" /build/build-inner.sh
' -- "$chroot_dir" "$projects" "$root" "$target_dir" "$work"

echo "build-in-resolute: packages in $out_dir"
ls "$out_dir"/*.deb | sed 's,.*/,  ,'
