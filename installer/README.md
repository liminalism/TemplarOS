# installer/ — the target-machine provisioner

`provision.sh` takes one target from an Ubuntu 26.04 base to the exact Developer Edition layout.
It is deliberately *not* an installer: one layout, no OEM flow, no arbitrary partitioning, no
graphical pages. Everything it does is either declared in `distro/deploy/layout.toml` (contract C3)
or is a package name it takes as a variable. Installed by `templaros-cli` as `templaros-install`
(`/usr/libexec/templaros/provision`), which is how the live medium runs it without a checkout.

```sh
# Plan. Writes nothing, needs no privileges, and is what you should read first.
installer/provision.sh --image /var/tmp/templaros.img --user dev

# Do it, to an image file, as root, in a disposable VM or container.
sudo installer/provision.sh --image /var/tmp/templaros.img --user dev --password-stdin
sudo installer/provision.sh --image /var/tmp/templaros.img --user dev --password-stdin \
    --apply --plan /var/tmp/templaros.img.plan.json

# The reference laptop: first identify the OS drive by model, serial, the lubuntu_2504
# label and partition layout. NVMe numbers can swap. The disk containing label Second
# is the data drive and must never be passed here.
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --free-space /dev/OS_DISK \
    --user dk --password-stdin --plan-out /tmp/templaros-install.json
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --free-space /dev/OS_DISK \
    --user dk --password-stdin --apply --plan /tmp/templaros-install.json

# Two partitions somebody already made (formatted; the table is not touched).
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --esp /dev/ESP_PART --root /dev/ROOT_PART --user dk

# A whole disk. Three separate acknowledgements, because it destroys one.
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 \
    installer/provision.sh --device /dev/sdX --i-mean-it --user dev --apply
```

The archive it installs from is `/templaros/devrepo` on the live medium and `/var/lib/templaros/devrepo`
elsewhere (`--archive DIR` overrides). It is copied whole to the target's `/var/lib/templaros/devrepo`,
so the installed system's `templar dev status` knows the set it came from and `templar dev rollback` has
somewhere to go.

## What it refuses to do

The refusals are checked before the first destructive step, not scattered among them:

- a target under `/dev`, or any block device, passed as `--image` — the flag has to match the thing;
- `--device` without `--i-mean-it` **and** `TEMPLAROS_PROVISION_ALLOW_DEVICE=1` in the environment. Two
  switches in two places, so that a command line copied out of a document cannot reach a disk;
- `--device` naming the disk this system booted from, under any combination of flags;
- `--free-space` whose largest free block is smaller than the layout's minimum (ESP `size_max` +
  root `size_min`). A gap anywhere on the disk is accepted — a leading hole left by a deleted OS
  is the reference-laptop case; the new partitions are confined to that block and cannot reach a
  neighbour;
- `--esp`/`--root` naming a whole disk, or a partition that is mounted;
- `--apply` as a non-root user, which would fail halfway through and leave a half-made target.

Without `--apply` it prints a numbered plan with every command resolved and exits. That plan is
what `tests/integration/provision-qemu.sh` asserts against.

## What it produces

1. **GPT + UEFI, no legacy BIOS.** ESP (vfat, ≤1 GiB, `TEMPLAR_ESP`) and root (btrfs,
   `templaros-root`). Sizes and labels from `layout.toml`.
2. **Btrfs subvolumes** `@ @home @var @snapshots @deployments`, from the same declaration the
   image's `mkosi.repart` uses. `@deployments` is where `templar-hostd` stages candidate snapshots.
3. **An Ubuntu 26.04 base** (`mmdebstrap`, or `debootstrap` when that is absent), with Canonical's
   archive and the local TemplarOS archive (`/var/lib/templaros/devrepo`, the `current` marker unless
   `--set` says otherwise), trusted through `templaros-archive-keyring` and nothing else. The set is
   verified with `templar dev repo verify` before it is used; `staged` is never installable.
4. **The TemplarOS package set** by metapackage name — the C1 names are variables at the top of the
   script, so a rename by D1 is one edit and not a rewrite.
5. **An Ubuntu-signed kernel and the matching Canonical-signed NVIDIA LRM.** No DKMS, no self-built
   module, no MOK enrolment (ADR-0004). `/etc/apt/apt.conf.d/01templaros-keep-kernels` keeps the
   previous kernel and its modules from being autoremoved, so the known-good kernel stays
   selectable — that is policy, not an accident of apt's mood.
6. **OS identity** from `distro/branding/os-release`.
7. **An account with no baked-in password.** `--password-stdin` reads one from stdin (never echoed,
   never written to a file, never in an argument list); otherwise the account is created with an
   expired password and the first login has to set one. Root is locked either way. There is no
   release password and no autologin — those exist only in the dev/VM image, in
   `distro/image/mkosi.conf.d/90-dev-credentials.conf`, which says so on every line.
8. **Session units and the greeter**, the same steps `distro/image/mkosi.postinst` performs.
9. **The production boot path**: Canonical-signed shim → Canonical-signed GRUB → Ubuntu-signed
   kernel, `GRUB_DEFAULT=saved`, and `/etc/grub.d/09_templaros` so `grub.cfg` sources the entries
   `templar-hostd` generates. `grub-install` runs with `--no-nvram`; on a real disk the firmware entry
   (`TemplarOS`, `\EFI\TemplarOS\shimx64.efi`) is registered from the host side and **appended** to the
   boot order, never put first — the firmware menu is how the first boots happen, and
   `efibootmgr -o` is a person's decision after `CHECKLIST.md` passes. A second ESP on a second disk
   is an ordinary UEFI configuration; the existing one is not touched. The first deployment is seeded: its kernel and initramfs are copied into
   `/EFI/templaros/<id>/`, its id is recorded as known-good, and `/boot/grub/templaros-entries.cfg` is
   written in the same shape hostd will rewrite it in.
10. **Diagnostics**: `templar doctor --root <mount>` against the finished tree.

## Afterwards

`installer/CHECKLIST.md` is the part a script cannot do: confirming on the real machine that Secure
Boot is enabled and that shim, GRUB, the kernel and the NVIDIA modules are all trusted by it, that
the previous kernel is still selectable, and that a rollback works. Run it once per machine, and
again after any change to the boot path.

## The installation medium

The live ISO (`distro/iso/`, ISO-PLAN.md) is a TemplarOS session that carries the build set at
`/templaros/devrepo` and this provisioner as `templaros-install`. Installing is running it from a
terminal in that session, reading the plan, and adding `--apply`. A graphical installer (Calamares
versus subiquity) remains a product decision for a release that has users who did not build it.
