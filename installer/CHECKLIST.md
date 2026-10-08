# Bare-metal checklist — run by a person, at the machine

Everything here needs a real UEFI machine with Secure Boot, a real NVIDIA card and a real reboot.
No agent, test or CI job has executed any of it; treat an unticked box as unknown, not as passing.
Take it in order: each section assumes the one above it held.

Print or keep a second device handy — steps 4 and 5 involve rebooting the machine you are reading
this on. Have a live USB ready before you start, and read §6 before you need it.

The reference laptop installs onto the OS drive: into its free block the first time, or onto the
TemplarOS pair (partition names `TEMPLAR_ESP` and `templaros-root`) an earlier attempt created.
`sudo templaros-autoinstall` finds that drive by its `lubuntu_2504` label — NVMe numbers have
swapped between boots, so no device name in an old note is authoritative — and never writes the
drive labelled `Second`. `--device --i-mean-it` destroys a whole disk and is not this checklist.

## 1. Before provisioning

- [ ] Run `lsblk -o NAME,MODEL,SERIAL,SIZE,RO,FSTYPE,LABEL,PARTLABEL,MOUNTPOINTS` and `sudo blkid`.
      Record both disks' model, serial and current device path before making GPT backups.
- [ ] The USB stick is the TemplarOS live image (`TEMPLAROS_LIVE`), written with `dd` of the
      whole ISO — not a FAT32 copy. The image is over 4 GB. On this host `dd` is uutils, so no
      `oflag=direct`.
- [ ] `lsblk` shows `lubuntu_2504` and `Second` on two different disks. The one with
      `lubuntu_2504` is the one the install writes; the other is out of scope.
- [ ] Firmware is in UEFI mode, not CSM/legacy. There is no BIOS boot path.
- [ ] Secure Boot state noted: `mokutil --sb-state` → `SecureBoot enabled` / `disabled`.
      Write down which, so §4 compares against something.
- [ ] The USB-C ethernet adapter is plugged in and `nmcli device` shows it `connected`.

## 2. Provision

There is no graphical installer on the medium. The first GRUB entry is a live session, the
second copies it into RAM, and the last installs unattended. From a session, installing is one
command in a terminal: `sudo templaros-autoinstall`.

If the live Home cannot open Files, Settings, a terminal, or Shut down, sessiond did not
start (casper user, missing `~/.config/templaros`). Do not hold the power button. Reboot
and pick **TemplarOS — text console (no session)** (fourth GRUB entry).

On the ISO that is on the stick today, that entry still has `console=ttyS0` and
`casper-md5check`. The checksum failure is not a hang — boot already reached
multi-user.target — but there is no getty on the panel. At GRUB highlight the
fourth entry, press `e`, and on the `linux` line:

- delete `console=ttyS0,115200 console=tty0`
- type `console=tty1 systemd.mask=casper-md5check.service`
- Ctrl+X to boot

Log in as `templaros` with an empty password.

From the live session, after `/run/templaros/preflight.txt` is readable and does not report a
hardware failure that would strand the machine:

