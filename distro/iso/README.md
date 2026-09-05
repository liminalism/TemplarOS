# distro/iso — the installation medium

A UEFI-only live ISO: the LegeOS session, booted through Canonical's signed shim and GRUB, with
one build set at `/legeos/devrepo` and the provisioner (`legeos-install`, from `legeos-cli`).
Installing is running `sudo legeos-install --free-space /dev/<disk> --user <you>` in the live
session, reading the plan, and adding `--apply`. ISO-PLAN.md §3 is the decision; this is the tool.

```sh
just deb && sudo lege dev repo publish          # a set the medium can carry
just iso                                        # → /var/tmp/legeos-build/@build/legeos-<set>.iso
tests/integration/iso-qemu.sh                   # boots it under Secure-Boot OVMF
```

`build.sh` runs `mmdebstrap` in unshare mode (no root) with the metapackages and the `live` layer
of `distro/seeds/selection.toml`, pipes the tar into `mksquashfs`, copies the kernel, initramfs and
the signed boot chain out of the rootfs, and writes the ISO with `xorriso`. The `live` layer and
`live/customize.sh` are the only places the live session differs from an installed one: casper,
autologin of a `legeos` user, passwordless sudo. None of it is packaged.

Each step is skipped when its output exists under the work directory; delete
`iso/casper/filesystem.squashfs` to bootstrap again. Beside the ISO: `.sha256`, and
`legeos-<set>.TIERS.manifest` — every package on the medium with its version and tier (`native`:
built by this repository; `inherited`: Ubuntu's, unchanged).

mkosi remains the VM/dev disk image (ADR-0006); it has no ISO output and its systemd-boot,
Secure-Boot-off posture is not the production boot path.
