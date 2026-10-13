# D-Bus interfaces

Single source of truth for every `org.templaros.*` interface. One bus name per daemon, domain-scoped
**versioned** interfaces at separate object paths (ADR-0001), so a future process split only relocates a
bus name.

| Bus name | Bus | Daemon | Interfaces |
|---|---|---|---|
| `org.templaros.Session` | session | `templar-sessiond` | `AppIdentity1`, `Launcher1`, `Associations1`, `Providers1`, `Background1`, `History1`, `Settings1`, `Projects1`, `HomeField1`, `Power1`, `Location1` (also `org.freedesktop.Notifications` when selected as the `notification-server` provider); an *application instance* serves `Checkpoint1` back to sessiond |
| `org.templaros.Resources` | session | `templar-resourced` | `Resources1` |
| `org.templaros.Files` | session | `templar-filed` | `Files.Volumes1` (+ `Operations1`, `Recents1`, `Search1` to come) |
| `org.templaros.Host` | system | `templar-hostd` | `Host.Updates1`, `Host.Deployments1`, `Host.Drivers1`, `Host.Storage1`, `Host.Hardware1` |
| `org.freedesktop.ScreenSaver` | session | installed `templar-comp` via `templar-comp-integration` | Playback compatibility at `/ScreenSaver` and `/org/freedesktop/ScreenSaver`; explicit video reasons are limited to the sender's visible fullscreen window, and released on pause/stop or disconnect |

Rules: `cargo xtask check-interfaces` fails if a `#[zbus::interface]` / `#[proxy]` name in the code has no
XML here. Constants live in `crates/templar-types/src/dbus.rs`; client proxies in `crates/templar-client`.
Bump the trailing version (`…2`) rather than changing a shipped signature.
