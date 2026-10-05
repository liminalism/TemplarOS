# Templar Displays — 0.1.0 alpha source

A Rust display-settings application and separate per-session recovery helper,
for Lubuntu/X11 and Wayland compositors implementing wlr-output-management.

**Delivery status:** implementation source, not a prebuilt or hardware-qualified
release. This authoring environment has neither `rustc` nor `cargo`, and network
access from the build container is unavailable. The Rust code and Rust tests
could not be compiled or executed here. The accompanying shell/manifest/source
structure checks are not substitutes for compilation. Start with the build and
demo below; do not replace your existing display utility before those pass.

## What the alpha implements

- Connected screens are shown even while disabled. A TV does not disappear from
  the interface merely because it is not part of the current desktop.
- Extend, Duplicate (conservative X11 common-mode clone), Show only on 1, and
  Show only on 2 configure complete layouts, including modes and the main screen.
- Resolution, refresh rate, orientation, visual/numeric placement, and X11 main
  screen selection; Wayland UI scaling when supported by the transport.
- A session helper independent of the settings window: opening the window starts
  it; closing the window leaves connection recovery active.
- A disappearing last active output causes a connected-screen fallback. An open
  laptop panel is preferred; with a closed lid, an external screen is preferred.
  This is a read-only lid hint, not a replacement for the system's suspend policy.
- New screens are added to the right while preserving existing active screens.
  Confirmed same-connector screen combinations can restore their saved layouts.
- An unconfirmed user change has a 15-second service-owned timer, readback, and
  rollback. A journal is written before mutation. A restarted service recovers
  an unfinished preview. An unavailable old screen causes a newly computed safe
  fallback rather than replaying a stale HDMI handle.
- A corner-drag TV-fitting panel using genuine RandR `underscan`, `underscan
  hborder`, and `underscan vborder` properties **when the driver advertises them**.
  This is a preview drawing followed by Apply to the physical TV, not a live
  full-screen calibration target. Borders are symmetric in this alpha.
- A simulated laptop/TV demo, read-only JSON inspection, CLI recovery, typed
  local IPC, profile persistence, XDG launcher/autostart templates, and an
  optional systemd user service with restart supervision.

## What it does not claim to implement

It is not yet a complete drop-in replacement for every display feature:

- TemplarOS compositor-side integration has **not** been patched. The connected
  `liminalism/TemplarOS` repository returned empty during this task. Your
  compositor must actually expose wlr-output-management for the included
  Wayland adapter to work. Smithay and XWayland do not provide that contract
  merely by being present. Read `docs/TEMPLAROS-INTEGRATION.md`.
- GNOME/Mutter and KDE/KWin display-configuration backends are not included.
  Ubuntu as a distribution does not imply a universal display-control API.
- NVIDIA NV-CONTROL / ViewPortOut calibration is designed but **not implemented**.
  An NVIDIA GPU does not automatically imply that an output has RandR underscan
  properties. The app reports fitting unavailable where its backend cannot do it.
- Wayland safe-area calibration, compositor-native mirroring and main-output
  selection require additional compositor APIs. Overlapping Wayland outputs is
  deliberately not mislabeled as mirroring.
- Full-screen identify/calibration overlays, independent four-edge offsets,
  HDR/VRR/color profiles, reflection, arbitrary transforms/panning, fractional
  scaling on X11, automatic HDMI audio, and port-independent monitor matching
  are deferred. Existing unsupported transforms/panning/reflection are rejected
  rather than silently flattened.
- NVIDIA-specific pre-existing MetaMode attributes are not fully captured by
  this alpha. Do not test it on a custom NVIDIA viewport/Mosaic/warp setup; keep
  the vendor utility for that configuration until the dedicated adapter exists.
- No proof is made that a driver reporting an output as active means the TV is
  on the correct input or physically showing a picture. Confirmation is still
  required for a new user-selected configuration.
- The current transport polls about every 600 ms, with a 750 ms topology
  debounce. Native event subscriptions are the next backend improvement.
- The packaged systemd wrapper targets one graphical session per user. Multiseat,
  multiple simultaneous sessions, suspend/resume and hybrid-GPU hardware need
  explicit qualification. The helper is not a privileged system-wide daemon.

## Build and first run

Use a current stable Rust toolchain. Native build dependencies for Ubuntu-family
systems, depending on installed development packages:

```sh
sudo apt install build-essential pkg-config libx11-dev libxrandr-dev \
  libxi-dev libxcursor-dev libxinerama-dev libxkbcommon-dev \
  libwayland-dev libegl1-mesa-dev libgl1-mesa-dev x11-xserver-utils
```

