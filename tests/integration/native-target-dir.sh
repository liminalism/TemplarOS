#!/usr/bin/env bash
# Target selection is independent of the mounted checkout and honors packaging's explicit path.
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
printf 'ID=templaros\nVERSION_ID=26.10\n' >"$fixture/templaros"
printf 'ID=ubuntu\nVERSION_ID=26.04\n' >"$fixture/lubuntu"
native=$(env -u CARGO_TARGET_DIR TEMPLAROS_OS_RELEASE="$fixture/templaros" "$root/scripts/native-target-dir.sh")
host=$(env -u CARGO_TARGET_DIR TEMPLAROS_OS_RELEASE="$fixture/lubuntu" "$root/scripts/native-target-dir.sh")
[ "$native" = "$root/target/native/templaros-26.10" ]
[ "$host" = "$root/target/native/ubuntu-26.04" ]
[ "$native" != "$host" ]
explicit=$(CARGO_TARGET_DIR="$root/target" TEMPLAROS_OS_RELEASE="$fixture/lubuntu" "$root/scripts/native-target-dir.sh")
[ "$explicit" = "$root/target" ]
printf 'native target selection: ok (distinct OS paths, explicit packaging override preserved)\n'
