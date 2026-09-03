# From here to a LegeOS you can develop LegeOS on

The goal in one sentence: a LegeOS that boots on this machine, joins WiFi, and builds its own
source tree — after which Lubuntu is deletable rather than load-bearing.

This is not a restatement of `docs/distroagentplan.md` (what the distribution is) or
`docs/distroagentmidplan.md` (how the Developer Edition 0.1 lanes were delegated). Those describe
the product. This describes the next fifteen or so hours of work on **this laptop**, in the order
they have to happen, with the facts about the machine that decide the order. Everything under
"Verified" below was measured on the host on 2026-09-04; everything else is marked as unknown.

Ledger: `@legeos.milestone.developer-edition-0-1` governs the packaging and boot work.
The hands-on findings this came out of are the fifth run's.

---

## 1. Verified: what already exists

| | |
|---|---|
| Package declaration | `distro/seeds/selection.toml`, 14 layers, generates every seed, image list, metapackage `control.in` and the Flatpak ref list |
| Provisioner | `installer/provision.sh` — plans by default, writes only on `--apply`; bootstraps an Ubuntu `resolute` base with mmdebstrap or debootstrap |
| Layout declaration | `distro/deploy/layout.toml` — GPT, ESP 512M–1G, btrfs root labelled `legeos-root`, subvolumes `@ @home @var @snapshots @deployments` |
| Boot | ADR-0004 accepted: Canonical shim → Canonical GRUB → Ubuntu kernel → Canonical LRM. `services/lege-hostd/src/boot_entries.rs` writes `/boot/grub/legeos-entries.cfg`, every entry carrying `legeos.deployment=<id>` |
| Rollback | Deployment registry persists current/previous/known-good; retention never drops the known-good or booted deployment; a Btrfs root snapshot precedes a userspace install |
| Packages built | 13 `.deb`s in `target/debian/`, build set `0.1.0~dev1788413867+g6b354c5.dirty` |
| Hardware | `lege doctor --hardware` executes bounded read-only probes with four honest outcomes |
| Bare-metal checklist | `installer/CHECKLIST.md`, 6 sections, none ticked |

## 2. Verified: the four things that stop it today

**2.1 There is nowhere to install to, except one place.** Both NVMe drives are nearly full, but
`nvme1n1` has unallocated space at its tail:

```
nvme0n1  953.9 GiB   p1 NTFS 536G · p2 NTFS 949M · p3 ext4 416G (Lubuntu /, 8.7G free)
                     p4 vfat 100M (the ESP) · p5 16M          free tail: 0.5 GiB
nvme1n1  931.5 GiB   p1 16M · p2 NTFS 875.6G (the projects drive, 86G free)
                                                               free tail: 55.9 GiB
```

55.9 GiB is enough for a 1 GiB ESP and a ~54 GiB btrfs root, and it touches neither Lubuntu nor
Windows. It is **not** enough to also hold a 42 GiB `target/` directory, which is why §6 exists.

**2.2 The existing ESP is 100 MiB and the layout needs 512 MiB.** A LegeOS deployment puts its
kernel and initramfs in `/EFI/legeos/<id>/`, and rollback means holding two of them at once. The
current ESP is `nvme0n1p4`, sandwiched between Lubuntu's root and a 16 MiB partition near the end
of the disk, so it cannot be grown without shrinking `nvme0n1p3`. Do not shrink it. Put a second
ESP in the free tail of `nvme1n1` — a machine may have as many as it has disks — and register it
with `efibootmgr`, leaving Lubuntu's entry as the fallback.

**2.3 The provisioner only takes a whole disk.** `installer/provision.sh` runs
`sgdisk --zap-all "$target"` and then creates partitions 1 and 2. There is no mode that consumes
free space on a disk that already has partitions, which is exactly what this machine needs.

**2.4 The workspace cannot be built from a clean checkout.** Five path dependencies point outside
the repository (`../pixelkit` ×4, `../bpg-rs`), plus the preview codecs' own siblings. A LegeOS
that cannot build LegeOS is not self-hosting, so this blocks §6, not §4.

Also missing, all trivially: `mmdebstrap` and `debootstrap` are not installed on this host, and
`nm-tray-2` is neither packaged nor named in the seed declaration.

