# Architecture Decision Records

| # | Title | Status |
|---|---|---|
| 0001 | One user session policy daemon (`lege-sessiond`) | accepted |
| 0002 | Resource governor is a separate, tiny, protected process | accepted |
| 0003 | One privileged host daemon (`lege-hostd`) — and what it does not do | accepted |
| 0004 | Boot deployments, rollback and the Secure-Boot posture | **proposed** (decide before Stage 3) |
| 0005 | Forks pinned by manifest, not submodules | accepted |
| 0006 | mkosi images; installer ISO later | accepted |
| 0007 | Ubuntu 26.04 kernel 7.0 base; mainline 7.2 as transaction candidate | accepted |
| 0008 | Toolchain, MSRV 1.97, `org.legeos` (licence superseded by 0013) | accepted |
| 0009 | Packaging: cargo-deb → vendored source package | accepted |
| 0010 | systemd session; greetd + cosmic-greeter; compositor-owned lock | accepted |
| 0011 | Flatpak not snapd; telemetry-free crash handling | accepted |
| 0012 | Compositor built on Smithay, not forked from cosmic-comp | accepted |
| 0013 | Own code AGPL-3.0-only; `lege-comp` stays GPL-3.0-only | accepted |
| 0014 | `systemd-oomd` is the backstop; `lege-resourced` owns the session | accepted |

Mirrored as AKR `decision` records (`.akr/`, rendered in `docs/generated/`).
