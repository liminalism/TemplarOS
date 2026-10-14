# Testing TemplarOS

For a planned comparison of daily-use performance and memory use with LXQt on
both distributions, see the [Lubuntu and TemplarOS performance comparison protocol](lubuntu-templaros-lxqt-performance-comparison.md).
It defines future benchmarks and reporting criteria; it contains no benchmark results.

There are four ways to run this, and they differ by how much you have to commit to. Start at the
top; only go down when the thing you need to test requires it.

The planning analysis (`docs/planning/01-initial-shape.md`) staged it this way deliberately. Stage
1 is "a replacement session on Lubuntu 26.04", using an existing install "only as the development
host". So the desktop-environment form below is **scaffolding, not the product** — it exists so
the work is testable long before there is an installer, and it does not commit TemplarOS to shipping
as a desktop environment on someone else's distribution.

| | What | What it costs | What it can prove |
|---|---|---|---|
| 1 | Nested compositor in your current session | nothing | protocols, geometry, recovery, chrome |
| 2 | Install the `.deb`s and pick **TemplarOS** at the login screen | packages on your current Ubuntu | the whole session, greeter included |
| 3 | mkosi disk image in a VM | mkosi + network | the image, first boot, the installer path |
| 4 | Real install on real hardware | a machine | DRM/KMS, both GPU paths, suspend, hotplug |

There is also a fifth thing that is not a "way to run it" but catches more than any of them:
`cargo test --workspace`, `tests/integration/run.sh`, and `templar-comp --check`. Those run anywhere,
including where there is no display at all, and they are what CI has.

## 1. Nested — no installation

```sh
cargo build --workspace
./target/debug/templar-comp --backend winit
```

`--backend winit` is the default whenever `WAYLAND_DISPLAY` or `DISPLAY` is already set, so inside
a desktop session the flag is redundant; it is spelled out here because being explicit about which
backend is running matters when something misbehaves.

It logs two socket names. Point a client at the first:

```sh
WAYLAND_DISPLAY=wayland-N weston-terminal      # or cosmic-term, foot, alacritty…
```

The second socket, `templaros-shell-wayland-N`, is the private one. Connecting there — and only there — is
what makes a client the trusted shell, which is the sole way to see the `templar_shell_v1` and
`templar_recovery_v1` protocols. Nothing a client can *say* grants that trust.

What to exercise by hand, because no test covers it:

* Drag a window mostly off-screen and let go. It must come back reachable.
* Change the nested window's size, which the compositor sees as an output geometry change. Every
  window must remain recoverable.
* Minimise a window (its own button, or `xdg_toplevel.set_minimized`). It must vanish entirely —
  not render, not take clicks where it was — and `Ctrl+Alt+Space` must bring it back.
* Maximise a window with its own button. It must fill the output and come back to its old size on
  unmaximise; Home itself has no taskbar or exclusive bottom strip.
* With two or three windows open, tap `Super` (or press `Ctrl+Alt+H`; `Super+D` remains a fallback).
  Every window must disappear behind the Home Surface with nothing else changing; `Escape`, the
  same key again, or clicking a window's entry must bring the exact prior stack back.
* **Kill `templar-shell` while revealed.** The windows must reappear on their own.
* On the Home Field (`WAYLAND_DISPLAY=templaros-shell-wayland-N templar-shell --surface`): click an application
  tile's body — it launches; click an open window's tile — it comes to the front (and any reveal
  ends); press its `park` control — it vanishes from the screen and its tile reads *parked*; click
  the parked tile — it comes back at its old geometry. Drag a tile by its title band: a ghost shows
  where it will land before you let go, and only tiles in that row or column make room. Drop a tile
  onto another and the label reads `NEW PROJECT FROM THESE TWO`; click the new project's body to zoom
  into it, move its tiles around inside, Backspace one back out, Escape to return. Ctrl+Z reverses
  each of these. A parked window's tile shows a snapshot of it (set `shell.home-field.previews`
  to `live` for ~1 fps updates on every session tile; `off` for icons only) — check the image is
  the right way up, and that an ordinary client connected to the *public* socket is not offered
  `ext_image_copy_capture_manager_v1` at all (`WAYLAND_DEBUG=1 weston-info` lists the globals).
  Resize the Downloads tile to two rows (Space, then arrows are move; resize is a later control —
  for now change `shell.home-field.tile-size`): tall, it shows a cushion treemap of `~/Downloads`
  with area proportional to bytes; compact, a colour band of its composition.
  Then do the same with the keyboard only: arrows, Enter, Space+arrows+Enter, Delete, Ctrl+Z.
  `templar-shell --print field` must list the same tiles.
* **Kill `templar-shell` and do it all again.** This is the point of the architecture: recovery is
  the compositor's job, not the shell's.

