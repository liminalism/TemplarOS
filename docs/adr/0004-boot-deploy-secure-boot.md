# ADR-0004: Boot deployments, rollback and the Secure-Boot posture

* **Status:** accepted (2026-09-03; the Secure-Boot posture was resolved from the three options below)
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`
* **Record:** `@templaros.decision.boot-deploy-secure-boot`

## Context
The docs require kernel + NVIDIA modules + userspace to succeed or roll back **as a pair**. systemd boot
assessment (`+N` counters, `boot-complete.target`, `systemd-bless-boot`) already implements the watchdog
sequence — but (a) a boot entry alone does not roll back NVIDIA userspace on a shared root, (b) Ubuntu's
initramfs post-update hooks rewrite entries on the ESP, and (c) Ubuntu 26.04 ships `systemd-boot` but **no
Canonical-signed systemd-boot**, so with Secure Boot on, shim will not chain to it without a MOK.

## Decision
* Root on **btrfs subvolumes from the first image**; a deployment = read-only snapshot + a hostd-owned ESP
  directory (`/EFI/templaros/<id>/`) with kernel, initrd and entry. Kernels stay Ubuntu-signed (Type #1
  entries, no self-built UKI); NVIDIA modules from Ubuntu's Canonical-signed LRM, never DKMS.
* `templar-boot-health` is a **system-scope** unit before `boot-complete.target`, `TimeoutStartSec` +
  `FailureAction=reboot-force`.
* `BootManager` trait with two backends: `systemd-boot` (bless-boot) and `grub` (`GRUB_DEFAULT=saved`,
  `grub-reboot`, hostd counter).
* Dev/VM images (mkosi): systemd-boot, Secure Boot off.
* **The production bare-metal path is option 2 below: Canonical-signed shim → Canonical-signed GRUB →
  Ubuntu-signed kernel → Canonical-signed matching NVIDIA LRM.** It gives real Secure Boot with no
  TemplarOS signing infrastructure at all, which is what `@templaros.constraint.first-release-scope` asks
  for. `templar-hostd` generates the TemplarOS menu entries itself
  (`services/templar-hostd/src/boot_entries.rs` → `/boot/grub/templaros-entries.cfg`, sourced by
  `/etc/grub.d/09_templaros`), and every generated entry carries `templaros.deployment=<id>` for the
  candidate and the known-good deployment — which is what closes
  `@templaros.question.grub-entries-must-carry-the-deployment-id` and lets `reconcile_boot` stop
  answering "cannot tell".
* The layout the two backends share is declared once in `distro/deploy/layout.toml` and pinned by
  `cargo test -p templar-hostd layout`.

## Considered and not taken
1. **TemplarOS-signed systemd-boot + one-time MOK enrolment at install** (which would also enable signing
   our own UKIs) — deferred until a self-built kernel actually exists. Until then it buys a MOK prompt
   at install time and a signing pipeline to maintain, in exchange for a loader we have no other reason
   to replace.
2. **Canonical-signed GRUB + hostd's own counter** — taken. See above.
3. **Secure Boot off** — dev/VM images only, never the reference machine.

## Still open
For Stage 4: snapshot deployments (current leaning, and what the code does today) vs A/B images
(`systemd-sysupdate`).