For the Wayland transport also install `wlr-randr`; the compositor still needs
its output-management protocol. No compositor replacement is installed here.

From this directory:

```sh
cargo test --no-default-features
cargo test --all-targets
cargo build --release
./target/release/templar-displays --demo
```

The first Cargo resolution generates `Cargo.lock`. Commit that generated lockfile
and use `--locked` for subsequent reproducible builds. No lockfile is fabricated
in this source delivery. A current compiler is recommended because transitive
dependencies can have a newer minimum than this crate's declared floor.

In Demo, use Show only on 2, Apply, then Keep. Use **Simulate HDMI plug / unplug**.
The simulated laptop should become active when the TV disappears. Repeat without
Keep and allow the preview to expire. Demo uses separate IPC and profile paths.

Inspect the real session **without starting the helper or changing displays**:

```sh
./target/release/templar-displays --inspect > display-before.json
```

Inspect that inventory and any `write_blocked` reason. Then launch the GUI:

```sh
./target/release/templar-displays
```

The real helper's automatic handling is enabled by default. Therefore the first
real GUI launch can enable an already-connected disabled TV after its short
startup debounce. Use `--inspect` when a strictly read-only check is intended.

The first real tests should be Extend and a resolution preview, while a known
working screen remains connected. **Do not use `sudo` to run the application.**

## Install for your user

```sh
./scripts/install-user.sh
```

This compiles/tests first and installs under `~/.local/bin`, with a Settings
launcher. It does not delete or replace the old monitor application.

After the manual tests pass, opt into login recovery and supervised restarts:

```sh
./scripts/install-user.sh --autostart
~/.local/bin/templar-displays-session-start
```

The session wrapper imports the real display environment before starting the
user service. Do not hardcode `DISPLAY=:0` or `WAYLAND_DISPLAY=wayland-0`.
The `.desktop` TemplarOS role keys are proposed integration metadata and must be
registered by the Templar session/launcher code; they are not a claim that a
currently shipped role registry already recognizes them.

Avoid two display-policy owners. After testing, disable the old desktop's
**automatic layout-restoration component**, not the whole desktop/power manager.
The installer deliberately does not guess which component that is or disable it.
Your old interactive settings utility can remain installed as a fallback.

## Useful commands and recovery limits

```sh
templar-displays --status
templar-displays --recover
templar-displays --stop
systemctl --user status templar-displays.service
journalctl --user -u templar-displays.service -b
```

`--stop` reverts an active preview before stopping. The systemd unit uses it as
its graceful stop action. Killing an unsupervised helper with SIGKILL cannot
execute rollback: the journal is recovered when the helper next starts. Use the
provided supervised session service for crash-restart recovery after testing.
No user-space utility can promise recovery through a hung kernel/driver, a dead
compositor, a broken panel/backlight, or physical loss of all usable displays.

Commands have a four-second timeout. The 15-second preview is a policy deadline,
not a hard real-time guarantee that the physical picture returns at exactly
15.000 seconds; driver latency and recovery attempts can extend it.

Profiles live under `~/.config/templar-displays/` (or `XDG_CONFIG_HOME`). Per-session
socket, manual-helper log and unconfirmed journal live under a private
`$XDG_RUNTIME_DIR/templar-displays-<session-hash>/` directory. Profiles use EDID
plus connector on X11, and advertised description plus connector on Wayland.
A changed port intentionally falls back to a new layout instead of guessing.

To remove login management, delete the app's XDG autostart file, run `--stop`,
and stop its user service. Keep existing desktop display-policy services intact
until the new app has passed your hardware checklist.

## Source map

`model.rs` contains serializable display capabilities and plans. `policy.rs` is
pure layout/recovery logic. `backend/` translates those plans through documented
user-session tools. `service.rs` is the single writer and rollback owner. `ipc.rs`
is bounded, private local IPC. `ui.rs` is an egui alpha front-end, separate from
hardware ownership so PixelKit can replace it later without moving safety logic
into a window. `docs/TESTING.md` is the acceptance gate and implementation status.

## Why a command transport is not the TV workaround you wanted to avoid

The Rust application invokes `xrandr` and `wlr-randr` with argument arrays, never
through a shell. They speak existing display-server interfaces. No custom
modelines, fake EDIDs, root scripts, startup `xorg.conf` rewrites, panning tricks,
provider auto-linking or direct DRM-master acquisition are used. That is an
ordinary adapter boundary, though native Rust protocol bindings will remove
text parsing and polling in a subsequent iteration. TV fitting is offered only
through an advertised driver operation, never approximated using a desktop zoom.

See `docs/DESIGN.md` for the intended complete product and `docs/SOURCES.md` for
primary technical references checked during design.
