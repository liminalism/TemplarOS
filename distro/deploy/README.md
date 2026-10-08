# distro/deploy — boot deployments and rollback (ADR-0004)

**Layout and authority.** [`layout.toml`](layout.toml) declares partitions,
subvolume identities, mounted deployment/bootstrap/State paths, the ESP namespace
and System/State/Work ownership. [`components.toml`](components.toml) declares
service/package writable surfaces and decision exceptions. Image construction,
the provisioner and the real hostd factory consume these values. Transaction
identity persists both the mounted path and relative boot subvolume; a restart
must not reinterpret an already-staged root after a layout change.

The bootstrap root is `@`; candidates are `@deployments/<id>`, mounted under
`/var/lib/templaros/deployments`, with paired boot files at `/EFI/templaros/<id>/`
on the ESP. `@var` mounts at `/var/lib/templaros/state`, not `/var`: declared
directory binds preserve runtime State while dpkg/APT databases, APT cache,
machine identity and account files remain root-paired System. A legacy whole-/var
root requires reviewed offline migration, not an inferred database copy.

**Writable is the default.** The provisioner/image finalizer have an explicit
read-only opt-in. It seals the completed root only after package work, fstab,
immutable boot copies and baseline metadata are complete. Owner disposable-media
and physical boot/password/recovery proofs remain required before RO adoption;
source tests are not hardware evidence.

**Flow (native package or kernel/NVIDIA transaction).**
1. Hostd creates a writable candidate; APT indexes/downloads/configures/installs
   entirely in that candidate's transient RootDirectory. A native developer
   deploy queues this operation; it does not modify live packages or restart the
   desktop. Only public signed archive inputs enter the child.
2. Verify a matching kernel/initrd pair, requested installed package versions,
   NVIDIA branch pairing/no-DKMS structure, account compatibility and shared-State
   reader windows. This is not cryptographic module-signature/ABI proof.
3. Rewrite candidate fstab to its own relative subvolume, copy paired boot files
   into the owned ESP namespace, then seal if the installation opted into RO.
4. Stage three boot attempts with UUID + candidate subvolume + explicit ro/rw.
   systemd-boot uses its tries suffix; GRUB uses an owned durable counter and
   one-shot selection. The initial seeded baseline is a fallback, not evidence
   that it has already passed a physical health gate.
5. Reboot. Boot health promotes; exhausted attempts select the known-good root.
   Root selection never overwrites `@` or rewinds user profiles/Work.

**How userspace knows what booted.** Every TemplarOS boot entry carries `templaros.deployment=<id>` on
its kernel command line — the root filesystem label is identical for every deployment, so nothing
else on that line distinguishes them. `templar-hostd`'s `reconcile_boot()` reads it at startup: a
candidate staged but *not* booted means an attempt was spent (GRUB's one-shot default was consumed
and the firmware fell back), so the counter comes down, and at zero the candidate is abandoned for
the known-good deployment. systemd-boot decrements its own `+tries` suffix in the loader and
reconciliation is a no-op there.

**GRUB entries and environment.** Writable-root installations use
`/boot/grub/templaros-entries.cfg`. Opt-in sealed roots keep both the generated
entries and mutable `grubenv` under `/boot/efi/EFI/templaros/grub/`.
[`grub.d/09_templaros`](grub.d/09_templaros) locates that ESP by UUID, loads/consumes
its one-shot selection and sources the generated entries. Hostd passes the
corresponding `--boot-directory` to GRUB tools; systemd-boot selection passes
the declared `--esp-path`. Ubuntu's other generators/menu entries remain intact.

The known-good record stores id, relative subvolume and root mode. Its writes,
counter writes and the host registry/journal are synchronized; malformed/future
state must refuse rather than lose the only fallback. Schema-1 registry/journal
readers accept the legacy representation and retain `.pre-schema-1` bytes before
rewriting. Both fallback and candidate advertise compatible State/account readers.

**Password maintenance.** `templar account password` uses interactive polkit
self-authentication and a sealed secret FD, not a username/UID supplied by the
caller. Hostd runs real chpasswd/PAM in a private scratch snapshot and propagates
only the caller's encrypted shadow row to compatible registered roots. A root-only
recoverable intent preserves original RO properties and file metadata; a separate
readable non-secret persistent marker gates the additive common-account PAM profile.
Boot recovery runs for either marker or intent before login. The existing PAM,
fingerprint and NSS stack is preserved. See
[`ownership-tiers.md`](../../docs/architecture/ownership-tiers.md) for the protocol
and fail-closed cases; direct account changes that diverge roots require reviewed
reconciliation, never silent credential downgrade.

**Secure Boot (resolved, ADR-0004).** Bare metal boots Canonical-signed shim → Canonical-signed
GRUB → Ubuntu-signed kernel → Canonical-signed matching NVIDIA LRM. No TemplarOS signing
infrastructure and no MOK enrolment; a TemplarOS MOK is deferred until a self-built kernel exists.
Dev/VM images built by mkosi keep systemd-boot with Secure Boot off. Module/kernel
trust, actual boot selection and physical rollback remain owner checklist items.

## Recovery shell — when the graphical session is not available

None of this needs a desktop, and none of it needs `templar-shell`, `greetd` or a working compositor.

**Rescue on the machine itself.** At the GRUB menu (hold `Shift`, or `Esc`) pick the known-good
deployment by name, or edit an entry with `e` and append `systemd.unit=rescue.target` (a root
shell, local filesystems mounted) or `3` for multi-user without a graphical target. `systemd.debug-shell=1`
puts a shell on tty9 in parallel with a boot that is otherwise proceeding — useful when the session
starts and then dies. From any of those:

```sh
templar dev status              # which build set is installed, running, rollback
templar update status           # current / previous / known-good deployments
journalctl -b -u templar-boot-health -u templar-hostd
templar doctor --root /
```

**GRUB itself is broken.** At the `grub>` prompt: `set prefix=(hd0,gpt2)/boot/grub`,
`insmod normal`, `normal`. If `templaros-entries.cfg` is missing or unreadable, Ubuntu's own `10_linux`
entries are still in `grub.cfg` and boot the last kernel apt installed — that is why 09_templaros adds
entries rather than replacing the menu.

**From a live USB.** The subvolume layout is the whole trick:

```sh
mount -o ro,subvol=@ UUID=<recorded-root-uuid> /mnt
mount -o subvol=@var UUID=<recorded-root-uuid> /mnt/var/lib/templaros/state
mount UUID=<recorded-esp-uuid> /mnt/boot/efi
# Bind only the declared State directories from layout.toml. Never mount @var over /var.
# Review pending account recovery before permitting login or selecting a root.
```

From the reviewed offline recovery environment:

- **restore the boot entries** — the persistent host State's `grub/known-good`
  record identifies both deployment and subvolume. Its paired kernel/initrd must
  exist in the ESP. Recreate the appropriate writable-root or ESP-owned entries
  file with that UUID/subvolume/mode; do not infer a root from a host mount path;
- **repair the bootloader offline** — preserve Canonical signed shim/GRUB,
  firmware fallback and the owned ESP environment. Reinstalling a bootloader is
  an explicit offline repair, not a native live-root developer deployment;
- **roll back** — select a known registered deployment with `templar update
  rollback <id>` after compatible credentials/State readers are established.
  Do not copy a candidate back over `@`, restore profiles, or delete a pending
  password intent/marker to bypass recovery.

**Do not** reach for `grub-mkconfig` to "fix" the TemplarOS entries: it regenerates `grub.cfg`,
including the `source` line, and leaves `templaros-entries.cfg` exactly as broken as it was.
