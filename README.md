# TemplarOS

**A tightly integrated desktop operating system on an Ubuntu 26.04 LTS core** — with its own compositor policy,
shell, file layer, application identity system, resource governor, driver/update transaction system and
installation image. Start from scratch in *policy and user interaction*, not in plumbing that already works.

Status: the native session includes the compositor, PixelKit shell, Homegarden, Settings and
first-party file services. The session daemon persists state and launches applications through
systemd `StartTransientUnit`; the compositor owns windows, recovery and typed overlay layers.
Homegarden represents current work, the bottom rail returns to tasks and saved targets, and one
[Apps/Files launcher](docs/launcher.md) discovers and opens them. The governor, file transaction
service and kernel/driver update service retain their separate ownership.

Acceptance and remaining hardware qualification are tracked in the AKR ledger and
`docs/generated/ROADMAP.md`. Source tests and owned fixtures establish specific behavior;
installed-session and physical monitor/GPU observations have their own gates. See
`docs/planning/` for the founding analysis and `docs/adr/` for decisions.

## The five things the first release must prove

1. A desktopless **Home Surface** is better than wallpaper and desktop icons.
2. Windows **cannot become lost** during monitor or geometry changes.
3. The machine **remains interactive** under extreme application memory pressure.
4. File opening is **predictable and centrally controlled**.
5. Kernel and NVIDIA updates **succeed as a pair or roll back as a pair**.

## What is different

* **One application contract.** `org.templaros.AppIdentity1`: a canonical `AppId`, one cgroup per launch
  (`app-templaros-<AppId>@<InstanceId>.service`), identity verified through the cgroup — never through the
  client's self-declared `app_id`. Dock grouping, associations, resource accounting, notifications and
  session restore all key on it.
* **One policy daemon** (`templar-sessiond`) instead of a dozen applets and helpers; **one small protected
  governor** (`templar-resourced`); **one file service** (`templar-filed`); **one privileged host daemon**
  (`templar-hostd`). One state store, one event history ("why did that happen? undo."), one typed settings
  schema, one CLI (`templar`), one portal backend, one selected provider per exclusive system role.
* **Compositor core vs shell.** A conservative single-scene compositor (`templar-comp`, on Smithay) with typed overlay
  layers and a compositor-owned recovery band on every window; everything decorative restarts freely.
* **Transactional kernel/NVIDIA updates** on btrfs deployments with systemd boot assessment and a
  system-scope health probe.

## Layout

```
crates/      pure library cores      services/   daemons        session/     entry + systemd units
compositor/  integration crate       shell/ apps/ portals/ cli/  user-facing binaries
interfaces/  D-Bus / Wayland / JSON contracts (single source of truth)
distro/      base.toml, seeds, metapackages, mkosi image, apt, policy, branding, release, hardware, deploy, forks
debian/      source package          installer/  (later)        tests/  tools/xtask  docs/  ci/
```

Full map: `ARCHITECTURE.md`. Every directory has a `README.md`.

## Build

```sh
just check          # fmt + clippy -D warnings + tests + msrv
cargo run -p templar -- doctor --root /   # the integration-check engine
cargo run -p templar-sessiond -- --dry-run
cargo run -p templar-resourced -- --once
just deb            # cargo-deb packages into target/debian/
```

Toolchain: Rust 1.97.1 for development, MSRV 1.97 (ADR-0008). Licence: AGPL-3.0-only for this
repository (`templar-comp` is GPL-3.0-only, ADR-0012); forks keep their upstream licences.

## Project knowledge

Durable decisions live in the AKR ledger (`.akr/`, `knowledge.*` tools) and are rendered to
`docs/generated/`; see `AGENTS.md`.
