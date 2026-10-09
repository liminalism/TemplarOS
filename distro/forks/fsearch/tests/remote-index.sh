#!/usr/bin/env bash
# Owned private session bus, fixture, Xvfb and daemon; no installed desktop access.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
exec env -u DISPLAY -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u DBUS_SYSTEM_BUS_ADDRESS \
    dbus-run-session --config-file="$here/private-bus.conf" -- env TEMPLAR_WP14_PRIVATE_BUS=1 \
    /usr/bin/python3 "$here/remote-index.py" "$@"
