# installer/ — the target-machine provisioner

`provision.sh` takes one disposable target from an Ubuntu 26.04 base to the exact Developer
Edition layout. It is deliberately *not* an installer: one machine shape, no dual boot, no OEM
flow, no arbitrary partitioning, no live ISO, no graphical pages. Everything it does is either
declared in `distro/deploy/layout.toml` (contract C3) or is a package name it takes as a variable.

```sh
# Plan. Writes nothing, needs no privileges, and is what you should read first.
installer/provision.sh --image /var/tmp/legeos.img --user dev

# Do it, to an image file, as root, in a disposable VM or container.
sudo installer/provision.sh --image /var/tmp/legeos.img --user dev --apply

# A real disk. Three separate acknowledgements, because it destroys one.
sudo LEGEOS_PROVISION_ALLOW_DEVICE=1 \
    installer/provision.sh --device /dev/sdX --i-mean-it --user dev --apply
```

## What it refuses to do

The refusals are checked before the first destructive step, not scattered among them:

- a target under `/dev`, or any block device, passed as `--image` — the flag has to match the thing;
- `--device` without `--i-mean-it` **and** `LEGEOS_PROVISION_ALLOW_DEVICE=1` in the environment. Two
  switches in two places, so that a command line copied out of a document cannot reach a disk;
- `--device` naming the disk this system booted from, under any combination of flags;
- `--apply` as a non-root user, which would fail halfway through and leave a half-made target.

Without `--apply` it prints a numbered plan with every command resolved and exits. That plan is
what `tests/integration/provision-qemu.sh` asserts against.

## What it produces

1. **GPT + UEFI, no legacy BIOS.** ESP (vfat, ≤1 GiB, `LEGEOS_ESP`) and root (btrfs,
   `legeos-root`). Sizes and labels from `layout.toml`.
2. **Btrfs subvolumes** `@ @home @var @snapshots @deployments`, from the same declaration the
   image's `mkosi.repart` uses. `@deployments` is where `lege-hostd` stages candidate snapshots.
3. **An Ubuntu 26.04 base** (`mmdebstrap`, or `debootstrap` when that is absent), with Canonical's
   archive and the local LegeOS archive (`/var/lib/legeos/devrepo`, the `current` marker unless
   `--set` says otherwise), trusted through `legeos-archive-keyring` and nothing else. The set is
   verified with `lege dev repo verify` before it is used; `staged` is never installable.
4. **The LegeOS package set** by metapackage name — the C1 names are variables at the top of the
   script, so a rename by D1 is one edit and not a rewrite.
5. **An Ubuntu-signed kernel and the matching Canonical-signed NVIDIA LRM.** No DKMS, no self-built
   module, no MOK enrolment (ADR-0004). `/etc/apt/apt.conf.d/01legeos-keep-kernels` keeps the
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
   kernel, `GRUB_DEFAULT=saved`, and `/etc/grub.d/09_legeos` so `grub.cfg` sources the entries
   `lege-hostd` generates. The first deployment is seeded: its kernel and initramfs are copied into
   `/EFI/legeos/<id>/`, its id is recorded as known-good, and `/boot/grub/legeos-entries.cfg` is
   written in the same shape hostd will rewrite it in.
10. **Diagnostics**: `lege doctor --root <mount>` against the finished tree.

## Afterwards

`installer/CHECKLIST.md` is the part a script cannot do: confirming on the real machine that Secure
Boot is enabled and that shim, GRUB, the kernel and the NVIDIA modules are all trusted by it, that
the previous kernel is still selectable, and that a rollback works. Run it once per machine, and
again after any change to the boot path.

## Later stage: an installation ISO

Still deferred, and still an ADR that has not been written: Calamares (Lubuntu lineage) versus
subiquity / `ubuntu-desktop-provision` (autoinstall). Whatever it is, it does what this script
does — `lege-hwdetect` picks exactly one NVIDIA flavour and the hardware profile, the layout comes
from `distro/deploy/layout.toml`, `os-release` from `distro/branding`, and first-run hands to
sessiond's migration module. No separate OOBE application. A provisioner that a person reads before
running is the honest shape for Developer Edition 0.1; a graphical installer is a product decision
for a release that has users who did not build it.
