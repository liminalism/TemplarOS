# distro/deploy — boot deployments and rollback (ADR-0004)

**Layout.** Declared once, in [`layout.toml`](layout.toml) — partitions, filesystem labels,
subvolumes, the deployment id grammar, the kernel command line and the two boot managers. Three
things consume it: `distro/image/mkosi.repart/*.conf` (the dev/VM image), `installer/provision.sh`
(a target machine), and `services/lege-hostd/src/boot_entries.rs`, whose compiled-in constants are
pinned to it by `cargo test -p lege-hostd layout`. Changing the layout means changing that file;
anything else drifts.

Root on btrfs: `@` (root), `@home`, `@var`, `@snapshots`, `@deployments`. A *deployment* is a
read-only snapshot of `@` under `@deployments/<id>` plus a hostd-owned directory on the ESP
(`/EFI/legeos/<id>/`) holding that deployment's kernel, initrd and entry — so Ubuntu's initramfs
post-update hooks cannot rewrite the previous entry from under us, and NVIDIA userspace rolls back
*with* the modules.

**Flow (kernel/NVIDIA transaction).**
1. `lege-hostd` snapshots `@` → candidate; installs the change set into the candidate (apt in a
   child transient unit, journal in `/var/lib/legeos/hostd/transactions/`).
2. Verifies signatures / version lock / initramfs contents.
3. `BootManager::stage_candidate(tries=3)`: systemd-boot entry `legeos-<id>+3.conf` (boot
   assessment) — or, on the GRUB backend, a regenerated `/boot/grub/legeos-entries.cfg` plus
   `grub-reboot <id>` against hostd's own counter.
4. Reboot. `lege-boot-health.service` (system scope) must succeed before `boot-complete.target`;
   `systemd-bless-boot` (or `grub-set-default`) marks the entry good → `Phase::Promoted`.
5. Failure: counter reaches 0 (or `FailureAction=reboot-force`) → previous default boots →
   `Phase::RolledBack`, explained in history.

**How userspace knows what booted.** Every LegeOS boot entry carries `legeos.deployment=<id>` on
its kernel command line — the root filesystem label is identical for every deployment, so nothing
else on that line distinguishes them. `lege-hostd`'s `reconcile_boot()` reads it at startup: a
candidate staged but *not* booted means an attempt was spent (GRUB's one-shot default was consumed
and the firmware fell back), so the counter comes down, and at zero the candidate is abandoned for
the known-good deployment. systemd-boot decrements its own `+tries` suffix in the loader and
reconciliation is a no-op there.

**The GRUB entries.** `services/lege-hostd/src/boot_entries.rs` writes
`/boot/grub/legeos-entries.cfg` whole on every stage, promotion and rollback: the known-good
deployment first, then the staged candidate. `/etc/grub.d/09_legeos` (shipped as
[`grub.d/09_legeos`](grub.d/09_legeos), installed by the provisioner) makes `grub.cfg` `source`
that file, so a rewrite takes effect at the next boot without running `grub-mkconfig` — which would
rerun Ubuntu's generators and put a privileged daemon in charge of the whole menu. The menu entry
id *is* the deployment id, unadorned, which is exactly what `GRUB_DEFAULT=saved` plus
`grub-reboot <id>` / `grub-set-default <id>` address. hostd never writes `/etc/default/grub`; the
provisioner sets `GRUB_DEFAULT=saved` there once.

The GRUB backend remembers the known-good deployment id in `<state-dir>/grub/known-good`. The
`BootManager` trait hands a backend a candidate to stage and a previous deployment to roll back to,
but never the known-good deployment as a standing fact, and the entries file is regenerated whole —
so the backend keeps that one id itself. `deployments.json` remains the identity authority;
`known-good` is the boot backend's own memory, seeded by the provisioner for the first deployment.

**Secure Boot (resolved, ADR-0004).** Bare metal boots Canonical-signed shim → Canonical-signed
GRUB → Ubuntu-signed kernel → Canonical-signed matching NVIDIA LRM. No LegeOS signing
infrastructure and no MOK enrolment; a LegeOS MOK is deferred until a self-built kernel exists.
Dev/VM images built by mkosi keep systemd-boot with Secure Boot off. Snapshot-vs-A/B for Stage 4
remains open.

## Recovery shell — when the graphical session is not available

None of this needs a desktop, and none of it needs `lege-shell`, `greetd` or a working compositor.

**Rescue on the machine itself.** At the GRUB menu (hold `Shift`, or `Esc`) pick the known-good
deployment by name, or edit an entry with `e` and append `systemd.unit=rescue.target` (a root
shell, local filesystems mounted) or `3` for multi-user without a graphical target. `systemd.debug-shell=1`
puts a shell on tty9 in parallel with a boot that is otherwise proceeding — useful when the session
starts and then dies. From any of those:

```sh
lege dev status              # which build set is installed, running, rollback
lege update status           # current / previous / known-good deployments
journalctl -b -u lege-boot-health -u lege-hostd
lege doctor --root /
```

**GRUB itself is broken.** At the `grub>` prompt: `set prefix=(hd0,gpt2)/boot/grub`,
`insmod normal`, `normal`. If `legeos-entries.cfg` is missing or unreadable, Ubuntu's own `10_linux`
entries are still in `grub.cfg` and boot the last kernel apt installed — that is why 09_legeos adds
entries rather than replacing the menu.

**From a live USB.** The subvolume layout is the whole trick:

```sh
mount -o subvol=@ /dev/disk/by-label/legeos-root /mnt
mount -o subvol=@var /dev/disk/by-label/legeos-root /mnt/var   # and @home, per layout.toml
mount /dev/disk/by-label/LEGEOS_ESP /mnt/boot/efi
for d in dev proc sys run; do mount --rbind /$d /mnt/$d; done
chroot /mnt
```

Inside the chroot:

- **restore the boot entries** — `cat /var/lib/legeos/hostd/grub/known-good` names the deployment to
  boot; its kernel and initrd are already at `/boot/efi/EFI/legeos/<id>/`. Recreate
  `/boot/grub/legeos-entries.cfg` in the shape `installer/provision.sh` writes it (one `menuentry`
  with `--id '<id>'` and `linux … legeos.deployment=<id>`), then `grub-set-default <id>`;
- **reinstall the bootloader** — `apt-get install --reinstall shim-signed grub-efi-amd64-signed`
  then `grub-install --target=x86_64-efi --efi-directory=/boot/efi --bootloader-id=LegeOS
  --uefi-secure-boot` and `update-grub`;
- **roll back by hand** — a deployment's snapshot is at `@deployments/<id>`; `btrfs subvolume list`
  shows them. Restoring one is a `btrfs subvolume snapshot` back over `@`, which is the operation
  `lege-hostd` performs and the one to reach for last, after the boot menu and the entries file have
  both been ruled out.

**Do not** reach for `grub-mkconfig` to "fix" the LegeOS entries: it regenerates `grub.cfg`,
including the `source` line, and leaves `legeos-entries.cfg` exactly as broken as it was.
