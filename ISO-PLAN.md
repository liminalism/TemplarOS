# From the current tree to a LegeOS ISO you boot, install into its own partition, and develop from

Status: plan, 2026-09-05. Written against `e257ebb` on `developer-edition-0-1`, the host's real
partition table, and the tools this host can install. Nothing in it has been executed yet.

Governing ledger: `@legeos.milestone.developer-edition-0-1` (all seven checks unsatisfied),
`@legeos.work.self-hosting-legeos`, `@legeos.work.reference-machine-provisioning`,
`@legeos.work.developer-edition-package-closure`. This plan supersedes the *route* in
`docs/planning/self-hosting-legeos.md` §4–§5 (provision from Lubuntu) with "provision from a LegeOS
live ISO"; that document's §6–§7 (develop inside, retire Lubuntu) still apply unchanged and are not
repeated here. The Developer Edition lanes are tracked by `@legeos.milestone.developer-edition-0-1`; this is the route.

---

## 0. The model this plan adopts

The outside advice is accepted as written, with LegeOS names: a **source-integrated Ubuntu derivative
that rebuilds selectively**. Concretely, three tiers, each with a home in the tree:

| Tier | Meaning | Where it is declared | Today |
|---|---|---|---|
| **Native** | LegeOS source, LegeOS packages | `debian/`, `distro/forks/manifest.toml` | 13 Rust `.deb`s + `legeos-nm-tray` fork; 8 metapackages rendered, not built |
| **Rebuilt** | Ubuntu source + LegeOS patch queue → `…ubuntuN+legeos1` in the LegeOS archive | `distro/sources/<pkg>/` (new, §7) | **none**, deliberately |
| **Inherited** | Canonical's binary, unchanged | `distro/seeds/selection.toml` | everything else: 103 declared names |

Rules that follow from it and are already true in the tree, stated so nobody re-decides them:

- **No Ubuntu Desktop underneath.** The `bootstrap` layer starts from `ubuntu-minimal`; the seeds build
  the whole composition. The ISO, the installed system and the mkosi VM image derive from the one
  `selection.toml` (`@legeos.evidence.de01-seeds-generated-20260904`). The live medium adds a `live`
  layer to that file; it does not get its own list.
- **Kernel, NVIDIA and CUDA stay inherited** for this edition (ADR-0004, ADR-0007,
  `@legeos.decision.base-ubuntu-2604-kernel-70`). The Canonical-signed shim → GRUB → kernel → LRM chain
  is the whole reason Secure Boot works on day one without a LegeOS key.
- **A package moves to Rebuilt only with a one-line answer** to "what changes because LegeOS builds
  this rather than Canonical?" recorded beside it. "It's more ours" is not an answer. The rebuild
  *pipeline* is built once the OS boots (§7), so the first justified candidate costs an afternoon, not
  a project.

## 1. Where we are (verified on this host, 2026-09-05)

**Present and passing**

- Package declaration: 14 layers, every generated artifact (seeds, mkosi drop-ins, 8 `control.in`)
  matches it; `cargo xtask seeds --check` and `check-packaging` green.
- Build identity: one BuildSetId feeds cargo-deb and `debian/changelog`; `lege dev repo` publishes
  signed sets with `current`/`previous`/`staged`, `verify` re-checks a set from disk.
- Layout: `distro/deploy/layout.toml` is the single C3 declaration; hostd's `boot_entries.rs` writes
  `legeos-entries.cfg` with `legeos.deployment=<id>`, pinned to the layout by a test.
- Provisioner: `installer/provision.sh` plans by default; produces GPT+ESP+btrfs
  `@ @home @var @snapshots @deployments`, mmdebstrap base, shim+GRUB, first deployment stamped, no
  baked password. Takes only `--image FILE` or `--device` (whole disk, zapped).
- Rollback: registry persists current/previous/known-good; retention proven in tests; dry run names
  every set and deployment.
- Desktop: nested-session journey passes end to end for the first time; 18 command checks green in one
  sweep (`@legeos.evidence.acceptance-sweep-20260904`).

**Absent or blocking** (each becomes a numbered step below)

