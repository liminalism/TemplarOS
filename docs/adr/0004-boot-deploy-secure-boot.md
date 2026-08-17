# ADR-0004: Boot deployments, rollback and the Secure-Boot posture

* **Status:** proposed
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
The docs require kernel + NVIDIA modules + userspace to succeed or roll back **as a pair**. systemd boot
assessment (`+N` counters, `boot-complete.target`, `systemd-bless-boot`) already implements the watchdog
sequence — but (a) a boot entry alone does not roll back NVIDIA userspace on a shared root, (b) Ubuntu's
initramfs post-update hooks rewrite entries on the ESP, and (c) Ubuntu 26.04 ships `systemd-boot` but **no
Canonical-signed systemd-boot**, so with Secure Boot on, shim will not chain to it without a MOK.

## Decision
* Root on **btrfs subvolumes from the first image**; a deployment = read-only snapshot + a hostd-owned ESP
  directory (`/EFI/legeos/<id>/`) with kernel, initrd and entry. Kernels stay Ubuntu-signed (Type #1
  entries, no self-built UKI); NVIDIA modules from Ubuntu's Canonical-signed LRM, never DKMS.
* `lege-boot-health` is a **system-scope** unit before `boot-complete.target`, `TimeoutStartSec` +
  `FailureAction=reboot-force`.
* `BootManager` trait with two backends: `systemd-boot` (bless-boot) and `grub` (`GRUB_DEFAULT=saved`,
  `grub-reboot`, hostd counter).
* Dev/VM images (mkosi): systemd-boot, Secure Boot off.

## Open (to decide before Stage 3 on real hardware)
1. LegeOS-signed systemd-boot + one-time MOK enrolment at install (also enables signing own UKIs), or
2. Canonical-signed GRUB + hostd env-block counter (works with SB out of the box), or
3. SB off (dev only).
And for Stage 4: snapshot deployments (current leaning) vs A/B images (systemd-sysupdate).
