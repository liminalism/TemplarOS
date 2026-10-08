#!/usr/bin/env bash
# Build the TemplarOS source package in a *disposable* Ubuntu 26.04 (resolute) root.
#
# Why a clean root at all: `just deb` builds thirteen packages with whatever this developer's
# machine happens to have installed, and a package that only builds here is not a package. This
# lane answers "does the source package build against Ubuntu 26.04 and nothing else?".
#
# Build only the explicitly pinned complete source closure, never the current
# checkout or host caches. Generate identity and changelog inside the guest with
# one epoch, before building the binary packages from that same extracted tree.
#
# This script never touches the host's package state, never installs anything outside the
# throwaway root, and exits 0 while printing NOT EXECUTED when neither backend is available. A
# printed plan is not a build and is never reported as one.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

release="resolute"
plan_only="no"
[ "${1:-}" = "--plan" ] && plan_only="yes"

have() { command -v "$1" >/dev/null 2>&1; }

# Reporting an unavailable lane never compiles host tools or resolves host siblings.
echo "source-build: pinned workspace archive ${TEMPLAROS_WORKSPACE_SOURCE_SHA256:-not supplied}"
echo "source-build: target root ubuntu:$release (disposable)"

if [ "$plan_only" = "yes" ]; then
  echo "source-build: NOT EXECUTED (--plan)"
  exit 0
fi

if ! have mmdebstrap && ! have podman; then
  echo "source-build: NOT EXECUTED (neither mmdebstrap nor podman is installed on this host)"
  echo "source-build:   install one of them to run this lane:"
  echo "source-build:     sudo apt-get install mmdebstrap    # chroot backend"
  echo "source-build:     sudo apt-get install podman        # container backend"
  echo "source-build:   nothing was built and nothing is claimed to have passed."
  exit 0
fi

# Close actual locked input content, including sibling assets and the independent
# preview graph. HEAD-only copying silently discarded dirty source and all siblings.
# A retained archive is an explicit exact-content pin, never "latest".
# Sources, vendor data, the guest toolchain and release objects need a real disk,
# not an implicitly quota-limited /tmp tmpfs. An explicit TMPDIR remains honored.
work_root="${TMPDIR:-$root/target/native/ubuntu-26.04/source-builds}"
mkdir -p "$work_root"
work="$(mktemp -d "$work_root/templaros-source-build-XXXXXX")"
trap 'rm -rf "$work"' EXIT
archive="${TEMPLAROS_WORKSPACE_SOURCE_ARCHIVE:-}"
digest="${TEMPLAROS_WORKSPACE_SOURCE_SHA256:-}"
if [ -z "$archive" ]; then
  echo "source-build: a complete retained source archive and trusted SHA256 are required"
  echo "source-build: run just source-archive, then set TEMPLAROS_WORKSPACE_SOURCE_ARCHIVE and TEMPLAROS_WORKSPACE_SOURCE_SHA256"
  exit 1
fi
python3 scripts/build-local-source-deb.py workspace --source "$archive" \
  --expect-sha256 "$digest" --extract "$work/closure"
src="$(python3 -c 'import json,sys; from pathlib import Path; p=Path(sys.argv[1]); print(p/"sources"/json.loads((p/"source-provenance.json").read_text())["application"])' "$work/closure")"
echo "source-build: prepared pinned closure $digest"

# Fix the invocation's epoch once. Neither xtask call may choose a different second.
epoch="${SOURCE_DATE_EPOCH:-$(date +%s)}"
case "$epoch" in ''|*[!0-9]*) echo "SOURCE_DATE_EPOCH must be an unsigned integer" >&2; exit 1;; esac
application="$(basename "$src")"
cat >"$work/build.sh" <<'BUILD'
#!/bin/sh
set -eu
application="$1"
epoch="$2"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install --yes --no-install-recommends build-essential devscripts equivs rustup python3 ca-certificates
cd "/closure/sources/$application"
# No host Cargo/Rustup state is mounted. Registry and Git sources stay offline;
# only guest OS dependencies and the declared compiler are connected prerequisites.
export CARGO_HOME="$PWD/debian/cargo_home"
export CARGO_TARGET_DIR="$PWD/target"
export CARGO_NET_OFFLINE=true
export CARGO_BUILD_JOBS=1
export CARGO_INCREMENTAL=0
export RUSTUP_HOME=/closure/.rustup
export SOURCE_DATE_EPOCH="$epoch"
toolchain="$(python3 -c 'import tomllib; print(tomllib.load(open("rust-toolchain.toml", "rb"))["toolchain"]["channel"])')"
export RUSTUP_TOOLCHAIN="$toolchain"
rustup toolchain install "$toolchain" --profile minimal
dependencies="$(python3 scripts/build-local-source-deb.py workspace --source "$PWD" --native-build-depends)"
printf 'Source: templaros-native-prerequisites\nSection: devel\nPriority: optional\nMaintainer: TemplarOS contributors <dev@templaros.org>\nBuild-Depends: %s\n' "$dependencies" >/closure/native-build-deps.control
mk-build-deps --install --remove --tool "apt-get --yes --no-install-recommends" /closure/native-build-deps.control
# Reuse the independently checked native-toolchain route, not dpkg -d or a
# fictional package claiming that Rustup is a distro rustc/cargo installation.
python3 scripts/build-local-source-deb.py workspace --source "$PWD" --check-build-environment >/closure/build-environment.json
id="$(cargo run --offline --locked -q -p xtask -- build-set --print-id --epoch "$epoch")"
export TEMPLAROS_BUILD_SET_ID="$id"
cargo run --offline --locked -q -p xtask -- archive changelog --epoch "$epoch" --write debian/changelog
printf '%s\n' "$id" >/closure/build-set-id
debian/rules binary
BUILD

if have mmdebstrap; then
  echo "source-build: backend mmdebstrap"
  chroot_dir="$work/root"
  mmdebstrap --variant=buildd "$release" "$chroot_dir" \
    "deb http://archive.ubuntu.com/ubuntu $release main universe"
  mkdir -p "$chroot_dir/closure"
  cp -a "$work/closure/." "$chroot_dir/closure/"
  cp "$work/build.sh" "$chroot_dir/build.sh"
  chroot "$chroot_dir" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root \
    /bin/sh /build.sh "$application" "$epoch"
  cp -a "$chroot_dir/closure/sources/"*.deb "$work/closure/sources/"
  cp "$chroot_dir/closure/build-set-id" "$chroot_dir/closure/build-environment.json" "$work/closure/"
else
  echo "source-build: backend podman"
  podman run --rm -v "$work/closure:/closure:Z" -v "$work/build.sh:/build.sh:ro,Z" \
    "ubuntu:$release" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root \
    /bin/sh /build.sh "$application" "$epoch"
fi

# The disposable root is removed on exit; retain only real packages and their
# exact-input/compiler provenance outside it.
id="$(cat "$work/closure/build-set-id")"
output="$root/target/source-build/$digest/$epoch"
mkdir -p "$(dirname "$output")"
mkdir "$output"
cp -a "$work/closure/sources/"*.deb "$output/"
cp "$work/closure/source-provenance.json" "$work/closure/build-environment.json" \
  "$work/closure/build-set-id" "$output/"
printf '%s  %s\n' "$digest" "$(basename "$archive")" >"$output/source-archive.sha256"
echo "source-build: ok ($id built in a disposable $release root)"
echo "source-build: packages and provenance retained at $output"
