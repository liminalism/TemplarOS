# compositor/

* `lege-comp-integration/` — the crate the compositor fork links:
  * `identity` — `DbusIdentityCache`: render-thread-safe lookups, PID-reuse-safe (correlates a `/proc`
    start time, not the bare PID), bounded (FIFO eviction), and reconnects to `org.legeos.AppIdentity1`
    instead of giving up the first time `lege-sessiond` is unreachable.
  * `broadcaster` — `ShellStateBroadcaster`: sanitized `WindowService` snapshots to the shell, encoded and
    sent off the render thread.
  * `governor` — `GovernorNotifier`: coalesced focus/visibility enrichment to `org.legeos.Resources1`,
    degrading silently when the governor is absent.
  * `protocol` — names and the `Layer` ↔ wire-value mapping shared with the fork's `wayland-scanner` glue.
* `FORK.md` — exactly what the cosmic-comp fork (`forks/cosmic-comp`, pinned in `distro/forks/manifest.toml`)
  changes, and the full map of fork event → `lege-window` / `lege-comp-integration` call.

The compositor binary is packaged as `lege-comp` from the fork repository, not from this workspace.
