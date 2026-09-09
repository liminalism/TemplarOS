# Integration tests

`run.sh` is the hermetic headless smoke: it replaces HOME/XDG state, starts a private session bus,
prevents host application discovery, and asserts the intentionally incomplete fixture is reported
unhealthy rather than treating `--report-only`'s zero exit as a pass.

`nested-session.sh` runs the real winit compositor and trusted graphical shell plus sessiond, filed,
and resourced on a private bus. It drives app discovery, file search, settings mutation/history,
provider resolution, and resource reporting through `templar`. Run it under an existing X display or
with `xvfb-run -a`; CI uses Xvfb.

`greeter.sh` covers the greetd framed-JSON conversation with the crate's fake peer and verifies the
compositor preserves `GREETD_SOCK` and supplies `WAYLAND_DISPLAY`. `package-install.sh` builds the six
login/session-facing packages and extracts them into an empty root, then checks their executable,
unit, session, greetd, sysusers, and tmpfiles contracts.

`task-system.sh` runs the Task System's three doc acceptances against a nested session of its own:
`from-fullscreen-every-time` (enter and leave Taskfield over a fullscreen client N times, and the
scene comes back byte-for-byte), `no-configure-sent` (no `xdg_toplevel.configure` reaches the client
during the mode) and `shell-dead` (kill the shell inside the mode; a card still activates). Two of
the three need the chords, which means XTEST, which goes to whatever X window has focus — so they
refuse to run unless `TEMPLAR_TASKFIELD_ACCEPT_FOCUS=1` says the desktop may be borrowed, and even
then every keystroke is preceded by a check that the nested compositor's own window will receive it.
It is not in `run.sh`: it is a hands-on gate, not a headless one.

The remaining `mkosi vm`/real-hardware journey will execute the representative load (browser tab
set + Rust build + cross-drive copy + video + deliberate pressure + monitor hotplug) and assert
pointer/switching/audio/overlay/terminal stay usable.
