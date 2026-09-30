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

# The reference laptop: one command from the live session (or the medium's last GRUB entry).
# It finds the OS drive by its lubuntu_2504 label and never writes the drive labelled Second,
# chooses between a fresh install, a rebuild of the TemplarOS pair and a resume by itself, and
# does the plan/apply dance below with a provisional password (dk / templaros, expired).
sudo templaros-autoinstall
# ...or, on a medium older than this script, straight from a checkout:
sudo /path/to/LegeOS/distro/iso/live/autoinstall.sh

# The same thing by hand. NVMe numbers can swap between boots; the disk containing label
# Second is the data drive and must never be passed here.
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --free-space /dev/OS_DISK \
    --user dk --password-stdin --plan-out /tmp/templaros-install.json
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --free-space /dev/OS_DISK \
    --user dk --password-stdin --apply --plan /tmp/templaros-install.json

# An install that stopped partway: what the pair holds, then carry on from it.
sudo templaros-install --esp /dev/ESP_PART --root /dev/ROOT_PART --inspect
sudo TEMPLAROS_PROVISION_ALLOW_DEVICE=1 templaros-install --esp /dev/ESP_PART --root /dev/ROOT_PART \
    --resume --user dk --password-stdin --plan-out /tmp/templaros-resume.json   # then --apply --plan

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

Ubuntu itself comes from the snapshot `distro/base.toml` pins (`snapshot_url`), not the moving
archive: the set's metapackages pin the NVIDIA modules and userspace to the exact versions its
closure resolved, and the archive drops those as soon as Ubuntu publishes the next kernel. The
installed system keeps the snapshot as its source; a newer base arrives with a newer set. The
components come from the same file (`components`): multiverse is among them because
`nvidia-cuda-toolkit`, which `templaros-dev-cuda` depends on, is in no other component.

Before its first write, every `--apply` resolves the whole package set in a scratch apt root (empty
dpkg status, the target's exact sources and set pin, `apt-get install --simulate`) and refuses with
one line per cause — `templaros-dev-cuda needs nvidia-cuda-toolkit — no configured source has it` —
instead of partitioning a disk it cannot finish. `--check-packages` asks the same question with no
target and no root; `distro/iso/build.sh` runs it before building a medium.

## Stopping partway, and carrying on

Every apply records its progress on the target, in `@var` at
`/var/lib/templaros/provision/state`: `formatted`, then `bootstrapped` once Ubuntu is in `@`, then
`complete`. The record names the root UUID, the ESP PARTUUID, the deployment id, the set, the
suite and the mirror it was written for. `--inspect` reads it (root mounts read-only) and says one
of:

- `fresh` — the pair has no installed system; install onto it;
- `resumable` — Ubuntu is bootstrapped for this suite and mirror; `--resume` keeps it and
  repeats every later step. Each of those is repeatable: `dpkg --configure -a` first, the account
  is only created if absent, `flatpak install --or-update`, and an existing firmware entry for this
  ESP's shim is kept rather than duplicated;
- `rebuild` — the pair is this provisioner's but cannot be resumed (it stopped before the
  bootstrap finished, the suite or mirror changed, or an earlier provisioner left no record);
  the same `--esp/--root` without `--reuse` reformats only those two partitions. A newer TemplarOS
  set is not a reason: the bootstrap is plain Ubuntu, and a resume repeats every step the set
  touches;
- `complete` — installed; nothing to do;
- `foreign` — not recognisably this provisioner's pair; nothing is done to it.

A failed apply still unmounts everything it mounted, then says which step it stopped at, whether
the target was modified, the recorded state, and the exact command that carries on.

The chroot steps get their own `/run` (a tmpfs, so nothing reaches `@`): Ubuntu's
`/etc/resolv.conf` is a symlink into `/run/systemd/resolve/`, which a never-booted target does not
have, so the file it names is written there from the live system's upstream resolvers, and
`getent ahostsv4 <mirror>` inside the target proves resolution before any step depends on it.

A privileged run re-executes itself in its own mount namespace, a slave of the host's
(`unshare --mount --propagation slave`). Host mounts are visible inside; nothing mounted or
unmounted inside — the target, its bind of `/dev`, `/dev/pts`, `/proc`, `/sys`, efivars, the
chroot's `/run` — can propagate back onto the live system, and the kernel drops all of it when the
run ends, however it ends.

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
`/templaros/devrepo` and this provisioner as `templaros-install`. On the reference laptop,
installing is `sudo templaros-autoinstall` (`distro/iso/live/autoinstall.sh`), or the medium's last
GRUB entry, which runs the same script with no session. Running it again after any failure is
always the next step. A graphical installer (Calamares versus subiquity) remains a product
decision for a release that has users who did not build it.
