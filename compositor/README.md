# compositor/

* `lege-comp/` — the compositor itself, on Smithay (ADR-0012): DRM/KMS and winit backends, the Wayland
  protocol surface, and the render-thread loop that drives `lege-window`.
* `lege-comp-integration/` — the async/D-Bus side the compositor links, so its render thread never blocks:
  * `identity` — `DbusIdentityCache`: render-thread-safe lookups, PID-reuse-safe (correlates a `/proc`
    start time, not the bare PID), bounded (FIFO eviction), and reconnects to `org.legeos.AppIdentity1`
    instead of giving up the first time `lege-sessiond` is unreachable.
  * `broadcaster` — `ShellStateBroadcaster`: sanitized `WindowService` snapshots to the shell, encoded and
    sent off the render thread.
  * `governor` — `GovernorNotifier`: coalesced focus/visibility enrichment to `org.legeos.Resources1`,
    degrading silently when the governor is absent.
  * `protocol` — names and the `Layer` ↔ wire-value mapping shared with `lege-comp`'s `wayland-scanner` glue.
* `COMPOSITOR.md` — the contract: the seven properties the compositor upholds, and the full map of
  compositor event → `lege-window` / `lege-comp-integration` call.

The compositor binary is built from this workspace like everything else. It is the one crate licensed
GPL-3.0-only rather than MPL-2.0 (ADR-0012); `cargo deny` allows that for `lege-comp` alone.

## Running it nested (development)

`lege-comp --backend winit` (the default whenever `WAYLAND_DISPLAY`/`DISPLAY` is already set) hosts the
compositor inside the current session as an ordinary window, and logs the socket name it created
(`wayland-N`) and its private trusted-shell socket (`wayland-N-shell`) at startup. Point a client at the
former to test against it, e.g. `WAYLAND_DISPLAY=wayland-N weston-terminal`. `--backend udev` runs against
real DRM/KMS hardware via `LibSeatSession` and is not exercisable from a nested session.

## Running it from a bare TTY (real hardware)

`--backend udev` is also the default the moment neither `WAYLAND_DISPLAY` nor `DISPLAY` is set — which is
exactly the environment a bare TTY login gives you, so `lege-comp` with no flags is correct there without
remembering to pass one.

1. **Switch to a free VT** the graphical session is not already using — `Ctrl+Alt+F3` (or F4, F5, ...; F1/F2
   are commonly the display manager or an existing graphical session, don't reuse those) — and log in.
2. **Run `lege-comp`** (or `systemctl --user start lege-comp.service` if the session units are installed).
   It opens `/dev/dri/card*` through `libseat`, takes DRM master, and lights up every connector on the one
   primary GPU it selects (`src/backend/udev.rs`'s module doc — a hybrid Intel+NVIDIA laptop's discrete GPU
   is deliberately not touched). Expect a plain dark fill (`CLEAR_COLOR` in `src/backend/udev.rs`) on every
   connected display and nothing else: no autostarted client, no cursor, no chrome text yet
   (`src/chrome.rs`'s own TODO — recovery bands render as flat colour rectangles, not lettering, until
   `pixelkit-text` lands) — a blank screen at this stage is the *expected* result of a successful run, not
   evidence of failure.
3. **To get out**, switch VT back (`Ctrl+Alt+F1`/whichever had your login shell) — VT switching is the
   kernel's own keyboard-console subsystem, handled before any keystroke reaches `lege-comp`, so it works
   even if the compositor has hung or is spinning. Switching away delivers `libseat`'s `PauseSession` event,
   which `src/backend/udev.rs` handles by releasing every DRM device; switching back re-acquires them and
   redraws from scratch, without you needing to restart the process. If it does need to be stopped, do that
   from the VT you switched back to: `systemctl --user stop lege-comp.service`, or `pkill lege-comp` if you
   ran it directly. **Never reboot to escape a stuck compositor** — VT-switch away first; a genuinely wedged
   process is still just `pkill`-able from another VT once you're off the one it's driving.
4. **What to check in the journal** — `journalctl --user -u lege-comp.service -b` if run as the unit,
   otherwise the terminal `lege-comp` was started from (its `tracing` output goes straight to the console;
   `RUST_LOG=lege_comp=debug lege-comp` for more detail). Look for:
   - `primary GPU selected` — which `DrmNode` it picked as the one GPU it will ever touch.
   - `ignoring non-primary GPU device` — expected once per *other* GPU on a hybrid laptop (the NVIDIA
     device); if it appears for the GPU actually driving the panel, primary-GPU selection picked wrong.
   - `output lit` per connector — absence of this for a connected display, with no matching `warn`, means
     the connector never scanned as connected at all (cabling/EDID) rather than a `lege-comp` failure.
   - `failed to initialize drm output` / `failed to query crtc planes` / `drm render_frame failed` — the
     specific step that failed for a connector that *did* scan as connected; these are the lines to attach
     to a bug report, along with the driver name logged alongside them.
   - `session paused` / `session resumed` around a VT switch — their absence means `libseat` never delivered
     the pause/resume event, which is a `libseat`/logind configuration problem, not a `lege-comp` one.
   - `failed to reactivate drm device after session resume` — the VT-switch-back path specifically failing;
     the display will stay blank on that VT until the process is restarted.
