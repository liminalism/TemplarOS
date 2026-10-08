#!/usr/bin/env bash
# Fetch/update every fork listed in manifest.toml into ../../forks/<name>. No submodules.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
dest="$root/forks"
mkdir -p "$dest"

# Minimal TOML reader: sections + key = "value" (enough for manifest.toml).
name=""
while IFS= read -r line; do
  line="${line%%#*}"
  [[ -z "${line// }" ]] && continue
  if [[ "$line" =~ ^\[(.+)\]$ ]]; then
    name="${BASH_REMATCH[1]}"; unset fork branch pin; continue
  fi
  if [[ "$line" =~ ^([a-z_]+)[[:space:]]*=[[:space:]]*\"(.*)\"$ ]]; then
    key="${BASH_REMATCH[1]}"; val="${BASH_REMATCH[2]}"
    case "$key" in fork) fork="$val";; branch) branch="$val";; pin) pin="$val";; esac
    if [[ "$key" == "source_kind" && "$val" == "local" ]]; then
      echo "local source $name: not fetched (no release origin/pin); use scripts/build-fork-deb.sh $name --source DIR" >&2
    fi
    if [[ "$key" == "depends_on" || "$key" == "package" ]]; then :; fi
  fi
  if [[ -n "$name" && -n "${fork:-}" && -n "${branch:-}" && "$key" == "pin" ]]; then
    if [[ -d "$dest/$name/.git" ]]; then
      git -C "$dest/$name" fetch --quiet origin "$branch"
    else
      git clone --quiet --branch "$branch" "$fork" "$dest/$name" || git clone --quiet "$fork" "$dest/$name"
    fi
    if [[ -n "${pin:-}" ]]; then git -C "$dest/$name" checkout --quiet "$pin"; else git -C "$dest/$name" checkout --quiet "$branch" && git -C "$dest/$name" pull --quiet --ff-only || true; fi
    echo "fork $name → $(git -C "$dest/$name" rev-parse --short HEAD)"
  fi
done < "$here/manifest.toml"
