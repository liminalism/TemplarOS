#!/usr/bin/env bash
# Build the native package set inside an Ubuntu 26.04 (resolute) chroot, without root.
#
# Why: `just deb` on the development host links against the host's libraries. The host is not
# resolute, and the first bootstrap of the medium found the consequence: templaros-comp depended on
# libdisplay-info2, which resolute does not have (it ships libdisplay-info3). A native package is a
# package for the target release, so it is built in the target release. This is also the shape a
# self-hosting TemplarOS uses for the rebuilt tier later (ISO-PLAN.md §5).
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

work="${TEMPLAROS_BUILD_DIR:-/var/tmp/templaros-build/@build}"
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
# Everything the build writes lives under the chroot, which mmdebstrap made owned by the
# namespace's root: the helper maps only the subordinate uid range, so a directory made here by
# the calling user is nobody's in there and cannot be written. Results are readable from outside.
build_dir="$chroot_dir/build"
target_dir="$build_dir/target"
out_dir="$build_dir/debs"

# Everything the workspace links, as -dev packages. debian/control's Build-Depends names only
# sqlite; the rest are what the built packages' own Depends resolve to, as -dev.
# libclang-dev and libpam0g-dev are not linked libraries: templar-shell's PAM binding (pam-sys) runs
# bindgen over PAM's headers at build time. Without them `cargo deb -p templar-shell` fails in the
# chroot while succeeding on a developer host that happens to have clang and the PAM headers.
dev_packages="build-essential pkgconf rustup just git curl ca-certificates libsqlite3-dev
    libclang-dev libpam0g-dev libdisplay-info-dev libgbm-dev libinput-dev libpixman-1-dev libseat-dev
    libudev-dev libxkbcommon-dev libegl-dev libgles-dev libwayland-dev"

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
# The tree belongs to an unmapped uid from in here; git must still answer for the build-set id.
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'
apt-get -qq update
apt-get -qq install -y --no-install-recommends $(echo $dev_packages | tr '\n' ' ')
cd "$root"
# The toolchain the tree pins (rust-toolchain.toml); Ubuntu's rustup fetches it once into RUSTUP_HOME.
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install "\$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)" --profile minimal -c rustfmt,clippy
command -v cargo-deb >/dev/null || cargo install --locked cargo-deb
# The preview-codecs helper re-resolves its lockfile offline; populate the registry first.
cargo fetch --manifest-path helpers/templar-preview-codecs/Cargo.toml
echo "build-in-resolute: \$(rustc --version) on \$(. /etc/os-release && echo \$PRETTY_NAME)"
just deb
cp -a target/debian/. /build/debs/
INNER
chmod 755 "$inner"

echo "build-in-resolute: entering $chroot_dir (tree at $root, target on $target_dir)"
mmdebstrap --unshare-helper /bin/sh -c '
    set -eu
    chroot_dir="$1"; projects="$2"; root="$3"; target_dir="$4"; inner="$5"; keep_target="$6"
    mkdir -p "$chroot_dir$projects" "$chroot_dir/build/cargo-home" "$chroot_dir/build/rustup-home" \
        "$chroot_dir/build/debs" "$target_dir"
    [ "$keep_target" = 1 ] || find "$target_dir" -mindepth 1 -delete
    cp "$inner" "$chroot_dir/build/build-inner.sh"
    mount --bind "$projects" "$chroot_dir$projects"
    mount --bind "$target_dir" "$chroot_dir$root/target"
    mount -t proc proc "$chroot_dir/proc"
    # sysfs and the host devices are conveniences a build does not need; a user namespace may
    # refuse them, and mmdebstrap left a usable /dev in the chroot already.
    mount --rbind /sys "$chroot_dir/sys" 2>/dev/null || true
    mount --rbind /dev "$chroot_dir/dev" 2>/dev/null || true
    mount -t devpts devpts "$chroot_dir/dev/pts" 2>/dev/null || true
    cp /etc/resolv.conf "$chroot_dir/etc/resolv.conf"
    exec chroot "$chroot_dir" /build/build-inner.sh
' -- "$chroot_dir" "$projects" "$root" "$target_dir" "$inner" "$keep_target"

echo "build-in-resolute: packages in $out_dir"
ls "$out_dir"/*.deb | sed 's,.*/,  ,'
