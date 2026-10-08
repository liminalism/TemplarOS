#!/usr/bin/env bash
# Build a Debian package for an external source registered in distro/forks/manifest.toml.
#
# ADR-0005 says forks are pinned, never vendored, so the source is not in this repository. What is
# in this repository is the *packaging*: distro/forks/<name>/debian/ holds the control file, the
# rules and the changelog, so the package's name, dependencies and install paths are TemplarOS facts
# that `cargo xtask check-packaging` can see, while the code stays in the fork.
#
#   scripts/build-fork-deb.sh nm-tray-2                  # from forks/<name> (just forks-fetch)
#   scripts/build-fork-deb.sh nm-tray-2 --source <dir>   # from a checkout you already have
#   scripts/build-fork-deb.sh nm-tray-2 --dry-run
#   scripts/build-fork-deb.sh liminalview --source <dir-or-archive> --deb-version <build-set-id>
#
# The .deb lands in target/debian/, beside the cargo-deb output, so `just repo-local` picks it up
# with everything else.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
name=""; source_dir=""; dry_run=0; deb_version=""
while [ $# -gt 0 ]; do
    case "$1" in
        --source) source_dir="${2:?--source needs a directory}"; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        --deb-version) deb_version="${2:?--deb-version needs a version}"; shift 2 ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) name="$1"; shift ;;
    esac
done
[ -n "$name" ] || { echo "usage: $0 <fork-name> [--source DIR] [--dry-run]" >&2; exit 2; }

overlay="$root/distro/forks/$name/debian"
[ -d "$overlay" ] || {
    echo "no packaging overlay at distro/forks/$name/debian" >&2
    echo "forks with one: $(cd "$root/distro/forks" && ls -d */debian 2>/dev/null | cut -d/ -f1 | tr '\n' ' ')" >&2
    exit 1
}

# A local-source entry is deliberately not a pretend Git fork. Its builder
# snapshots the complete locked closure before compiling and can rebuild that
# same archive without the original sibling directories.
source_kind="$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1],"rb")).get(sys.argv[2],{}).get("source_kind","git"))' "$root/distro/forks/manifest.toml" "$name")"
if [ "$source_kind" = local ]; then
    args=("$root/scripts/build-local-source-deb.py" "$name")
    [ -z "$source_dir" ] || args+=(--source "$source_dir")
    [ -z "$deb_version" ] || args+=(--deb-version "$deb_version")
    [ "$dry_run" = 0 ] || args+=(--dry-run)
    exec python3 "${args[@]}"
fi
# The manifest is the authority for the pin; read it without a TOML parser, the way fetch.sh does.
pin="$(awk -v want="[$name]" '
    $0 == want { inside = 1; next }
    /^\[/      { inside = 0 }
    inside && $1 == "pin" { gsub(/^[^"]*"|"$/, ""); print; exit }
' "$root/distro/forks/manifest.toml")"

[ -n "$source_dir" ] || source_dir="$root/forks/$name"
[ -d "$source_dir" ] || {
    echo "no source at $source_dir — run 'just forks-fetch', or pass --source" >&2
    exit 1
}
source_dir="$(cd "$source_dir" && pwd)"

# What is actually being built, which is not always what the manifest pins.
head_sha="$(git -C "$source_dir" rev-parse HEAD 2>/dev/null || echo unknown)"
short="$(git -C "$source_dir" rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty=""
if [ -n "$(git -C "$source_dir" status --porcelain 2>/dev/null)" ]; then dirty="+dirty"; fi

echo "fork:    $name"
echo "source:  $source_dir"
echo "head:    $head_sha$dirty"
if [ -n "$pin" ] && [ "$head_sha" != "$pin" ]; then
    echo "warning: this is not the pinned commit ($pin)" >&2
fi

preflight="$root/distro/forks/$name/preflight.sh"
if [ -x "$preflight" ]; then
    echo "preflight:"
    "$preflight" "$source_dir" || { echo "refusing to package $name" >&2; exit 1; }
    echo "  ok"
fi

version="$(awk 'NR==1 { gsub(/^.*\(|\).*$/, ""); print; exit }' "$overlay/changelog")"
stamped="${deb_version:-$version~g$short${dirty:+.dirty}}"

if [ "$dry_run" = 1 ]; then
    echo "would build: $(awk '/^Package:/ { print $2; exit }' "$overlay/control") $stamped"
    exit 0
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/templaros-fork-$name.XXXXXX")"
cleanup() { case "$work" in */templaros-fork-*) rm -rf -- "$work" ;; esac; }
trap cleanup EXIT

# Copy the source, never build in it: the checkout may be the user's own working tree.
# dpkg-buildpackage writes its output beside the source directory, so that is $work and nothing
# else — building straight in a temp root would sweep up whatever .deb happened to be in /tmp.
mkdir -p "$work/src"
tar -C "$source_dir" --exclude=.git --exclude='build*' -cf - . | tar -C "$work/src" -xf -
rm -rf "$work/src/debian"
cp -r "$overlay" "$work/src/debian"

# Stamp the fork's commit onto the version so an installed package names what built it.
sed -i "1s/(${version})/(${stamped})/" "$work/src/debian/changelog"

( cd "$work/src" && dpkg-buildpackage -us -uc -b )

mkdir -p "$root/target/debian"
found=0
for deb in "$work"/*.deb; do
    [ -e "$deb" ] || continue
    mv "$deb" "$root/target/debian/"
    echo "built:   target/debian/$(basename "$deb")"
    found=1
done
[ "$found" = 1 ] || { echo "dpkg-buildpackage produced no .deb" >&2; exit 1; }
