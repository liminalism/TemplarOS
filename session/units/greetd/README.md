# session/units/greetd — the login screen

Greeter/display-manager choice (ADR-0010, as amended by ADR-0012): [`greetd`](https://git.sr.ht/~kennylevinsen/greetd)
+ `templar-greeter` (`apps/templar-greeter`), not the `cosmic-greeter` fork — `templar-greeter` uses pixelkit, the
same toolkit as the shell, so there is no second toolkit/session stack, and it authenticates by speaking
greetd's own JSON IPC over `GREETD_SOCK`. **greetd alone owns PAM**: `templar-greeter` links no PAM, no
shadow and nothing setuid. The lock screen is unrelated to any of this — it is compositor-core
`ext-session-lock-v1` (the `lock-screen` provider role has one provider by construction) and needs no
greeter at all.

`config.toml` here is the template installed to `/usr/share/templaros/greetd/config.toml` by the
`templaros-greeter` package. The TemplarOS image build copies it to `/etc/greetd/config.toml`, disables the
conflicting VT1 getty, and enables greetd. An ordinary package installation intentionally leaves an
administrator's existing display-manager configuration untouched; see "Enabling" for that case.

## The greetd user

greetd runs `default_session.command` as `default_session.user` — a dedicated system account, never a
real login. `config.toml` names it `templar-greeter`; the package ships
`/usr/lib/sysusers.d/templaros-greeter.conf` and `/usr/lib/tmpfiles.d/templaros-greeter.conf`, so normal
package/image setup creates both the account and its state directory. To repair them manually:

```sh
sudo useradd --system --no-create-home --home-dir /var/lib/templar-greeter \
    --shell /usr/sbin/nologin --user-group templar-greeter
sudo install -d -o templar-greeter -g templar-greeter /var/lib/templar-greeter
```

After a successful `start_session` request, the greeter saves the username in
`/var/lib/templar-greeter/last-user` (0600, atomic replacement). It pre-fills that username on the
next login but never starts authentication automatically. Failed authentication, cancellation and
a failed session launch cannot overwrite the remembered username; no password or biometric data
is persisted by the greeter.

The owner-run `scripts/reinstall-templaros-desktop.sh` seeds a missing hint with the installing
desktop account for the first upgrade. It publishes a private greeter-owned file atomically without
replacing an existing hint; subsequent successful session launches replace that bootstrap value.

What it needs, and why:

* **A fresh `XDG_RUNTIME_DIR` and a registered login session.** greetd runs the greeter's PAM stack
  (`/etc/pam.d/greetd-greeter`, shipped by the `greetd` package, not by TemplarOS) before starting
  `default_session.command`; on a normal systemd system that stack includes `pam_systemd`, which creates
  `/run/user/<uid>` and registers the session with logind. `templar-greeter` never reads
  `XDG_RUNTIME_DIR` itself, but `templar-comp` and its Wayland socket do.
* **DRM/KMS and libinput device access**, because `default_session.command` runs `templar-comp --backend
  udev` as this user (see "What actually happens on a bare VT" below), and that backend needs to open
  `/dev/dri/card*` and the input devices directly. `templar-comp`'s own `Cargo.toml` comment notes it uses
  `LibSeatSession`, which — via `systemd-logind` (the common case on a system with `pam_systemd` wired in,
  as above) or via `seatd` — is meant to grant this per-session without needing the user in any
  supplementary group. If it does not (a `seatd`-only setup, or a distro whose PAM stack does not run
  `pam_systemd` for greetd's own session), add the fallback groups by hand:
  ```sh
  sudo usermod -aG video,input templar-greeter
  ```
  This has not been exercised against a real greetd/logind/seatd stack in the environment this crate was
  built in (see `apps/templar-greeter/src/main.rs`'s own doc comment on what remains unverified) — treat it
  as the documented starting point, not a tested guarantee.

## Fingerprint login on the reference laptop

`pam-login` is installed as a template at `/usr/share/templaros/greetd/pam-login`, not over an
administrator's existing PAM file. The image and installer select it explicitly. It preserves
the `pam_nologin` gate, tries `pam_fprintd` for ten seconds, then falls through to the normal
`common-auth` password substack on failure, missing enrollment or unavailable hardware. Fingerprint
success skips only that password substack; login's account, session and password-management checks
still run. PAM information/error messages are painted and acknowledged automatically; one worker
waits for greetd while the UI remains responsive. Escape queues cancellation, so an intervening
authentication success cannot launch a session after cancellation was requested.

This laptop's ELAN `04f3:0c4b` revision `24.30` needs the owner's Lenovo 0.0.8 **OpenSSL-3** TOD
plugin, not the old OpenSSL-1.1 blob and not the generic swipe driver. The installed Lubuntu plugin
and `elan-fp/vendor/lenovo-r1slf01w/extracted/package/` copy have SHA-256
`a5060ccdfc0c613bd4522be29f87cd94f098e6b83294cf02cd5a958b24b27f80`.
Its retained `vendor-runs/run3/verify.log` records a matching fingerprint on Lubuntu. A native
Resolute 1.95.1+tod1 smoke loaded this exact plugin and discovered `efd_tod (ELAN Fingerprint Sensor)`;
that observation is device/ABI discovery, not a new enrollment or authentication claim.

Build the separate owner-supplied package without copying biometric captures/templates:

```sh
scripts/build-elan-fingerprint-deb.py \
  /mnt/lubuntu/home/dk/elan-fp/vendor/lenovo-r1slf01w/extracted/package/libfprint-2-tod1-elan.so
sudo apt-get install ./target/debian/templaros-elan-fingerprint_0.0.8+templaros1_amd64.deb
sudo cp -a /etc/pam.d/greetd /etc/pam.d/greetd.before-templaros-fingerprint
sudo install -m 644 /usr/share/templaros/greetd/pam-login /etc/pam.d/greetd
fprintd-list dk
# If no matching enrollment is present on this OS:
fprintd-enroll -f right-index-finger dk
fprintd-verify dk
```

The package adds the sensor-revision-specific no-autosuspend rule; it does not install obsolete
OpenSSL, start/restart any desktop service, or import private enrollment data from Lubuntu. If the
new udev rule is not yet applied, reboot is the owner-controlled way to apply it. Keep password
login as the recovery path. Fingerprint login does not supply a password to unlock an encrypted
GNOME/KWallet keyring. Test `fprintd-verify`, password fallback and cancellation before relying on
the new login path; do not stop a working display manager from the current desktop.

Package directories are root-owned mode `0755` and payload files `0644`, independent of the
builder's umask; the temporary staging parent stays private.

## What actually happens on a bare VT

greetd runs one command:

```
command = "templar-comp --backend udev --exec /usr/bin/templar-greeter"
```

The compositor starts, brings up its Wayland socket, and spawns `templar-greeter` with
`WAYLAND_DISPLAY` already set in that child's environment. `GREETD_SOCK`, which greetd put in the
compositor's environment, is inherited straight through. When the greeter exits — logged in, gave
up, or crashed — the compositor exits too, with the greeter's own exit code. `greetd(5)`: "The
session will start after the greeter process terminates", and what has to terminate is this whole
command, compositor included.

The greeter never starts a compositor and never looks for a socket name. An earlier design did:
it spawned `templar-comp`, read its log output for the line announcing the socket, and re-execed
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

1. Install `templaros-comp`, `templaros-greeter` and `greetd` (`templaros-session` already `Recommends:` all three,
   `debian/control`).
2. Put this directory's `config.toml` at `/etc/greetd/config.toml`:
   `sudo install -m 644 /usr/share/templaros/greetd/config.toml /etc/greetd/config.toml`.
3. Apply the packaged account declarations if package hooks have not already done so:
   `sudo systemd-sysusers /usr/lib/sysusers.d/templaros-greeter.conf` and
   `sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/templaros-greeter.conf`.
   Installing the fingerprint PAM template is a separate administrator decision; review the
   fingerprint section above before replacing `/etc/pam.d/greetd`.
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
`apps/templar-greeter/src/main.rs` for exactly why (no greetd running in that environment, and taking over a
real display manager's VT was explicitly out of scope for it) and exactly what *was* verified instead
(the IPC client and login state machine against an in-process fake greetd, the session-file picker against
real files on that machine, `cargo test -p templar-greeter`), plus the `--exec` handshake itself,
which is verified in `compositor/templar-comp` and in `tests/integration/run.sh`.

## Recovering — read this before enabling step 5 above

A broken greeter means no graphical login at all, on the one VT it occupies. None of this touches that VT:

* **Get to a shell first.** Switch to a different virtual terminal — physically, `Ctrl+Alt+F2` (or F3, F4,
  …; not F1, which is where greetd itself sits per `config.toml`'s `vt = 1`) — and log in at the getty
  prompt there. This works regardless of what greetd or `templar-greeter` is doing on VT1, because it is a
  different VT with its own getty.
* **Read what happened.** `journalctl -u greetd -b` — greetd does not redirect its children's stdio away
  from its own, so `templar-comp`'s and `templar-greeter`'s tracing output both land in the same
  unit's journal. Look for `started --exec client`, `--exec client exited`, or a
  — is the templaros-comp package installed?`, or `connecting to greetd via GREETD_SOCK` — each names a
  different failure named directly in `apps/templar-greeter/src/main.rs` or
  `compositor/templar-comp/src/session_command.rs`.
* **Stop the bleeding.** `sudo systemctl stop greetd.service` frees VT1 immediately. A real greetd package
  typically also carries its own restart policy (`Restart=always` with a `StartLimitBurst`/
  `StartLimitInterval`, as installed on the machine this crate was built on) so a crash-looping greeter
  eventually gives up on its own, at which point VT1 goes back to being an ordinary unused console — but
  do not wait for that if step 5 above was just enabled system-wide.
* **Fall back to a known-good greeter while debugging `templar-greeter` itself.** Edit
  `/etc/greetd/config.toml`'s `[default_session]` back to whatever the distro's `greetd` package ships by
  default (typically `agreety`, e.g. `command = "agreety --cmd '${SHELL:-/bin/sh}'"`, `user = "_greetd"`
  or similar — check `/usr/share/doc/greetd` on the machine in question) and `sudo systemctl restart
  greetd.service`. This is also the config `greetd`'s own package installs before this file ever
  overwrites it, so keeping a copy of it before step 2 above is the cheapest way back.
* **Sanity-check the binary without touching greetd at all.** Running `templar-greeter` directly from an
  ordinary shell — as any user, with `GREETD_SOCK` unset — connects to nothing and exits immediately with
  a clear "GREETD_SOCK is not set" error; it never opens a window, starts a compositor, or touches PAM.
  This is a safe way to confirm the binary is installed and linked correctly. To exercise the login
  surface itself and every state it can be in — a mistyped password, a `start_session` failure, a dropped
  connection — without any of that risk, run `cargo test -p templar-greeter` from a checkout instead: the
  whole IPC client and state machine are tested against an in-process fake greetd, with no socket file,
  no display and no PAM anywhere near it.

## Keyboard session picker

Tab/Shift-Tab traverse the primary field, session picker and sign-in button. On the closed picker,
arrow keys cycle sessions and Enter/Space opens its popup. Inside that popup, Up/Down select a row,
Enter commits it, and Escape or Tab dismisses it. Those popup keys do not submit or cancel
authentication; dismissal leaves focus on the picker and restores the form's three-slot traversal.
Escape outside the popup still cancels authentication. Queued opening/navigation/dismissal keys
are handled in order, including an opening Enter followed by Escape in the same frame.

On 2026-10-06, the qualified greeter regressions
`screen::tests::keyboard_session_popup_dismisses_without_canceling_authentication` and
`screen::tests::queued_session_popup_keys_keep_authentication_and_response_intact` each passed,
and the native greeter binary was rebuilt. The actual 800×600 native window then ran on a separately
owned authenticated Xvfb display and private session/system buses, with private HOME/XDG state and
a bubblewrap-private `/var/lib`. Its owned framed-JSON peer could only present an
authentication-unavailable prompt or deny authentication; it had no PAM or session launcher.
Native Tab/Enter opened the session popup, Escape dismissed it with the prompt unchanged and no
authentication request, subsequent Tabs focused the button then field, and Tab/Space plus
Down/Enter selected LXQt Desktop without submitting a response. Only a later Escape outside the
popup sent `cancel_session` and restored the username form.

The retained observations, framed requests, screenshots and exact owned-child teardown are under
`.agent/scratch/greeter-dropdown-focus-20261006/` (`evidence.json`, `greetd-wire.json` and
`final-native-picker-open.png`/`final-native-picker-dismissed.png`). No password was entered, no real greetd/PAM
was contacted, and no installed unit or display-manager setting changed. This proves the native
consumer keyboard path, not physical-seat, biometric, login or deployment acceptance.

## Remaining boundary

The package ships the account declarations and a safe configuration template. Only the TemplarOS image
build selects it automatically; a package installed alongside another desktop does not replace that
machine's display manager. Real PAM/logind/seat/VT testing remains part of the hardware phase.