| # | Fact | Consequence |
|---|---|---|
| B1 | No Ubuntu 26.04 index has ever been seen by this tree; every seed name is *declared*, closure `unavailable` | the first `mmdebstrap resolute` will be the first time the names are tested |
| B2 | 8 metapackages have never been built into `.deb`s | `legeos-desktop` cannot be installed, so nothing can be provisioned |
| B3 | `legeos-nm-tray`'s pin lacks `--popup` and the `X-LegeOS-*` keys; they exist only in an uncommitted tree at `../not-rust-projects/nm-tray-2`; `preflight.sh` refuses to build | network widget on a fresh install is dead until those commits are pushed |
| B4 | Provisioner has no free-space / existing-partition mode | cannot run on this laptop at all |
| B5 | `distro/iso/` is one README; no live layer, no squashfs, no boot catalog, no ISO recipe | there is no medium |
| B6 | 5 workspace path deps (`../pixelkit`×4, `../bpg-rs`) + 4 in preview codecs point outside the repo | the installed LegeOS cannot rebuild LegeOS from a checkout |
| B7 | Host root is 99% full (6 GB free); the 70 GB free are on an NTFS volume | a rootfs cannot be built on either as-is |
| B8 | `mmdebstrap`, `mkosi`, `squashfs-tools`, `casper`, `germinate`, `shim-signed`, `grub-efi-amd64-signed` are installable (Ubuntu 25.10 host) but not installed | one `apt install` |
| B9 | Target: `nvme1n1` has a 55.9 GiB free tail; the existing ESP (`nvme0n1p4`) is 100 MiB, cannot be grown | new 1 GiB ESP + ~54 GiB root in that tail; Lubuntu untouched |
| B10 | Secure Boot is **enabled** on the laptop; the host's own NVIDIA module is a MOK-signed DKMS build | LegeOS must ship Canonical LRM, and the live ISO must boot under SB or the test is not the real test |

## 2. Definition of done

1. `distro/iso/build.sh` turns `selection.toml` + one named build set into `legeos-<set>.iso`, on any
   Ubuntu host, with a manifest of every package and its origin tier.
2. The ISO boots in QEMU with a Secure-Boot OVMF, into a **LegeOS session** (not a tty, not GNOME),
   with kitty, Files, the browser and `nmcli` working.
3. From that live session, `sudo legeos-install --free-space /dev/<disk>` installs into unallocated
   space on a disk that already has partitions, leaves those partitions alone, and registers a UEFI
   entry. Proven in QEMU against a fixture disk that mimics `nvme1n1`.
4. The installed system boots with `legeos.deployment=<id>` on `/proc/cmdline`, Secure Boot on, no
   MOK prompt, Canonical LRM loaded, WiFi via `nmcli` and via the Home network widget.
5. Inside the installed system: `just check` green, `just deb` builds a set, `sudo lege dev deploy`
   activates it, and `distro/iso/build.sh` rebuilds the ISO. That last one is the self-hosting proof.
6. `@legeos.milestone.developer-edition-0-1` checks `declared-package-image`,
   `disposable-provisioning`, `coherent-build-sets`, `reference-hardware` and
   `userspace-and-boot-recovery` carry evidence. The remaining two (`one-shared-index`,
   `daily-driver-day`) are earned by living in it.

Out of scope, on purpose: a graphical installer, dual-boot UI, TPM/LUKS, arm64/BIOS, a public
archive, a LegeOS kernel. The installer ADR that `installer/README.md` defers (Calamares vs
subiquity) is answered for this edition by **"the live session runs the provisioner"**; record it as
an ADR when P3 lands, and revise `@legeos.decision.mkosi-images` ("ISO is a later stage") to say
the ISO is now the medium and mkosi stays the VM image.

## 3. The ISO route, decided

**mmdebstrap rootfs → casper live layer → squashfs → xorriso ISO with Canonical-signed shim+GRUB.**

Why this and not the alternatives the roadmap left open:

- *mkosi*: no ISO output (its own README says so). Keep it for the VM disk image; do not bend it.
- *live-build*: Debian's tool; Ubuntu's flavours use `livecd-rootfs`, which is Launchpad-shaped and
  heavy. Both would introduce a second package-list mechanism, which the no-independent-lists rule
  forbids.