- [ ] `sudo templaros-autoinstall` (or the medium's last GRUB entry) ended with
      `autoinstall: DONE`. It prints the plan it applies: on a first install that is new
      partitions 5 (TEMPLAR_ESP) and 6 (templaros-root) with the four existing OS-drive partitions
      kept, and `OTHER PHYSICAL DISKS` showing the `Second` disk as `WILL NOT BE TOUCHED`; on a
      second attempt it names the pair and whether it is rebuilding or resuming it.
      If it stopped, fix what it names and run the same command again.
- [ ] `templar doctor --root <mount>` at the end reported no fault it called fatal.
- [ ] Reboot without the stick and choose TemplarOS from the firmware menu. The installer
      appends its UEFI entry rather than making it first. Lubuntu stays bootable.

## 3. Chain of trust — check before the first boot

With the target still mounted, or from a live USB afterwards:

- [ ] The ESP holds Canonical's shim, not a self-signed one:
      `sha256sum <esp>/EFI/TemplarOS/shimx64.efi` matches the one in the installed `shim-signed`
      package (`dpkg -L shim-signed`).
- [ ] GRUB is the signed build: `grubx64.efi` came from `grub-efi-amd64-signed`, and
      `dpkg -V grub-efi-amd64-signed shim-signed` reports nothing.
- [ ] The kernel is Ubuntu's: `dpkg -S <root>/boot/vmlinuz-*` names a `linux-image-*` package from
      Ubuntu, not a locally built file.
- [ ] The NVIDIA modules are Canonical's LRM, not DKMS:
      `dpkg -l 'linux-modules-nvidia-*'` shows an installed package, and
      `dpkg -l 'nvidia-dkms-*'` shows none.
- [ ] `<root>/etc/apt/apt.conf.d/01templaros-keep-kernels` exists (the previous kernel will survive).

## 4. First boot, with Secure Boot ON

- [ ] Enable Secure Boot in firmware if §1 said it was off, then boot the target.
- [ ] It boots to the greeter with **no MOK enrolment prompt**. A blue MOK screen means something
      in the chain is not Canonical-signed — stop and record what.
- [ ] `mokutil --sb-state` → `SecureBoot enabled`.
- [ ] `cat /proc/cmdline` contains `templaros.deployment=<id>` and the id matches the directory under
      `/boot/efi/EFI/templaros/`. This is the one fact the whole rollback machinery rests on: without
      it hostd reports "cannot tell" and counts no failed boot attempts.
- [ ] `lsmod | grep nvidia` is non-empty and `dmesg | grep -i 'module verification failed'` is
      empty — the modules loaded *and* the kernel trusted their signature.
- [ ] `nvidia-smi` prints the card. If it does not, `templar doctor --hardware` before anything else.
- [ ] The account created in §2 authenticates with its chosen password without autologin
      or forced-expiry lockout; use `templar account password` for paired changes.

## 5. The previous kernel is still selectable

- [ ] The GRUB menu (hold `Shift`, or `Esc` on some firmware) lists the TemplarOS deployment entries
      first, and Ubuntu's own entries with the previous kernel underneath.
- [ ] Boot the previous kernel once from that menu. It reaches a shell or the greeter.
- [ ] Back on the current kernel: `templar dev status` reports the same deployment identities as
      before that excursion.

## 6. Rollback — practise it before you need it

- [ ] `templar update status` (or `templar dev status --deployments`) names the current, previous and
      known-good deployments, and they are not all the same id.
- [ ] Stage something harmless and let it fail its health gate, or reboot into the candidate and
      power-cycle it before `templar-boot-health` completes, three times. The machine must come back
      on the known-good deployment by itself.
- [ ] After the automatic rollback, `/proc/cmdline` names the known-good id, and
      `/boot/grub/templaros-entries.cfg` no longer contains the abandoned candidate.

### If the machine does not come back

1. **The GRUB menu still has the known-good entry.** Pick it by name — the menu entry id *is* the
   deployment id. This is the first thing to try and it needs no other media.
2. **GRUB drops to a shell** (`grub>`): `set prefix=(hd0,gpt2)/boot/grub`, `insmod normal`,
   `normal`. If `templaros-entries.cfg` is unreadable, Ubuntu's own `10_linux` entries are still in
   `grub.cfg` and will boot the last kernel apt installed.
3. **No GRUB at all**: boot a live USB and see the recovery-shell procedure in
   `distro/deploy/README.md`, which covers mounting the subvolumes, chrooting, and restoring the
   entries file or the bootloader.
4. Record what you saw. A boot failure that is not written down is a boot failure that gets
   rediscovered.

## 7. Catch the installed OS up with the tree

The live ISO is a snapshot (the medium on the SanDisk Ultra is
`templaros-0.1.0~dev1788945442+gc2403fa.dirty`, 2026-09-09). Native packages newer than that
are installed from the checkout *after* first boot, not by rewriting the USB.

- [ ] The data drive (identified by label `Second`, never by an assumed NVMe number) is mounted so the checkout and its sibling
      crates are visible.
- [ ] `scripts/update-templaros.sh --check-host` exits 0 (ID=templaros, not casper).
- [ ] `scripts/update-templaros.sh` builds the native set and deploys it through
      `templar dev deploy`. It refuses to run on Lubuntu — that host uses
      `scripts/reinstall-templaros-desktop.sh`.
- [ ] File manager (`pcmanfm-qt`), terminal (`kitty`), pointer (libinput via the compositor)
      and NVIDIA (`nvidia-smi`, modules loaded, no `module verification failed`) all work
      after the first login. NVIDIA is installed by the provisioner into the target, not
      carried on the live medium.
- [ ] Log out to activate a new compositor; reboot only if this deploy included kernel or
      driver packages (this script does not).

## What to report back

For each unticked box: the step, the exact command, its output, and whether the machine was usable
afterwards. "It worked" without the commands is not evidence anyone can act on later.

## 8. Native ownership and opt-in read-only acceptance

These are **pending owner proofs**, not agent/test success. First use an owned
disposable raw image/loop device and private mount namespace; then repeat actual
boot trials on the target. Do not run these against a mounted/live desktop root
or an unrelated disk. Keep the image/device identity, exact commands, output,
root/ESP UUIDs and failure phase with the evidence.

- [ ] `just check-ownership` resolves every pinned exception to fresh active
      normative authority. Proposed source-candidates are not release approval.
- [ ] Provision and resume a disposable default-RW target, then a separately
      reviewed `--read-only` target. Inspect actual Btrfs subvolumes/default,
      fstab and mounts: `@var` is the narrow State store, never whole `/var`;
      dpkg/APT DB/cache have the selected root's own mount identity.
- [ ] Image finalization owns only its unused regular image, exact loop device
      and private mounts. Boot files/entry/registry precede sealing; refreshed
      unsigned SHA256SUMS match the resulting raw file. Signed inputs refuse
      before mutation unless a separately implemented re-signing path exists.
- [ ] Native developer deploy returns **queued**, not installed/promoted/running;
      live dpkg status and live desktop services are unchanged. Candidate APT
      actually runs ordinary and kernel/LRM maintainer scripts; failure cannot
      be hidden by fake update-grub/initramfs effects.
- [ ] Stage, restart mid-download/install/verification, and resume the same
      mounted candidate/relative subvolume/ESP identity. Changing layout mid-flight
      refuses reinterpretation. A matching latest kernel/initrd pair is copied
      before the opt-in candidate's RO property becomes true.
- [ ] With each real backend, actual `/proc/cmdline` and `findmnt` show the
      recorded root UUID, selected root's own subvolume and ro/rw policy.
      GRUB uses the correct ESP-owned grubenv for RO; systemd-boot uses the
      declared ESP. Healthy promotion and three failed trials retain/select the
      known-good root without overwriting `@`, profiles or Work.
- [ ] As a normal local UID >= 1000, `templar account password` applies real
      PAM policy and changes only that caller. Another UID/root/arbitrary target,
      an unsealed/oversized/invalid secret FD or cancelled authorization refuses.
      Colon works; NUL/CR/LF do not. Password/fingerprint login still follows
      the unchanged existing stack after success.
- [ ] During maintenance the persistent root-owned non-secret marker gates
      common-account for password/fingerprint/login/su/polkit irrespective of
      real UID; State-bind absence, malformed marker and malformed/future intent
      fail closed. No caller can create the marker, read the private intent, or
      bypass recovery by unreadable-journal existence semantics.
- [ ] Kill only the exact owned helper PID / interrupt the disposable operation
      before intent, while preparing, after prepared partial propagation, and
      after committed intent. Restart recovery before allowing any PAM login.
      Orphan marker/scratch cleanup is proven; lost-marker intent recreates the
      gate; prepared restores old and committed preserves new encrypted rows.
      Current/fallback/candidate all authenticate the intended password and
      restore every original Btrfs RO property/file metadata. The live user root
      mount never becomes writable.
- [ ] Inspect owned process argv, logs, journal/request/temporary files: no
      plaintext password appears. Unknown account structure, unjournaled concurrent
      credentials, incompatible/future State readers or unsupported profile
      rewind refuses without touching Work.
- [ ] Record retention behavior: booted/current/previous/known-good/trial roots
      survive; removed roots do not remain password-writer registry targets.
      RO package cache is not reported as reclaimed. Logical unlink estimates
      are not claimed as exact physical CoW space.

Unattended privilege failure or missing real reboot/hardware leaves these boxes
unticked. Writable default stays in force; an RO opt-in is not promotion evidence.
