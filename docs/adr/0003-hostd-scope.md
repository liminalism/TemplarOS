# ADR-0003: One privileged host daemon (templar-hostd) — and what it does not do

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
Staged updates, driver authority, boot deployments, storage reserve and hardware profile are all root
policy over apt/systemd; each as its own root daemon multiplies polkit surfaces.

## Decision
`templar-hostd` (system bus `org.templaros.Host`, one polkit policy `org.templaros.host.*`, AppArmor profile) hosts
Updates, Deployments (`BootManager` trait), Drivers/GPU-mode, Storage reserve, Hardware profile. It keeps an
on-disk transaction journal (resume mid-apt after restart) and runs apt as a child transient unit.
**Timezone/location policy is not here**: it is network-facing (untrusted JSON) and lives in `templar-sessiond`,
setting the zone through polkit-gated `org.freedesktop.timedate1`.

## Consequences
Fewer privileged surfaces; one place for `templar update`, `templar doctor` support-violation checks and the
hardware profile.