| Keys | What it does | Works with the shell dead |
|---|---|---|
| `Ctrl+Alt+Space` | show every window, scattered — shelved ones included | yes |
| `Ctrl+Alt+Escape` | escape a fullscreen or modal window | yes |
| `Alt+Tab` / `Alt+Shift+Tab` | compositor's fallback switcher | yes |
| bare `Super` tap | toggle Home, as the primary no-taskbar window-switching route | yes (shows the bare backdrop) |
| `Ctrl+Alt+H`, `Super+D` | fallback routes for the same Home toggle | yes (shows the bare backdrop) |
| `Ctrl+Alt+P` | park the active window onto Home's shelf | yes |
| `Escape` while revealed | ends the reveal | yes |

These call `WindowService` directly rather than going over the wire protocol, which is why they
keep working when there is no shell to route them through. A reveal is a view, not a state: it
shelves nothing and moves nothing, so there is never anything to undo when it ends.

## 2. Installed packages — the easiest full session

This is the "desktop environment" form. You install TemplarOS's packages onto the Ubuntu you already
have, and a **TemplarOS** entry appears in your display manager's session list.

For repeat development installs, the repository script rebuilds nm-tray2 and every TemplarOS package,
installs the exact package set, repairs dependencies, reloads the user manager, and runs the installed
doctor check:

```sh
scripts/reinstall-templaros-desktop.sh
```

Use `--dry-run` to inspect the commands, `--skip-build` to reinstall existing artifacts, or
`--skip-nm-tray2` when that separate source tree is intentionally unavailable. The script must be run
as the desktop user, not through `sudo`; it elevates only the install commands and never restarts the
live session.

The equivalent manual TemplarOS package steps are:

```sh
just deb                                # builds target/debian/*.deb
sudo dpkg -i target/debian/*.deb
sudo apt-get -f install                 # pull in anything dpkg flagged
systemctl --user daemon-reload          # refresh units cached by the current desktop session
```

The explicit reload matters during development because these Stage-1 `cargo-deb` packages do not
yet ship Debian maintainer scripts. A user manager can survive logout and otherwise reuse the old
unit definitions even though the corrected files are already present under `/usr/lib/systemd/user`.

Then check it before you log out, because the whole point is not to end up unable to log in:

```sh
/usr/bin/templar doctor --root /
```

`unit-binaries` and `compositor-usable` are the two that matter here. The first resolves every
TemplarOS unit's `ExecStart` against what is actually installed; the second additionally checks
XWayland, which is a runtime lookup no build step can catch. If either fails, do not log out —
fix it first. The absolute path is intentional: a development host may already have an unrelated
`templar` in `~/.local/bin`, which commonly precedes `/usr/bin` in `PATH`.

Now log out and choose **TemplarOS** from the session menu.

### If it does not come up

Switch to a TTY with `Ctrl+Alt+F3` and log in there. `journalctl --user -b -u templar-comp` and
`journalctl --user -b -u templar-shell` have the current boot's story. After a forced restart, use
`journalctl -b -1 _SYSTEMD_USER_UNIT=templar-comp.service` (and `templar-shell.service`) for the previous
boot instead. To get back to your usual desktop, just pick it from the session menu at the next
login — installing these packages changes nothing about your existing session.

### Caveats

* Your machine's Ubuntu version need not be 26.04 for this path. The distribution itself pins
  26.04 "resolute" (`distro/base.toml`), and paths 3 and 4 target that; these packages are Rust
  binaries and systemd units and are far less fussy.
* `greetd` is only *recommended* by `templaros-session`, and reconfiguring your display manager to
  use it is a separate, riskier step. See `session/units/greetd/README.md` — including how to get
  back — before doing that. You do not need it for this path: your existing display manager will
  list the TemplarOS session perfectly well.

## 3. mkosi image in a VM

`distro/image/` builds a disk image (`compose.sh`, `mkosi.conf`). This is Stage 2 — it is what
proves the image composes, first boot works, and the btrfs deployment layout is right. It needs
mkosi and network access to Ubuntu's archive.

## 4. Real hardware

Stage 4, and the only way to test what actually matters about a display server: DRM/KMS
modesetting, direct scanout, both the Intel and NVIDIA paths, suspend/resume, and real monitor
hotplug. `--backend udev` is the default the moment neither `WAYLAND_DISPLAY` nor `DISPLAY` is
set, which is exactly the situation on a bare TTY. `tests/hardware/*.toml` is the matrix.

## What runs without any of this

```sh
cargo test --workspace                  # unit, integration and property tests
tests/integration/run.sh                # the daemons' runtime smoke
./target/debug/templar-comp --check        # the compositor, headlessly
```

`--check` constructs every protocol global, binds both sockets and exits. It is the only
compositor mode that runs where there is no seat and no host compositor to nest inside — CI, a
build machine, a container. It says nothing about rendering or input, and does not claim to.

`cargo test -p templar-comp` includes the one test that connects a real Wayland client over the real
socket and asserts the advertised protocol set — in both directions across the trust boundary.
