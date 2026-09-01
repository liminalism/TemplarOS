# Architecture

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ lege-shell (Home Surface, panel/dock, launcher, window shelf, notifications) │
│ lege-settings · lege-files (fork) · xdg-desktop-portal-lege · lege (CLI)   │
└───────────────┬────────────────────────────────────┬──────────────────────┘
                │ D-Bus (session bus)                 │ private lege_shell_v1
┌───────────────┴──────────────┐  ┌────────────┐  ┌──┴──────────────────────┐
│ lege-sessiond  org.legeos.Session │  │ lege-filed │  │ lege-comp (Smithay)          │
│  AppIdentity1  Launcher1          │  │ org.legeos │  │  + lege-comp-integration     │
│  Associations1 Providers1         │  │   .Files   │  │  + lege-window (one scene,   │
│  History1 Settings1 Projects1     │  │            │  │    recovery invariant,       │
│  HomeField1                       │  │            │  │                              │
│  Power1 Location1 Notifications   │  └────────────┘  │    typed layers)             │
└───────────────┬──────────────┘                     └──────────┬───────────┘
                │ signals                                       │
        ┌───────┴────────┐                          Wayland / XWayland / DRM-KMS
        │ lege-resourced │ org.legeos.Resources — separate, mlocked, MemoryLow, OOMScoreAdjust=-1000
        └────────────────┘ reads app.slice cgroups directly (self-describing unit names)
──────────────────────────────── system bus ────────────────────────────────
┌────────────────────────────────────────────────────────────────────────┐
│ lege-hostd  org.legeos.Host — Updates1 Deployments1 Drivers1 Storage1 Hardware1 │
│ lege-boot-health (system scope) → boot-complete.target → systemd-bless-boot     │
└────────────────────────────────────────────────────────────────────────┘
        Ubuntu 26.04 LTS: systemd, cgroup v2, apt, btrfs, PipeWire, NetworkManager, BlueZ, Mesa, NVIDIA LRM
```

## Processes (why these and not more)

| Process | Scope | Why separate |
|---|---|---|
| `lege-comp` | user, session.slice | Wayland display server; a crash disconnects clients → conservative, minimal |
| `lege-shell` | user | everything decorative; restartable at will |
| `lege-sessiond` | user | **the** policy daemon: identity/launch, associations, providers/autostart, history, notifications, settings, projects, power, location — one state DB, one crash unit, no cross-daemon races (ADR-0001) |
| `lege-resourced` | user | must tick while everything else thrashes; no SQLite/fsync/D-Bus on its loop (ADR-0002) |
| `lege-filed` | user | heavy I/O; a stuck mount must not stall policy |
| `lege-preview-codecs` | transient child of filed | bounded JPXL/Lege-PDF decode seam; keeps their heavier dependency graph out of the service |
| `lege-hostd` | root, system bus | the one privileged surface, one polkit policy (ADR-0003) |
| `lege-boot-health` | root, oneshot | gates `boot-complete.target` (ADR-0004) |
| `xdg-desktop-portal-lege` | user, D-Bus activated | one backend over the same services |

## Crates → processes

| Crate | Hosted by | Contents |
|---|---|---|
| `lege-types` | everyone | AppId/InstanceId/manifests/claims, provider roles, unit naming, checkpoint protocol, D-Bus names |
| `lege-appd` | sessiond | Registry (PID→cgroup→instance→AppId), launcher, manifests, `AppIdentity1`/`Launcher1` |
| `lege-associationd` | sessiond | rule engine, MIME detector, `mimeapps.list` export, `Associations1` |
| `lege-session-core` | sessiond | providers, autostart, history, notifications, settings schema, projects, power, location, first-run |
| `lege-state` | sessiond, filed | SQLite migrations |
| `lege-window` | lege-comp | geometry invariant (`repair_window ⇒ is_recoverable`, proptest), window records, drag, layers — **no async deps** |
| `lege-field` | shell, sessiond | the Home Field model: tiles on a bounded grid, ops with exact inverses (undo = apply the inverse), stable local reflow, representation ladder, drag/throw state machine, text form — **no async deps, no toolkit** |
| `lege-treemap` | shell | cushion treemaps for tray and drawer interiors (file-tree arena, byte-proportional squarified layout, category palette, CPU rasterizer; lifted from Silvaview-rs) — **no dependencies at all** |
| `lege-comp-integration` | lege-comp | `IdentityCache` over D-Bus, governor notifier, shell broadcaster |
| `lege-comp` (bin) | itself | the Smithay compositor: DRM/KMS + winit backends, protocols, render loop (ADR-0012, GPL-3.0) |
| `lege-client` | shell, apps, portal, cli, comp | proxies + app SDK (checkpoint protocol) |
| `lege-hwdetect` | hostd, installer, doctor | GPU detection, flavour, modes |
| `lege-resourced` (lib) | itself | cgroup wrapper, PSI, pure `Governor`, executor, `app.slice` scan |
| `lege-filed` (lib) | itself | structured browsing, bounded previews, operation journal, storage/reserve, last-dir policy |
| `lege-preview-codecs` (bin) | transient helper | JPXL decode and first-page Lege PDF CPU rendering behind time, memory and output limits |
| `lege-build` | every long-lived binary, xtask, cli | build identity: one BuildSetId per set, runtime stamps saying what is running, the component matrix, D-Bus signature comparison |
| `lege` (lib) | cli, tests | `doctor` engine, `lege dev` deployment (convergence, local archive, activation, gates) |

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
