# D-Bus interfaces

Single source of truth for every `org.legeos.*` interface. One bus name per daemon, domain-scoped
**versioned** interfaces at separate object paths (ADR-0001), so a future process split only relocates a
bus name.

| Bus name | Bus | Daemon | Interfaces |
|---|---|---|---|
| `org.legeos.Session` | session | `lege-sessiond` | `AppIdentity1`, `Launcher1`, `Associations1`, `Providers1`, `History1`, `Settings1`, `Projects1`, `Power1`, `Location1` (also `org.freedesktop.Notifications` when selected as the `notification-server` provider) |
| `org.legeos.Resources` | session | `lege-resourced` | `Resources1` |
| `org.legeos.Files` | session | `lege-filed` | `Files.Volumes1` (+ `Operations1`, `Recents1`, `Search1` to come) |
| `org.legeos.Host` | system | `lege-hostd` | `Host.Updates1`, `Host.Deployments1`, `Host.Drivers1`, `Host.Storage1`, `Host.Hardware1` |

Rules: `cargo xtask check-interfaces` fails if a `#[zbus::interface]` / `#[proxy]` name in the code has no
XML here. Constants live in `crates/lege-types/src/dbus.rs`; client proxies in `crates/lege-client`.
Bump the trailing version (`…2`) rather than changing a shipped signature.
