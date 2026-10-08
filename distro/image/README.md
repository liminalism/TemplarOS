# distro/image — mkosi disk image (dev / CI / VM)

`mkosi.conf` + `mkosi.conf.d/` + `mkosi.repart/` (ESP + btrfs subvolumes) + `mkosi.extra/` +
`mkosi.postinst`. Produces `mkosi.output/templaros_0.1.raw`, bootable in `mkosi vm` and dd-able to a USB
stick. The host requires mkosi, systemd/repart >= 256 and btrfs-progs >= 6.12 for
offline Btrfs subvolume/default-subvolume construction (see
[repart.d](https://www.freedesktop.org/software/systemd/man/latest/repart.d.html)).
`just image build` composes package/seed/branding/archive drop-ins, builds with
mkosi, then invokes the owned `finalize.sh` loop-device/private-mount operation.
The finalizer records actual root/ESP identities, prepares narrow State binds,
copies a matching boot pair, writes an initial UUID/subvolume boot entry and
seeds fallback metadata. Repart creates actual `@`, `@home`, State `@var`,
`@snapshots` and deployment namespaces; `@var` is not a blanket `/var` copy.

Writable is the default. `TEMPLAROS_READ_ONLY=1 just image build` explicitly seals
the completed System root; it does not claim owner RO boot/password/recovery
proof. See `installer/CHECKLIST.md` and `docs/architecture/ownership-tiers.md`.

Finalization requires an unused regular, non-symlink raw image and owns only its
exact loop device and private mounts. It never touches the live desktop, selects
host boot defaults or attaches an already-used image. Raw-image SHA256SUMS are
refreshed after modification. Signed image/checksum artifacts are refused before
mutation: there is no implicit re-signing or stale-signature success path.
Do not distribute a partially finalized image after an error.

`mkosi summary` describes configuration only; it does not establish image build,
GRUB/systemd-boot boot selection, Secure Boot/module trust, mounted State or
password crash-recovery success. Building also requires network and the selected
signed TemplarOS archive; hardware/loop proof is recorded separately by the owner.
