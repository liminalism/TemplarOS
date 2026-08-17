# distro/image — mkosi disk image (dev / CI / VM)

`mkosi.conf` + `mkosi.conf.d/` + `mkosi.repart/` (ESP + btrfs subvolumes) + `mkosi.extra/` +
`mkosi.postinst`. Produces `mkosi.output/legeos_0.1.raw`, bootable in `mkosi vm` and dd-able to a USB
stick. Requirements on the host: `sudo apt install mkosi` (26.04 ships mkosi 26; on older hosts
`pipx install mkosi`). `just image` composes `mkosi.conf.d/50-seeds.conf` from `../seeds`, renders
`os-release` from `../branding`, then runs `mkosi build`.

Not in M0: actually building the image (needs network + the LegeOS apt repo). `mkosi summary` validates
the configuration.
