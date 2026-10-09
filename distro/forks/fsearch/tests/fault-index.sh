#!/usr/bin/env bash
# Controlled Index1 failures on an owned bus with no activation directories.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
exec env -u DISPLAY -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u DBUS_SYSTEM_BUS_ADDRESS \
    dbus-run-session --config-file="$here/private-bus.conf" -- env TEMPLAR_WP14_PRIVATE_BUS=1 \
    /usr/bin/python3 "$here/fault-index.py" "$@"
