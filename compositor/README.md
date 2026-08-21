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
