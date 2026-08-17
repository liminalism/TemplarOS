# ADR-0006: mkosi for dev/CI/VM disk images; installer ISO later

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Decision
`distro/image/mkosi.conf`: `Distribution=ubuntu Release=resolute Repositories=main,universe,restricted
Format=disk Bootable=yes Bootloader=systemd-boot UnifiedKernelImages=no ToolsTree=default`, `mkosi.repart/`
(ESP + btrfs subvolumes), packages composed from `distro/seeds/`. mkosi has no ISO output; the installer ISO
(`distro/iso/`, `installer/`) comes later with `ubuntu-image classic` or `live-build`.

## Consequences
One declarative image definition, `mkosi vm` for tests, a straight path to systemd-repart/A-B later.
Host needs mkosi ≥ 25 (`apt install mkosi` on 26.04; `pipx install mkosi` on older hosts).
