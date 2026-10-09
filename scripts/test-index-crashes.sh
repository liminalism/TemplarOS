#!/usr/bin/env bash
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
exec env -u DISPLAY -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u DBUS_SYSTEM_BUS_ADDRESS \
    dbus-run-session --config-file="$here/test-index-crashes.conf" -- \
    env TEMPLAR_INDEX_CRASH_PRIVATE_BUS=1 /usr/bin/python3 "$here/test-index-crashes.py" "$@"
