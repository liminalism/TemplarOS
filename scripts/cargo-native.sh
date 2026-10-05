#!/usr/bin/env bash
# Use the same native target selector as just and the nested launcher.
set -euo pipefail
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
export CARGO_TARGET_DIR=$("$here/native-target-dir.sh")
exec cargo "$@"
