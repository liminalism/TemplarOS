#!/usr/bin/env bash
# Build the TemplarOS source package in a *disposable* Ubuntu 26.04 (resolute) root.
#
# Why a clean root at all: `just deb` builds thirteen packages with whatever this developer's
# machine happens to have installed, and a package that only builds here is not a package. This
# lane answers "does the source package build against Ubuntu 26.04 and nothing else?".
#
# Why the tree is copied first: the build-set id carries `.dirty` when the working tree has
# uncommitted changes, and debian/changelog is generated *from* that id. Writing the generated
# changelog into the checkout would therefore change the id it was derived from, so the generated
# entry goes into the copy and never into git.
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

# The id first, but never as a prerequisite for reporting that this lane did not run: a workspace
# that cannot be queried must still produce an honest "not executed" line rather than a stack of
# cargo errors.
id="$(cargo run -q -p xtask -- build-set --print-id 2>/dev/null || echo "unknown")"
echo "source-build: build set $id"
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

# The disposable copy: tracked files only, plus the vendored crates the offline build needs.
work="$(mktemp -d -t templaros-source-build-XXXXXX)"
trap 'rm -rf "$work"' EXIT
src="$work/templaros-${id}"
mkdir -p "$src"
git -C "$root" archive --format=tar HEAD | tar -x -C "$src"
echo "source-build: copied HEAD into $src"

# Vendor into the copy, so the chroot needs no network and no crates.io.
( cd "$src" && cargo vendor --locked vendor >"$src/.cargo-vendor-config" )
mkdir -p "$src/.cargo"
mv "$src/.cargo-vendor-config" "$src/.cargo/config.toml"

# The one version authority: the changelog head entry is generated from the id, in the copy.
cargo run -q -p xtask -- archive changelog --write "$src/debian/changelog"

build_script="set -eu
export DEBIAN_FRONTEND=noninteractive
export TEMPLAROS_BUILD_SET_ID='$id'
apt-get update
apt-get install --yes --no-install-recommends build-essential debhelper devscripts equivs cargo rustc pkg-config
cd /src
dpkg-buildpackage -us -uc -b
"

if have mmdebstrap; then
  echo "source-build: backend mmdebstrap"
  chroot_dir="$work/root"
  mmdebstrap --variant=buildd "$release" "$chroot_dir" \
    "deb http://archive.ubuntu.com/ubuntu $release main universe"
  mkdir -p "$chroot_dir/src"
  cp -a "$src/." "$chroot_dir/src/"
  echo "$build_script" >"$chroot_dir/build.sh"
  chroot "$chroot_dir" /bin/sh /build.sh
else
  echo "source-build: backend podman"
  podman run --rm -v "$src:/src:Z" -w /src "ubuntu:$release" /bin/sh -c "$build_script"
fi

echo "source-build: ok ($id built in a disposable $release root)"
