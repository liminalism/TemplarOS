# Integration tests

`run.sh` is the hermetic headless smoke: it replaces HOME/XDG state, starts a private session bus,
prevents host application discovery, and asserts the intentionally incomplete fixture is reported
unhealthy rather than treating `--report-only`'s zero exit as a pass.

`nested-session.sh` runs the real winit compositor and trusted graphical shell plus sessiond, filed,
and resourced on a private bus. It drives app discovery, file search, settings mutation/history,
provider resolution, and resource reporting through `templar`. Run it under an existing X display or
with `xvfb-run -a`; CI uses Xvfb.

The opt-in input journey uses actual built Settings and Files clients plus the real shell. It
asserts activation stays unchanged while Alt is held, changes to the selected window on release,
then parks it, reveals Home and clicks its shell-published card to restore it. There is no XTEST
or host focus requirement: input goes only to the explicitly enabled private nested socket.
After building the workspace, run:

```sh
export CARGO_TARGET_DIR="$(scripts/native-target-dir.sh)"
xvfb-run -a env TEMPLAR_NESTED_SKIP_BUILD=1 TEMPLAR_NESTED_INPUT_JOURNEY=1 \
  TEMPLAROS_NESTED_BIN_DIR="$CARGO_TARGET_DIR/debug" tests/integration/nested-session.sh
```

This wrapper creates a private bus and HOME/XDG runtime/state, keeps failed-run logs, and never
starts or stops user units. The developer launcher `scripts/nested-session.sh` separately supports
Wayland or X11 hosts, `TEMPLAROS_HOST_WAYLAND` / `TEMPLAROS_HOST_DISPLAY`, and
`TEMPLAROS_NESTED_BIN_DIR`. It deliberately still refuses to run over a live installed TemplarOS
session because it shares the user's bus; use the isolated integration wrapper instead.
The CLI pipeline assertions drain stdout before reporting a match. An early-closing `grep -q`
can race Rust's stdout writer and turn a successful JSON response into a spurious broken-pipe
panic; this is a harness failure, not a desktop or D-Bus failure.

`greeter.sh` covers the greetd framed-JSON conversation with the crate's fake peer and verifies the
compositor preserves `GREETD_SOCK` and supplies `WAYLAND_DISPLAY`. `package-install.sh` builds the six
login/session-facing packages and extracts them into an empty root, then checks their executable,
unit, session, greetd, sysusers, and tmpfiles contracts. `update-templaros.sh` checks that
`scripts/update-templaros.sh` refuses a Lubuntu overlay host and a casper live session, and accepts
an `ID=templaros` os-release without casper.

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

## Native deployment/account contracts

`python3 tests/integration/test-deployment-contract.py` exercises the real helper
parsers and an owned temporary tree without root, live buses or services: sealed
bounded secret FD/colon handling, malformed/future/Work-root intent refusal,
package DB/Work state-bind exclusion, offline GPU-mode-only preservation and
pre-write legacy whole-/var refusal. Hostd/CLI Rust tests cover native queue
identity, real factory wiring over an in-process private D-Bus peer, interrupted
transaction identity and future-schema backups.

These are **not** Btrfs property, native PAM policy, loop/ESP, signature/module or
physical boot proofs. Run the owned disposable-media and hardware cases in
`installer/CHECKLIST.md` before adopting explicit RO mode. Default stays RW.
