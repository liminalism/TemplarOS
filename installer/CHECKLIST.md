# Bare-metal checklist — run by a person, at the machine

Everything here needs a real UEFI machine with Secure Boot, a real NVIDIA card and a real reboot.
No agent, test or CI job has executed any of it; treat an unticked box as unknown, not as passing.
Take it in order: each section assumes the one above it held.

Print or keep a second device handy — steps 4 and 5 involve rebooting the machine you are reading
this on. Have a live USB ready before you start, and read §6 before you need it.

## 1. Before provisioning

- [ ] The target disk is one you are willing to lose entirely. `provision.sh` does not merge, dual
      boot, resize or preserve anything.
- [ ] Firmware is in UEFI mode, not CSM/legacy. There is no BIOS boot path.
- [ ] Secure Boot state noted: `mokutil --sb-state` → `SecureBoot enabled` / `disabled`.
      Write down which, so §4 compares against something.
- [ ] You have read the plan: `installer/provision.sh --device /dev/sdX --user <you>` (no `--apply`).

## 2. Provision

- [ ] `sudo LEGEOS_PROVISION_ALLOW_DEVICE=1 installer/provision.sh --device /dev/sdX --i-mean-it
      --user <you> --apply` completed without an error.
- [ ] `lege doctor --root <mount>` at the end reported no fault it called fatal.

## 3. Chain of trust — check before the first boot

With the target still mounted, or from a live USB afterwards:

- [ ] The ESP holds Canonical's shim, not a self-signed one:
      `sha256sum <esp>/EFI/LegeOS/shimx64.efi` matches the one in the installed `shim-signed`
      package (`dpkg -L shim-signed`).
- [ ] GRUB is the signed build: `grubx64.efi` came from `grub-efi-amd64-signed`, and
      `dpkg -V grub-efi-amd64-signed shim-signed` reports nothing.
- [ ] The kernel is Ubuntu's: `dpkg -S <root>/boot/vmlinuz-*` names a `linux-image-*` package from
      Ubuntu, not a locally built file.
- [ ] The NVIDIA modules are Canonical's LRM, not DKMS:
      `dpkg -l 'linux-modules-nvidia-*'` shows an installed package, and
      `dpkg -l 'nvidia-dkms-*'` shows none.
- [ ] `<root>/etc/apt/apt.conf.d/01legeos-keep-kernels` exists (the previous kernel will survive).

## 4. First boot, with Secure Boot ON

- [ ] Enable Secure Boot in firmware if §1 said it was off, then boot the target.
- [ ] It boots to the greeter with **no MOK enrolment prompt**. A blue MOK screen means something
      in the chain is not Canonical-signed — stop and record what.
- [ ] `mokutil --sb-state` → `SecureBoot enabled`.
- [ ] `cat /proc/cmdline` contains `legeos.deployment=<id>` and the id matches the directory under
      `/boot/efi/EFI/legeos/`. This is the one fact the whole rollback machinery rests on: without
      it hostd reports "cannot tell" and counts no failed boot attempts.
- [ ] `lsmod | grep nvidia` is non-empty and `dmesg | grep -i 'module verification failed'` is
      empty — the modules loaded *and* the kernel trusted their signature.
- [ ] `nvidia-smi` prints the card. If it does not, `lege doctor --hardware` before anything else.
- [ ] The account created in §2 required a new password at first login, and there was no autologin.

## 5. The previous kernel is still selectable

- [ ] The GRUB menu (hold `Shift`, or `Esc` on some firmware) lists the LegeOS deployment entries
      first, and Ubuntu's own entries with the previous kernel underneath.
- [ ] Boot the previous kernel once from that menu. It reaches a shell or the greeter.
- [ ] Back on the current kernel: `lege dev status` reports the same deployment identities as
      before that excursion.

## 6. Rollback — practise it before you need it

- [ ] `lege update status` (or `lege dev status --deployments`) names the current, previous and
      known-good deployments, and they are not all the same id.
- [ ] Stage something harmless and let it fail its health gate, or reboot into the candidate and
      power-cycle it before `lege-boot-health` completes, three times. The machine must come back
      on the known-good deployment by itself.
- [ ] After the automatic rollback, `/proc/cmdline` names the known-good id, and
      `/boot/grub/legeos-entries.cfg` no longer contains the abandoned candidate.

### If the machine does not come back

1. **The GRUB menu still has the known-good entry.** Pick it by name — the menu entry id *is* the
   deployment id. This is the first thing to try and it needs no other media.
2. **GRUB drops to a shell** (`grub>`): `set prefix=(hd0,gpt2)/boot/grub`, `insmod normal`,
   `normal`. If `legeos-entries.cfg` is unreadable, Ubuntu's own `10_linux` entries are still in
   `grub.cfg` and will boot the last kernel apt installed.
3. **No GRUB at all**: boot a live USB and see the recovery-shell procedure in
   `distro/deploy/README.md`, which covers mounting the subvolumes, chrooting, and restoring the
   entries file or the bootloader.
4. Record what you saw. A boot failure that is not written down is a boot failure that gets
   rediscovered.

## What to report back

For each unticked box: the step, the exact command, its output, and whether the machine was usable
afterwards. "It worked" without the commands is not evidence anyone can act on later.