## 3. The route

Install LegeOS into the free tail of `nvme1n1`, keeping Lubuntu untouched and bootable. Boot it,
prove WiFi and the desktop, then move development into it. Delete Lubuntu last, and only against
the checklist in §7 — at which point its 416 GiB becomes a second btrfs device for the LegeOS
root, which is how a 54 GiB installation becomes a development machine.

Lubuntu is the recovery path for every step until §7. Nothing here removes it.

## 4. Stage A — make it installable (about a day)

**A1. Teach the provisioner to use free space.** Add `--esp PART --root PART` (use these existing
partitions, format them, do not touch the table) and `--free-space DEVICE` (create the two
partitions in the tail, leave everything else alone). Refuse both unless the named partitions are
unmounted and outside every mounted filesystem. This is the one change that is genuinely required
rather than convenient: everything else in §5 can be done by hand once, but a provisioner that can
only zap a disk cannot be run on this machine at all.

Acceptance: `installer/provision.sh --free-space /dev/nvme1n1 --user dk` prints a plan naming
`nvme1n1p3` (1 GiB, ESP) and `nvme1n1p4` (54.9 GiB, root), and changes nothing without `--apply`.

**A2. Package nm-tray-2.** It is a fork (`github.com/LegeApp/nm-tray-alt`, upstream
`palinek/nm-tray`), and ADR-0005 says forks are pinned in `distro/forks/manifest.toml` and never
vendored. Add the stanza, produce `legeos-nm-tray`, and add it to the `session` layer of
`distro/seeds/selection.toml` beside `network-manager` with `role = "network-control"` so the
Home information row's network widget resolves to it on a fresh install. Today
`network_control_app` looks for `org.legeos.NetworkTray` and then for any
`*nm-tray.desktop`, so the package needs to install one of those two names and nothing else has
to change in the shell.

Acceptance: `cargo xtask seeds --check` clean, `cargo xtask check-packaging` clean, and the
rendered `legeos-desktop` control file depends on `legeos-nm-tray`.

**A3. Build the metapackages.** `just deb` builds the 13 Rust packages with cargo-deb; the eight
directories under `distro/metapackages/` are rendered `control.in` files that nothing turns into
`.deb`s yet. Add an xtask or a `debian/rules` target that builds them, and publish all 21 into the
local archive as one build set.

Acceptance: `lege dev repo verify` reports one coherent set containing every component
`distro/deploy/components.toml` names.

**A4. Install the two bootstrap tools**: `sudo apt install mmdebstrap` (debootstrap is the
fallback the script already handles).

## 5. Stage B — provision and boot (an evening, at the machine)

Do this with `installer/CHECKLIST.md` open; it is the authority and this is only the order.

1. `installer/provision.sh --free-space /dev/nvme1n1 --user dk` with no `--apply`, and read the
   plan. Confirm it names the two new partitions and nothing existing.
2. Back up the partition table first: `sudo sgdisk --backup=/mnt/.../nvme1n1.gpt /dev/nvme1n1`.
   This is the one command that makes 2.1 reversible.
3. Run it again with `--apply`. It bootstraps `resolute`, installs the LegeOS archive keyring and
   the metapackages, keeps the previous kernel, installs shim and GRUB into the new ESP, and
   stamps the first deployment id.
4. `sudo efibootmgr --create --disk /dev/nvme1n1 --part 3 --label LegeOS --loader
   '\EFI\LegeOS\shimx64.efi'`, then `efibootmgr --bootorder` to put Lubuntu first for now. Boot
   LegeOS from the firmware menu, not from the default, until it has proven itself.
5. Checklist §3 (chain of trust) before the first boot, then §4 (first boot with Secure Boot on).
   The one fact everything rests on: `cat /proc/cmdline` must contain `legeos.deployment=<id>` and
   the id must match a directory under the ESP. Without it hostd reports "cannot tell" and counts
   no failed boot attempts, and the whole rollback machinery is decorative.
6. WiFi: `nmcli device wifi list` then `nmcli device wifi connect <ssid> --ask` from a TTY. Only
   after that is the nm-tray-2 route worth testing — a GUI that cannot connect and a driver that
   cannot see the radio look identical from the desktop.
