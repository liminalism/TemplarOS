# session/units/greetd — the login screen

Greeter/display-manager choice (ADR-0010, as amended by ADR-0012): [`greetd`](https://git.sr.ht/~kennylevinsen/greetd)
+ `lege-greeter` (`apps/lege-greeter`), not the `cosmic-greeter` fork — `lege-greeter` uses pixelkit, the
same toolkit as the shell, so there is no second toolkit/session stack, and it authenticates by speaking
greetd's own JSON IPC over `GREETD_SOCK`. **greetd alone owns PAM**: `lege-greeter` links no PAM, no
shadow and nothing setuid. The lock screen is unrelated to any of this — it is compositor-core
`ext-session-lock-v1` (the `lock-screen` provider role has one provider by construction) and needs no
greeter at all.

`config.toml` here is the template installed to `/usr/share/legeos/greetd/config.toml` by the
`legeos-session` package (`debian/legeos-session.install`). Getting it live at `/etc/greetd/config.toml`
is a packaging step that does not exist yet (no postinst copies it) — see "Enabling" below for the manual
equivalent, and the note at the end of this file for what that gap is.

## The greetd user

greetd runs `default_session.command` as `default_session.user` — a dedicated system account, never a
real login. `config.toml` here names it `lege-greeter`; nothing currently creates that account
automatically (there is no `sysusers.d` entry for it yet — another packaging gap, noted below). Until
that lands, create it by hand once per image/install:

```sh
sudo useradd --system --no-create-home --home-dir /var/lib/lege-greeter \
    --shell /usr/sbin/nologin --user-group lege-greeter
sudo install -d -o lege-greeter -g lege-greeter /var/lib/lege-greeter
```

What it needs, and why:

* **A fresh `XDG_RUNTIME_DIR` and a registered login session.** greetd runs the greeter's PAM stack
  (`/etc/pam.d/greetd-greeter`, shipped by the `greetd` package, not by LegeOS) before starting
  `default_session.command`; on a normal systemd system that stack includes `pam_systemd`, which creates
  `/run/user/<uid>` and registers the session with logind. `lege-greeter` never reads
  `XDG_RUNTIME_DIR` itself, but `lege-comp` and its Wayland socket do.
* **DRM/KMS and libinput device access**, because `default_session.command` runs `lege-comp --backend
  udev` as this user (see "What actually happens on a bare VT" below), and that backend needs to open
  `/dev/dri/card*` and the input devices directly. `lege-comp`'s own `Cargo.toml` comment notes it uses
  `LibSeatSession`, which — via `systemd-logind` (the common case on a system with `pam_systemd` wired in,
  as above) or via `seatd` — is meant to grant this per-session without needing the user in any
  supplementary group. If it does not (a `seatd`-only setup, or a distro whose PAM stack does not run
  `pam_systemd` for greetd's own session), add the fallback groups by hand:
  ```sh
  sudo usermod -aG video,input lege-greeter
  ```
  This has not been exercised against a real greetd/logind/seatd stack in the environment this crate was
  built in (see `apps/lege-greeter/src/main.rs`'s own doc comment on what remains unverified) — treat it
  as the documented starting point, not a tested guarantee.

## What actually happens on a bare VT

greetd runs one command:

```
command = "lege-comp --backend udev --exec /usr/bin/lege-greeter"
```

The compositor starts, brings up its Wayland socket, and spawns `lege-greeter` with
`WAYLAND_DISPLAY` already set in that child's environment. `GREETD_SOCK`, which greetd put in the
compositor's environment, is inherited straight through. When the greeter exits — logged in, gave
up, or crashed — the compositor exits too, with the greeter's own exit code. `greetd(5)`: "The
session will start after the greeter process terminates", and what has to terminate is this whole
command, compositor included.

The greeter never starts a compositor and never looks for a socket name. An earlier design did:
it spawned `lege-comp`, read its log output for the line announcing the socket, and re-execed
itself with `WAYLAND_DISPLAY` set. That made a human-readable log line into an API, and it meant
`RUST_LOG` set to anything quieter than `info` anywhere in the greeter's environment would stop
the socket ever being announced — no graphical login, caused by an environment variable. `--exec`
has no such surface: nothing is parsed, nothing is timed out, and the socket is already listening
before the child starts.

Two things worth knowing before debugging this on real hardware:

* If the greeter dies immediately — a missing font, an unreachable `GREETD_SOCK`, a panic — the
  compositor exits with it rather than leaving a black screen. This case is specifically tested
  (`tests/integration/run.sh`), because it used to hang: calloop's event loop clears its stop flag
  when it starts, so a client that exited before the loop began was ignored.
* Both processes log to the same place, so `journalctl` (see "Recovering" below) shows the
  compositor's output and the greeter's together, under whichever unit greetd itself runs as.

## Enabling

1. Install `legeos-comp`, `legeos-greeter` and `greetd` (`legeos-session` already `Recommends:` all three,
   `debian/control`).
2. Put this directory's `config.toml` at `/etc/greetd/config.toml` (until the packaging gap above is
   closed): `sudo install -m 644 /usr/share/legeos/greetd/config.toml /etc/greetd/config.toml`.
3. Create the `lege-greeter` user (above), if the package has not by the time this is read.
4. **Check VT1 is not already spoken for.** `config.toml` runs on `vt = 1`; if a getty is still enabled
   there (`systemctl status getty@tty1.service`), greetd and the getty will both be attached to the same
   VT and corrupt each other's display. Disable it first:
   ```sh
   sudo systemctl disable --now getty@tty1.service
   ```
5. `sudo systemctl enable --now greetd.service`. On a typical greetd package this unit carries
   `Alias=display-manager.service`, so this is also what makes it *the* display manager — if another one
   (`gdm3`, `sddm`, `lightdm`, `cosmic-greeter-daemon`'s own units, …) is currently enabled, disable that
   one first, or the two will race for the same VT at boot the same way a getty would.

None of this has been run end-to-end while building this crate — see the note at the top of
`apps/lege-greeter/src/main.rs` for exactly why (no greetd running in that environment, and taking over a
real display manager's VT was explicitly out of scope for it) and exactly what *was* verified instead
(the IPC client and login state machine against an in-process fake greetd, the session-file picker against
real files on that machine, `cargo test -p lege-greeter` — 68 tests, all headless), plus the `--exec` handshake itself,
which is verified in `compositor/lege-comp` and in `tests/integration/run.sh`.

## Recovering — read this before enabling step 5 above

A broken greeter means no graphical login at all, on the one VT it occupies. None of this touches that VT:

* **Get to a shell first.** Switch to a different virtual terminal — physically, `Ctrl+Alt+F2` (or F3, F4,
  …; not F1, which is where greetd itself sits per `config.toml`'s `vt = 1`) — and log in at the getty
  prompt there. This works regardless of what greetd or `lege-greeter` is doing on VT1, because it is a
  different VT with its own getty.
* **Read what happened.** `journalctl -u greetd -b` — greetd does not redirect its children's stdio away
  from its own, so `lege-comp`'s and `lege-greeter`'s tracing output both land in the same
  unit's journal. Look for `started --exec client`, `--exec client exited`, or a
  — is the legeos-comp package installed?`, or `connecting to greetd via GREETD_SOCK` — each names a
  different failure named directly in `apps/lege-greeter/src/main.rs` or
  `compositor/lege-comp/src/session_command.rs`.
* **Stop the bleeding.** `sudo systemctl stop greetd.service` frees VT1 immediately. A real greetd package
  typically also carries its own restart policy (`Restart=always` with a `StartLimitBurst`/
  `StartLimitInterval`, as installed on the machine this crate was built on) so a crash-looping greeter
  eventually gives up on its own, at which point VT1 goes back to being an ordinary unused console — but
  do not wait for that if step 5 above was just enabled system-wide.
* **Fall back to a known-good greeter while debugging `lege-greeter` itself.** Edit
  `/etc/greetd/config.toml`'s `[default_session]` back to whatever the distro's `greetd` package ships by
  default (typically `agreety`, e.g. `command = "agreety --cmd '${SHELL:-/bin/sh}'"`, `user = "_greetd"`
  or similar — check `/usr/share/doc/greetd` on the machine in question) and `sudo systemctl restart
  greetd.service`. This is also the config `greetd`'s own package installs before this file ever
  overwrites it, so keeping a copy of it before step 2 above is the cheapest way back.
* **Sanity-check the binary without touching greetd at all.** Running `lege-greeter` directly from an
  ordinary shell — as any user, with `GREETD_SOCK` unset — connects to nothing and exits immediately with
  a clear "GREETD_SOCK is not set" error; it never opens a window, starts a compositor, or touches PAM.
  This is a safe way to confirm the binary is installed and linked correctly. To exercise the login
  surface itself and every state it can be in — a mistyped password, a `start_session` failure, a dropped
  connection — without any of that risk, run `cargo test -p lege-greeter` from a checkout instead: the
  whole IPC client and state machine are tested against an in-process fake greetd, with no socket file,
  no display and no PAM anywhere near it.

## Known gaps outside this file's own reach

Both noted above, repeated here because they are easy to miss: no `sysusers.d`/postinst creates the
`lege-greeter` system user yet, and no postinst copies `config.toml` from
`/usr/share/legeos/greetd/config.toml` to the live `/etc/greetd/config.toml`. Both are `debian/**`
packaging work, not `session/units/greetd/**`'s to do.
