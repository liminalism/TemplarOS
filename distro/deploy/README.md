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

**How userspace knows what booted.** Every LegeOS boot entry carries `legeos.deployment=<id>` on
its kernel command line — the root filesystem label is identical for every deployment, so nothing
else on that line distinguishes them. `lege-hostd`'s `reconcile_boot()` reads it at startup: a
candidate staged but *not* booted means an attempt was spent (GRUB's one-shot default was consumed
and the firmware fell back), so the counter comes down, and at zero the candidate is abandoned for
the known-good deployment. systemd-boot decrements its own `+tries` suffix in the loader and
reconciliation is a no-op there. **The GRUB path depends on the generated entries carrying that
parameter**, which the boot-entry generation work under ADR-0004 has to provide; until it does,
reconciliation reports that it cannot tell and counts nothing, rather than guessing and burning an
attempt on a candidate that is running fine.

**Open (ADR-0004):** Secure-Boot posture (LegeOS-signed systemd-boot + MOK vs Canonical-signed GRUB +
hostd counter) and snapshot-vs-A/B for Stage 4. Dev/VM images: systemd-boot, SB off.
