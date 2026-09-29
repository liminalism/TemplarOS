# Architecture

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ templar-shell (Home Surface, panel/dock, launcher, window shelf, notifications) │
│ templar-settings · templar-files (fork) · xdg-desktop-portal-templar · templar (CLI)   │
└───────────────┬────────────────────────────────────┬──────────────────────┘
                │ D-Bus (session bus)                 │ private templar_shell_v1
┌───────────────┴──────────────┐  ┌────────────┐  ┌──┴──────────────────────┐
│ templar-sessiond  org.templaros.Session │  │ templar-filed │  │ templar-comp (Smithay)          │
│  AppIdentity1  Launcher1          │  │ org.templaros │  │  + templar-comp-integration     │
│  Associations1 Providers1         │  │   .Files   │  │  + templar-window (one scene,   │
│  History1 Settings1 Projects1     │  │            │  │    recovery invariant,       │
│  HomeField1                       │  │            │  │                              │
│  Power1 Location1 Notifications   │  └────────────┘  │    typed layers)             │
└───────────────┬──────────────┘                     └──────────┬───────────┘
                │ signals                                       │
        ┌───────┴────────┐                          Wayland / XWayland / DRM-KMS
        │ templar-resourced │ org.templaros.Resources — separate, mlocked, MemoryLow, OOMScoreAdjust=-1000
        └────────────────┘ reads app.slice cgroups directly (self-describing unit names)
──────────────────────────────── system bus ────────────────────────────────
┌────────────────────────────────────────────────────────────────────────┐
│ templar-hostd  org.templaros.Host — Updates1 Deployments1 Drivers1 Storage1 Hardware1 │
│ templar-boot-health (system scope) → boot-complete.target → systemd-bless-boot     │
└────────────────────────────────────────────────────────────────────────┘
        Ubuntu 26.04 LTS: systemd, cgroup v2, apt, btrfs, PipeWire, NetworkManager, BlueZ, Mesa, NVIDIA LRM
```

## Processes (why these and not more)

| Process | Scope | Why separate |
|---|---|---|
| `templar-comp` | user, session.slice | Wayland display server; a crash disconnects clients → conservative, minimal |
| `templar-shell` | user | everything decorative; restartable at will |
| `templar-sessiond` | user | **the** policy daemon: identity/launch, associations, providers/autostart, history, notifications, settings, projects, power, location — one state DB, one crash unit, no cross-daemon races (ADR-0001) |
| `templar-resourced` | user | must tick while everything else thrashes; no SQLite/fsync/D-Bus on its loop (ADR-0002) |
| `templar-filed` | user | heavy I/O; a stuck mount must not stall policy |
| `templar-preview-codecs` | transient child of filed | bounded Templar-PDF render seam; keeps its heavier dependency graph out of the service |
| `templar-hostd` | root, system bus | the one privileged surface, one polkit policy (ADR-0003) |
| `templar-boot-health` | root, oneshot | gates `boot-complete.target` (ADR-0004) |
| `xdg-desktop-portal-templar` | user, D-Bus activated | one backend over the same services |

## Crates → processes

| Crate | Hosted by | Contents |
|---|---|---|
| `templar-types` | everyone | AppId/InstanceId/manifests/claims, provider roles, unit naming, checkpoint protocol, D-Bus names |
| `templar-appd` | sessiond | Registry (PID→cgroup→instance→AppId), launcher, manifests, `AppIdentity1`/`Launcher1` |
| `templar-associationd` | sessiond | rule engine, MIME detector, `mimeapps.list` export, `Associations1` |
| `templar-session-core` | sessiond | providers, autostart, history, notifications, settings schema, projects, power, location, first-run |
| `templar-state` | sessiond, filed | SQLite migrations |
| `templar-window` | templar-comp | geometry invariant (`repair_window ⇒ is_recoverable`, proptest), window records, drag, layers — **no async deps** |
| `templar-field` | shell, sessiond | the Home Field model: tiles on a bounded grid, ops with exact inverses (undo = apply the inverse), stable local reflow, representation ladder, drag/throw state machine, text form — **no async deps, no toolkit** |
| `templar-treemap` | shell | cushion treemaps for tray and drawer interiors (file-tree arena, byte-proportional squarified layout, category palette, CPU rasterizer; lifted from Silvaview-rs) — **no dependencies at all** |
| `templar-file-index` | filed, shell, cli | the shared file index: generations, lossless byte paths, budgeted scans, FTS5 trigram search, cursors, sealed snapshots, and the `EventSource` watcher seam |
| `templar-fanotify` | nothing (measured, unadopted) | fanotify bindings and a `FanotifySource` behind `templar-file-index`'s `EventSource`. The one crate that does not `forbid(unsafe_code)`; `src/sys.rs` holds the exception and `crates/templar-fanotify/README.md` states why. `InotifySource` remains the default — see `docs/measurements/fanotify-2026-09-04.md` |
| `templar-comp-integration` | templar-comp | `IdentityCache` over D-Bus, governor notifier, shell broadcaster |
| `templar-comp` (bin) | itself | the Smithay compositor: DRM/KMS + winit backends, protocols, render loop (ADR-0012, GPL-3.0) |
| `templar-client` | shell, apps, portal, cli, comp | proxies + app SDK (checkpoint protocol) |
| `templar-hwdetect` | hostd, installer, doctor | GPU detection, flavour, modes |
| `templar-resourced` (lib) | itself | cgroup wrapper, PSI, pure `Governor`, executor, `app.slice` scan |
| `templar-filed` (lib) | itself | structured browsing, bounded previews, operation journal, storage/reserve, last-dir policy |
| `templar-preview-codecs` (bin) | transient helper | first-page Templar PDF CPU rendering behind time, memory and output limits |
| `templar-build` | every long-lived binary, xtask, cli | build identity: one BuildSetId per set, runtime stamps saying what is running, the component matrix, D-Bus signature comparison |
| `templar` (lib) | cli, tests | `doctor` engine, `templar dev` deployment (convergence, local archive, activation, gates) |

## Invariants the code enforces

* One instance ⇔ one canonical AppId ⇔ one cgroup ⇔ one resource record ⇔ N windows.
* A window's identity comes from its cgroup; claims only raise `claim_mismatch`.
* Every `AppId` names a **variant** — one installed copy from one origin. Grouping variants into
  a logical application is presentation; rights (MIME defaults, provider declarations, autostart
  approvals, instances) stay keyed on the variant, so a wrong grouping can never widen a grant.
* Trust is derived from provenance, never declared: a user-writable copy may not take an
  exclusive provider role, and a desktop id whose provenance moves from a packaged copy to a user
  file loses the grants the packaged copy had earned.
* After every drag/output change, `is_recoverable(window)` holds (proptest, 2000 cases).
* Detected MIME beats extension unless the user created an explicit override.
* Exactly one provider per exclusive role may autostart.
* The governor is observe-only until traces are reviewed; it never selects by memory size alone.
* Nothing under `/usr/local`; every file is owned by a package.
