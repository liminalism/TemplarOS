#!/usr/bin/env bash
# Refuse to package a nm-tray-alt checkout the LegeOS shell cannot drive.
#
# The Home Field has no system tray. Its network control launches this binary with `--popup`
# (shell/lege-shell/src/surface/app.rs), and an already-running instance is expected to take the
# request over the session bus rather than start a second copy. A build without that option
# installs cleanly, resolves as the network provider, and then does nothing when clicked — which
# is the worst way to find out, so it is a build failure here instead.
set -euo pipefail
src="$1"
fail=0
if ! grep -q 'QStringLiteral("popup")' "$src/src/main.cpp" 2>/dev/null; then
    echo "  the source at this pin does not implement --popup" >&2
    fail=1
fi
if ! grep -q 'X-LegeOS-Provides' "$src/resources/nm-tray-autostart.desktop" 2>/dev/null; then
    echo "  the autostart entry at this pin carries no X-LegeOS-Provides key" >&2
    fail=1
fi
if [ "$fail" -ne 0 ]; then
    cat >&2 <<'WHY'

  These live only in a working tree at ../not-rust-projects/nm-tray-2 and have never been
  committed to github.com/LegeApp/nm-tray-alt. Commit and push them — ideally as a `legeos`
  branch, which ADR-0005 expects and which this fork does not have — then update `pin` in
  distro/forks/manifest.toml.
WHY
    exit 1
fi
