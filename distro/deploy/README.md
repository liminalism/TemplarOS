# distro/deploy — boot deployments and rollback (ADR-0004)

**Layout.** Root on btrfs: `@` (root), `@home`, `@var`, `@snapshots`. A *deployment* is a read-only
snapshot of `@` plus a hostd-owned directory on the ESP (`/EFI/legeos/<deployment-id>/`) holding that
deployment's kernel, initrd and boot entry — so Ubuntu's initramfs post-update hooks cannot rewrite the
previous entry from under us, and NVIDIA userspace rolls back *with* the modules.

**Flow (kernel/NVIDIA transaction).**
1. `lege-hostd` snapshots `@` → candidate; installs the change set into the candidate (apt in a child
   transient unit, journal in `/var/lib/legeos/hostd/transactions/`).
2. Verifies signatures / version lock / initramfs contents.
3. `BootManager::stage_candidate(tries=3)`: systemd-boot entry `legeos-<id>+3.conf` (boot assessment) —
   or GRUB `grub-reboot` with hostd's own counter.
4. Reboot. `lege-boot-health.service` (system scope) must succeed before `boot-complete.target`;
   `systemd-bless-boot` marks the entry good → `Phase::Promoted`.
5. Failure: counter reaches 0 (or `FailureAction=reboot-force`) → previous default boots →
   `Phase::RolledBack`, explained in history.

**Open (ADR-0004):** Secure-Boot posture (LegeOS-signed systemd-boot + MOK vs Canonical-signed GRUB +
hostd counter) and snapshot-vs-A/B for Stage 4. Dev/VM images: systemd-boot, SB off.
