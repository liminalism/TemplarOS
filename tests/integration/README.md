# Integration tests

`run.sh` is the hermetic headless smoke: it replaces HOME/XDG state, starts a private session bus,
prevents host application discovery, and asserts the intentionally incomplete fixture is reported
unhealthy rather than treating `--report-only`'s zero exit as a pass.

`nested-session.sh` runs the real winit compositor and trusted graphical shell plus sessiond, filed,
and resourced on a private bus. It drives app discovery, file search, settings mutation/history,
provider resolution, and resource reporting through `lege`. Run it under an existing X display or
with `xvfb-run -a`; CI uses Xvfb.

`greeter.sh` covers the greetd framed-JSON conversation with the crate's fake peer and verifies the
compositor preserves `GREETD_SOCK` and supplies `WAYLAND_DISPLAY`. `package-install.sh` builds the six
login/session-facing packages and extracts them into an empty root, then checks their executable,
unit, session, greetd, sysusers, and tmpfiles contracts.

The remaining `mkosi vm`/real-hardware journey will execute the representative load (browser tab
set + Rust build + cross-drive copy + video + deliberate pressure + monitor hotplug) and assert
pointer/switching/audio/overlay/terminal stay usable.