- *ubuntu-image classic*: its docs say ISO/installer output is future work.
- *This route* reuses the exact bootstrap the provisioner already uses (`mmdebstrap`), the seed
  generator that already exists (`cargo xtask seeds`), and Ubuntu's own live-boot (`casper`). The
  script is the recipe; the inputs are the declarations. Roughly 150 lines of bash, one file.

Secure Boot on the live medium: `/EFI/BOOT/BOOTX64.EFI` = `shimx64.efi` from `shim-signed`,
`grubx64.efi` from `grub-efi-amd64-signed`, both **copied out of the resolute rootfs** (never the
host's), `grub.cfg` beside them (signed GRUB reads an unsigned config; it refuses unsigned
*modules* and unsigned *kernels*, and Ubuntu's kernel is signed). This is exactly how Ubuntu's own
ISOs boot. Risk: if resolute's signed GRUB lacks a module we need at boot, the first QEMU-SB boot
says so in P3.2 and the fallback is "SB off for the live medium only", recorded as a finding, never
silently.

Where the installed system gets its LegeOS packages: the ISO carries the build set as a flat signed
archive at `/legeos/devrepo/` (the same layout `lege dev repo` publishes). The provisioner points
apt at it as `file://` during install and **copies it to `/var/lib/legeos/devrepo` on the target**,
so the first boot already has `current` = the set it was installed from and `lege dev status`
converges. No network is needed to install; network is needed for the Ubuntu base unless P0.3 also
caches a pool (optional; see P0.3).

## Status (2026-09-07, evening)

Since the 2026-09-05 entry (P0.3, P1.1–P1.4, P2.1–P2.4's plan half, P3.1 and P3.2/P3.3's harness in
the tree, gates green, nothing executed) the medium has been **built for the first time**, and the
attempt found what the plan could not:

- **Native packages have to be built in resolute.** The first bootstrap refused the set published
  from this host: `legeos-comp : Depends: libdisplay-info2 (>= 0.2.0) but it is not installable` —
  resolute ships `libdisplay-info3`. A package built on Ubuntu 25.10 carries 25.10's shared-library
  dependencies, so `scripts/build-in-resolute.sh` (mmdebstrap chroot, unprivileged, no root) is now
  the route to a set the medium can install, and P1's exit condition means a set built *there*.
  That script needed `libclang-dev` and `libpam0g-dev` as well: the shell's PAM binding runs bindgen
  at build time, which a developer host happens to have and a fresh chroot does not.
- **Hardware safety, ahead of P4.** `scripts/hardware-inventory.sh` records what the reference
  laptop loads; `scripts/hardware-closure.sh` joins that to the package providing each module and
  firmware file, against the pinned snapshot's `Contents-amd64.gz` (one fetch, then offline).
  Result in `docs/measurements/hardware-inventory-dk-82y3-20260907.md`: **no firmware in use is
  unprovided**, and the three loaded modules with no resolute package are two renames and one
  out-of-tree DKMS module. 26.04 has no `linux-modules-extra`; it splits `linux-firmware` per
  vendor and lifts Intel Wi-Fi into `linux-modules-iwlwifi-generic`, which the `hardware` layer now
  declares along with the four packages (`firmware-sof-signed`, `alsa-ucm-conf`, `wpasupplicant`,
  `thermald`) that only a Recommends would otherwise pull in.
- **§3/P3 gains a preflight and two rescue entries.** `lege doctor --preflight` answers, from
  `/sys` and `/proc` alone, whether there is a DRM device with a driver, a render node, a network
  interface with a driver, a registered radio, a sound card and input devices;
  `legeos-preflight.service` runs it once on the live medium, leaves the report at
  `/run/legeos/preflight.txt` and prints it to the console, and `legeos-install` refuses to write
  when that report contains a failure (`--ignore-preflight` overrides). The medium's GRUB menu now
  carries **safe graphics (no NVIDIA)** — which blacklists the NVIDIA stack rather than passing
  `nomodeset`, since `nomodeset` would disable the Intel modesetting the session needs — and
  **text console (no session)** (`systemd.unit=multi-user.target nomodeset`).
  `tests/integration/iso-qemu.sh` now also asserts a passing `preflight-network` line.

Still not run anywhere: the OVMF boot of a finished ISO, an `--apply` onto real hardware, and P4 in
full. Still waiting on: the nm-tray-2 push (P0.5) and P0.4 — `../pixelkit` has no git remote at
all, `../bpg-rs` is 5 commits ahead of origin and `../Lege-ecosystem` 41, so nothing can be pinned
until those are pushed. P0.2's build volume is not needed as stated: `/var/tmp` on the root
filesystem has the room, and `/mnt/Samsung980_1TB` stays out of it because it is NTFS.

## 4. Phases

Ordered by dependency. Time is a guess at effort, not a promise. "User" marks steps only the owner
of the laptop can do; everything else an agent can do in this tree without touching the live session,
the bootloader, or a real disk (the standing rule).

### P0 — Unblock the host and the tree (half a day)

- **P0.1 Tools.** `sudo apt install mmdebstrap squashfs-tools xorriso mtools dosfstools qemu-system-x86 ovmf`.
  `casper`, `shim-signed`, `grub-efi-amd64-signed` are installed *into the rootfs* from resolute, not
  on the host. *(User: needs sudo.)*
- **P0.2 A build volume that is not NTFS and not the full root.** A 30 GiB ext4 loop image on the
  Samsung volume: `truncate -s 30G /mnt/Samsung980_1TB/legeos-build.img && mkfs.ext4 …`, mounted at
  `/var/tmp/legeos-build`. NTFS cannot hold a rootfs (ownership, xattrs, device nodes) but can hold
  the file. Budget inside it: rootfs 5 GiB, squashfs 2 GiB, ISO 2.5 GiB, QEMU fixture disk 12 GiB
  (sparse), archive 1 GiB. *(User: mount needs sudo; an `/etc/fstab` line makes it survive reboots.)*
- **P0.3 Resolve the closure for the first time.** Point `cargo xtask seeds closure` at a resolute
  index: `mmdebstrap --mode=unshare --variant=apt resolute` into the build volume gives
  `/var/lib/apt/lists` for `main universe restricted`; run the closure against it. Every
  `unavailable` name is a fix in `selection.toml` (rename, drop, or note the `@NVIDIA_BRANCH@` value:
  resolute's LRM is `linux-modules-nvidia-580-open-generic`). Set `snapshot_url` in
  `distro/base.toml` to the `snapshot.ubuntu.com` serial the index came from, so the next build
  resolves the same way. Evidence closes `closure-report`.
- **P0.4 Pin the sibling deps (B6).** Convert the 5 workspace path deps and the 4 preview-codec ones
  to `git = …, rev = …` in their own commit; prove `cargo build` in a checkout with no siblings
  present. Nothing later works without this, and it is mechanical, so it goes first. The preview
  codecs' lockfile drift note in CLAUDE.md applies.
- **P0.5 nm-tray (B3). *User:*** commit and push the `--popup` / forwarding-slot / `X-LegeOS-*`
  work in `../not-rust-projects/nm-tray-2` to `LegeApp/nm-tray-alt`, update the `pin` in
  `distro/forks/manifest.toml`, and rename the branch to something typeable (the current one has
  U+2011 hyphens). Until then the package cannot be built and the desktop's network control is a
  no-op tile.

Exit: closure report shows zero unavailable names against a named snapshot; workspace builds
without siblings; `scripts/build-fork-deb.sh` produces `legeos-nm-tray` without `preflight.sh`
refusing.

### P1 — The complete package set (a day)

- **P1.1 Build the metapackages (B2).** An `xtask metapackages` subcommand (or a target in
  `debian/rules`) that turns each `distro/metapackages/*/control.in` into an `all` `.deb` with
  `dpkg-deb`, versioned from the BuildSetId like everything else. `just deb` publishes 13 + 8 + 1
  (nm-tray) = 22 packages as **one** set. `check-packaging` learns that a metapackage's rendered
  `Depends` must resolve either in the set or in the P0.3 closure.
- **P1.2 Package the provisioner.** `installer/provision.sh` becomes `/usr/libexec/legeos/provision`
  inside a `legeos-installer` `.deb` (with `installer/CHECKLIST.md` and `distro/deploy/grub.d/09_legeos`
  as its data), plus a `legeos-install` wrapper on `PATH`. "Nothing under /usr/local; every file is
  owned by a package" applies to the live medium too, and the live session has no checkout.
- **P1.3 Repo verify as a user (papercut).** `lege dev repo verify` must say "cannot read the
  keyring as this user" instead of "FAILED". The live session runs it before installing and a false
  tamper report there would stop an install for nothing.
- **P1.4 A `live` layer.** In `selection.toml`: `kind = "live"`, `image = false`, packages
  `casper`, `legeos-installer`, `efibootmgr`, `gdisk`, `mtools`, plus the live-only credentials
  drop-in (autologin of a `legeos` live user, the same posture as `90-dev-credentials.conf` and
  labelled the same way: **live medium only**; the installed system gets none of it). The
  generator emits `distro/seeds/live.seed` and refuses to render a `live`-kind package into any
  metapackage or the mkosi list.

Exit: `lege dev repo verify --set <id>` reports one coherent set containing every component in
`components.toml` plus the 8 metapackages plus `legeos-nm-tray` plus `legeos-installer`. Closes
`metapackages-built`, `nm-tray-2-packaged`, `coherent-build-sets`.

### P2 — A provisioner that fits this laptop (a day)

- **P2.1 `--free-space DEVICE` and `--esp PART --root PART` (B4).** Free-space mode reads the GPT,
  finds the largest unallocated run at the tail, plans `ESP 1 GiB + root (rest)`, and creates
  *only those two partitions* (`sgdisk -n 0:0:+1G -t 0:ef00`, `sgdisk -n 0:0:0 -t 0:8304`). The
  explicit-partition mode formats two existing partitions and touches the table not at all. Both
  refuse if any named partition is mounted, is inside a mounted filesystem, or if the free run is
  under `root.size_min`. `--device --i-mean-it` (zap) stays as it is. The plan output names the
  partitions it will create, e.g. `nvme1n1p3`, `nvme1n1p4`, and every existing one it will not touch.
- **P2.2 A second ESP is normal.** The provisioner installs shim+GRUB into *its* ESP and registers
  one UEFI entry with `efibootmgr --create --disk … --part … --loader '\EFI\legeos\shimx64.efi'
  --label LegeOS`, appended to the boot order, **not** first. The firmware menu is how the first boots
  happen; `efibootmgr -o` becomes a user step after the checklist passes. Refuse to write NVRAM in
  QEMU-less dry run; print the command.
- **P2.3 Archive handoff.** `--archive DIR` (default: `/legeos/devrepo` when running from the live
  medium, `/var/lib/legeos/devrepo` otherwise). Copy the named set to the target's
  `/var/lib/legeos/devrepo` and write its `current` marker. `lege doctor --root` on the target must
  report the set as installed.
- **P2.4 QEMU fixture.** `tests/integration/provision-qemu.sh` gains a fixture disk built with
  `sgdisk`: 16 MiB `msftres`, a 6 GiB NTFS-typed partition (no filesystem needed), a 100 MiB ESP
  with junk, then 12 GiB free — the shape of the laptop, small. Assert: the plan names exactly two
  new partitions; after `--apply` the three pre-existing partitions have identical `sgdisk -i` output;
  the target boots under OVMF and `/proc/cmdline` carries `legeos.deployment=install-…`.

Exit: `provision-qemu.sh` passes in both modes. Closes `provisioner-takes-free-space`,
`disposable-provisioning`, `grub-entries-carry-id` (already) and gives `boots-with-secure-boot`
its VM half.

### P3 — The ISO (a day and a half)

- **P3.1 `distro/iso/build.sh`.** Inputs: `selection.toml` (via `cargo xtask seeds --live`),
  `base.toml` (mirror, snapshot, components), a build-set id. Steps, each idempotent under
  `--resume`:
  1. `mmdebstrap --mode=unshare --variant=minbase --include=$(seeds: bootstrap+hardware+graphics+session+desktop+browser+media+dev-rust+live) resolute rootfs/ <mirror-or-snapshot> <file://devrepo>`;
     with the LegeOS keyring pre-trusted via `--setup-hook`. Recommends follow `policy.recommends`.
  2. Post-hooks: `os-release` from `distro/branding`, the greeter/session enablement `mkosi.postinst`
     already performs (factor it into `distro/image/postinst.sh` and have both callers source it —
     one copy), live autologin drop-in, `update-initramfs -c` with casper hooks.
  3. `mksquashfs rootfs/ iso/casper/filesystem.squashfs -comp zstd`; copy `vmlinuz` and `initrd` out
     of the rootfs to `iso/casper/`; write `filesystem.manifest` and a `TIERS.manifest`
     (package, version, tier ∈ inherited/rebuilt/native, from the archive membership).
  4. Copy the set: `iso/legeos/devrepo/` = the `lege dev repo` layout for that one set.
  5. Boot files: `iso/EFI/BOOT/BOOTX64.EFI` = shim, `grubx64.efi`, `mmx64.efi`, and `grub.cfg`
     (`search --set=root --file /.disk/info`, `linux /casper/vmlinuz boot=casper quiet ---`,
     a second entry `… toram`, a third for "check medium"); build `efi.img` with `mformat`/`mcopy`.
  6. `xorriso -as mkisofs -r -V "LegeOS <set>" -J -joliet-long -e boot/grub/efi.img -no-emul-boot
     -isohybrid-gpt-basdat -o legeos-<set>.iso iso/`; plus `sha256sum` and the two manifests beside it.
  `just iso` runs it inside the P0.2 volume. No hybrid MBR/BIOS path; UEFI only, as everywhere else.
- **P3.2 Boot it under Secure-Boot OVMF.** `qemu-system-x86_64 -machine q35,smm=on -drive
  if=pflash,…OVMF_CODE_4M.secboot.fd … -cdrom legeos-<set>.iso` with the MS-keys-enrolled
  `OVMF_VARS` (`ovmf` ships one). Gate: reaches the greeter/LegeOS session, `mokutil --sb-state`
  in the live session says enabled, `dmesg` shows the shim → GRUB → kernel chain, no MOK prompt.
  This is the step that turns the §3 risk into a fact.
- **P3.3 End to end in QEMU.** Attach the P2.4 fixture disk; from the live session's kitty run
  `sudo legeos-install --free-space /dev/vda --user dev`, read the plan, `--apply`; reboot without the
  ISO; assert the P2.4 conditions plus `lege dev status` = converged and `nmcli device` lists the
  virtio NIC. Automate the whole thing as `tests/integration/iso-qemu.sh` (serial console + expect
  is enough; no screenshots). This test is the one that must stay green from now on.
- **P3.4 Record.** Write the ADR (live-session-runs-the-provisioner; ISO is the medium; mkosi stays
  the VM image); replace `distro/iso/README.md`; revise `@legeos.decision.mkosi-images`.

Exit: `iso-qemu.sh` green under SB firmware. Closes `declared-package-image`, `reinstall-route-exists`,
and the VM halves of `userspace-and-boot-recovery`.

### P4 — The laptop (an evening; all *User*)

`installer/CHECKLIST.md` stays the authority; this is the order.

1. `dd` the ISO to a USB stick (or Ventoy; the ISO is a plain UEFI ISO). Back up the table first:
   `sudo sgdisk --backup=nvme1n1.gpt /dev/nvme1n1` to somewhere that is not this machine.
2. Boot the stick from the firmware menu with Secure Boot **on**. Stop conditions, in order: a MOK
   or "Security Violation" prompt (record the exact message; something is not Canonical-signed);
   casper drops to `(initramfs)` (record `/casper.log`); the session does not start (record
   `journalctl -b`). Each is a bug for P3, fixed in QEMU, not worked around here.
3. In the live session: `nmcli device wifi list` — this is where B10's "does the radio exist under
   Ubuntu's kernel and firmware" is answered, before anything is written to disk.
4. `sudo legeos-install --free-space /dev/nvme1n1 --user dk`. Read the plan: it must name
   `nvme1n1p3` (1 GiB) and `nvme1n1p4` (~54.9 GiB) and nothing else. Then `--apply`.
5. Reboot, pick **LegeOS** from the firmware menu (not yet the default). Checklist §3 → §4 → §5:
   `cat /proc/cmdline` contains `legeos.deployment=install-…`; `mokutil --sb-state`; `lsmod | grep
   nvidia` + `modinfo -F signer nvidia` says Canonical, not a MOK key; `lege doctor --hardware`;
   WiFi via `nmcli` and then via the Home network widget; sound; an external display if handy.
6. Checklist §6: `sudo lege dev rollback`, reboot, `lege dev status`, deploy forward again. Rollback
   that has not been exercised is not rollback. Then and only then `efibootmgr -o` with LegeOS first,
   Lubuntu second.

Closes `boots-with-secure-boot`, `wifi-works`, `rollback-exercised`, `reference-hardware`.

### P5 — Develop LegeOS from inside LegeOS (a day)

- **P5.1 Room.** 54 GiB holds the OS and the checkout, not a 42 GiB `target/`. `CARGO_TARGET_DIR`
  on the NTFS projects volume (mounted with `uid=`, `fmask=`) is the quick answer; watch for
  case-sensitivity and permission surprises and stop if they appear. The btrfs root already mounts
  `compress=zstd:1`. Permanent room comes from retiring Lubuntu (self-hosting plan §7), later.
- **P5.2 Toolchain.** The `dev-rust` layer must actually yield a working `cargo +1.97.1` (rustup or
  Ubuntu's rust; decide by what `just msrv` needs; rustup is the honest default for a pinned
  toolchain). `dev-cuda` stays optional and off the image; install it by metapackage when wanted —
  CUDA 13.x is in resolute's archive, so it is inherited, not fetched from NVIDIA.
- **P5.3 The proof.** Inside LegeOS, with no other computer involved: `just check` → `just deb` →
  `sudo lege dev deploy` → logout/login → `just iso`. If the ISO LegeOS builds boots in QEMU inside
  LegeOS, the system is self-hosting and the medium is reproducible from the thing it produced.
  Closes `self-hosts`. Keep the P4 stick as the rescue medium until a second one is written from the
  inside build.

### P6 — Live in it, then retire Lubuntu

Self-hosting plan §7 verbatim: seven days as the only desktop, WiFi survives suspend, a kernel update
booted with the previous kernel still present, rollback exercised, the stick exists, backups exist.
Then delete `nvme0n1p3`, `btrfs device add`, balance. This is where `daily-driver-day` and
`one-shared-index` earn their evidence, by use rather than by a run.

## 5. The Rebuilt tier: pipeline shape now, packages only when justified

This is the advisor's Phase 2 and the one piece of infrastructure worth building before it is needed,
because it is what makes "rebuild one Ubuntu package" an afternoon instead of a research project. It
is **P7, after P5**: nothing about booting depends on it, and it should be built from inside the OS.

- `distro/sources/TIERS.toml`: one stanza per non-inherited package: `package`, `tier`, `reason`
  (the one-line answer to the rule), `upstream_version`, `patches = [...]`. `check-packaging` fails
  if a package in the archive has no stanza or a stanza has no reason.
- `lege source import <pkg>`: `apt-get source` from the pinned snapshot into
  `distro/sources/<pkg>/{upstream,debian}`, an empty `legeos/patches/series`, and a `TIERS.toml`
  stanza to fill in. Rejects an import with an empty `reason`.
- `lege source build <pkg>`: `sbuild`-shaped but implemented with `mmdebstrap --mode=unshare`
  (already a dependency): fresh resolute chroot, apply `legeos/patches`, append `+legeos1` to the
  Debian revision (`260.1-1ubuntu1` → `260.1-1ubuntu1+legeos1`, which sorts above Ubuntu's and below
  Ubuntu's next revision, so a Canonical security update *does* supersede ours until re-imported,
  which is the correct default), `dpkg-buildpackage -us -uc -b`, record source hash, chroot snapshot
  serial, build log.
- `lege dev repo publish` learns to take those `.deb`s into the same set as the native ones; the
  ISO's `TIERS.manifest` then says `rebuilt` for them.
- `lege source refresh <pkg>`: re-import the newest Ubuntu source, reapply the queue, stop on the
  first patch that does not apply. That loop is the whole maintenance cost of the tier.
- Smoke: a zero-patch rebuild of one small leaf package, published and installed, to prove the
  versioning and the publish path. **First real candidate: none today.** Plausible early ones, each
  still needing its one-line reason when it arrives: `casper` (if the live layer needs a hook Ubuntu's
  does not carry), `xdg-desktop-portal` (only if the LegeOS backend needs a frontend change; today it
  does not), `plymouth` theme (that is a new *native* package, not a rebuild), the kernel (only via the
  ADR-0007 candidate channel, never for this edition).

Per-CPU optimisation (`x86-64-v3`, PGO) is benchmark-gated and lives in the same pipeline as a
build-flag stanza; it is not started until there is a measurement that says which package.

## 6. Ownership, if this is delegated

| Lane | Phases | Writable | Must not touch |
|---|---|---|---|
| Closure & seeds | P0.3, P1.4 | `distro/seeds/**`, `distro/base.toml`, `tools/xtask/src/seeds*` | installer, hostd |
| Packages | P0.4, P1.1–P1.3 | `Cargo.toml` deps, `debian/**`, `distro/metapackages/**`, `cli/lege/src/dev/repo.rs`, `tools/xtask` metapackage module | seeds, installer |
| Provisioner | P2 | `installer/**`, `tests/integration/provision-qemu.sh` | `services/lege-hostd/**` (C3 changes go through `layout.toml` + hostd's owner) |
| ISO | P3 | `distro/iso/**`, `distro/image/postinst.sh` (factored), `tests/integration/iso-qemu.sh`, `justfile` `iso` recipe | seeds (requests to the closure lane), installer internals |
| Rebuild pipeline | P7 | `distro/sources/**`, `cli/lege/src/source/**` | anything above |
| User | P0.1, P0.2, P0.5, P4, P5, P6 | the laptop | — |

Same rules as every campaign here: one shared tree, disjoint paths, `You are a subagent. Don't run
memo.`, no live-session or real-disk actions by agents, `knowledge.context` before and
`knowledge.validate` after.

## 7. Ledger actions this plan implies (do before P0 code)

- Revise `@legeos.work.self-hosting-legeos` to route through the live ISO; add checks
  `iso-builds`, `iso-boots-secure-boot-qemu`, `iso-installs-into-free-space-qemu`, `iso-rebuilt-inside`.
- Propose `@legeos.decision.live-iso-runs-the-provisioner` (the installer ADR) and revise
  `@legeos.decision.mkosi-images`'s "ISO is a later stage".
- Propose `@legeos.decision.three-package-tiers` recording §0 with the rule, and
  `@legeos.work.source-rebuild-pipeline` for §5, `proposed` and explicitly *after* P5.
- Attach P0.3's closure report as evidence to `closure-report` with the snapshot serial as
  `observed_at` provenance.

## 8. Risks, in the order they would bite

1. **Resolute names that do not resolve** (B1). Every one is a seeds edit; the cost is bounded by
   103 names. Do P0.3 before anything else so the number is known on day one.
2. **Signed GRUB on the live medium.** Proven or disproven in P3.2 within an hour. Fallback: SB off
   for the live boot only, installed system unchanged; record it.
3. **Radio or firmware missing under Ubuntu's 7.0 kernel** on this exact laptop. Found at P4.3 before
   any write. Fix is a `hardware` layer package (firmware), inherited tier.
4. **nm-tray commits never pushed** (B3). Nothing else waits on it, but the desktop's network widget
   is dead until it happens; P4.5 tests `nmcli` first for exactly this reason.
5. **The 100 MiB ESP temptation.** Do not shrink `nvme0n1p3`; the plan uses a second ESP. Two ESPs
   are normal and the firmware menu shows both.
6. **Disk on the build host.** Root at 99% will fail a `cargo build` before it fails an ISO build.
   P0.2's loop volume is for the ISO; `target/` still needs its own room on the host until P5.