7. `lege doctor --hardware --json`. Expect the NVIDIA finding this host already reports to be
   *absent*: the module must be Canonical's LRM, not a locally signed DKMS build.

Stop conditions, in order of severity: a MOK enrolment prompt (something in the chain is not
Canonical-signed — stop and record what); no `legeos.deployment=` on the command line; no WiFi
device in `nmcli device`. Each of those is a bug to fix from Lubuntu, not to work around at the
machine.

## 6. Stage C — develop from inside it (a day)

**C1. Pin the sibling dependencies.** Convert the five path dependencies to git dependencies
pinned by commit, in their own commit, and confirm `cargo build` succeeds from a checkout with no
siblings present. Until this is done, a LegeOS install cannot rebuild LegeOS, and §7 is unsafe at
any speed.

**C2. Give the root room.** A 54 GiB root holds the system and a source tree but not a 42 GiB
`target/`. Two options, in order of preference:
  - Build with `CARGO_TARGET_DIR` on the NTFS projects partition mounted at `/mnt/projects`. Fast
    to arrange, and it is where the tree already lives; watch for permission and case-sensitivity
    surprises and stop if any appear.
  - Or keep `target/` on the btrfs root and lean on `compress=zstd:1`, which the layout already
    sets, plus `cargo clean -p` discipline. Workable, tight.
  Either way this is temporary: §7 ends with 416 GiB more.

**C3. Prove it self-hosts**: `just check` green *inside* LegeOS, then `just deb`, then
`sudo lege dev deploy` and a logout, on the machine itself with no other computer involved. That
is the definition of self-hosting and it is the acceptance for this stage.

## 7. Stage D — retire Lubuntu

Do not start this until every line is true. Each is a thing that has actually happened, not a
thing that ought to work.

- [ ] LegeOS has been the machine's only desktop for **seven consecutive days** of ordinary use.
- [ ] WiFi reconnects on its own after a suspend and after a reboot, on at least two networks.
- [ ] `just check` and `just deb` have both run green inside LegeOS (C3).
- [ ] A `lege dev deploy` has been rolled back — deliberately, at least once — and the machine came
      back. Rollback that has never been exercised is not rollback.
- [ ] A kernel update has been installed and booted, with the previous kernel still present.
- [ ] Sound, the browser, an external display and the trackpad have each been used in earnest.
- [ ] A LegeOS installation medium exists that does not depend on this disk. Today there is none:
      the provisioner runs from a working Linux userspace, so with Lubuntu gone there is no way to
      reinstall. A live USB of stock Ubuntu is the minimum; a LegeOS live image is the right answer
      and is not built yet.
- [ ] The GPT backup from B2 and a copy of `~` are somewhere that is not this machine.

Then, and only then:

1. `efibootmgr` — delete the Lubuntu entry, make LegeOS the default.
2. `sgdisk --delete=3 /dev/nvme0n1` (Lubuntu's root; leave the 100 MiB ESP alone for now — it
   costs nothing and it is a second place to put a rescue loader).
3. `sudo btrfs device add /dev/nvme0n1p3 /` then `sudo btrfs balance start /`. The root becomes a
   two-device filesystem of about 470 GiB, and `target/` can come home.

## 8. What this plan does not cover, and what is unknown

- **No live image.** `just image` needs mkosi, which is absent here, and no image has ever been
  built or booted. §7's last checkbox is the reason this matters; until then it is optional.
- **No package closure.** There is no Ubuntu 26.04 index on this host, so every package name in
  `distro/seeds/selection.toml` is unresolved against `resolute`. `resolute` *is* reachable
  (`archive.ubuntu.com` answers 200 for its `Release` file), so this is one `apt update` against a
  `resolute` sources list in a container away, not a research problem — but it is unknown today and
  the provisioner will be the first thing to find out.
- **Nothing here has been executed.** No target provisioned, no QEMU boot, no clean-root build.
  Every step above is written from the code and from the machine's partition table, not from a run.
- **Timings are guesses.** "About a day" means the work looks like a day's work, not that anyone
  has done it.
