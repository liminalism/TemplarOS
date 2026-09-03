# distro/image — mkosi disk image (dev / CI / VM)

`mkosi.conf` + `mkosi.conf.d/` + `mkosi.repart/` (ESP + btrfs subvolumes) + `mkosi.extra/` +
`mkosi.postinst`. Produces `mkosi.output/legeos_0.1.raw`, bootable in `mkosi vm` and dd-able to a USB
stick. Requirements on the host: `sudo apt install mkosi` (26.04 ships mkosi 26; on older hosts
`pipx install mkosi`). `just image` runs `compose.sh`, which generates the package
drop-ins (`10-packages-base.conf`, `50-seeds.conf`) from `../seeds/selection.toml` via
`cargo xtask seeds`, renders `os-release` from `../branding`, writes `40-local-archive.conf` when a
local LegeOS build set is named (`$LEGEOS_BUILD_SET`, else the archive's `current` marker), and then
runs mkosi.

Not in M0: actually building the image (needs network + the LegeOS apt repo). `mkosi summary` validates
the configuration.
