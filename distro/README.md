# distro/ — what makes this a distribution, not a desktop

| Path | Purpose |
|---|---|
| `base.toml` | **the** declaration of the base: Ubuntu 26.04 "resolute", kernel flavour, NVIDIA source, image tool, identity |
| `seeds/` | package lists composed into metapackages and images |
| `metapackages/` | `templaros-desktop`, `templaros-kernel`, `templaros-nvidia-*`, `templaros-archive-keyring` |
| `image/` | mkosi dev/VM disk image (ESP + btrfs subvolumes) |
| `iso/` | installer ISO (later stage) |
| `apt/` | the archive: aptly layout, deb822 source, keyring, pins (one driver authority) |
| `policy/` | systemd/sysctl/tmpfiles/udev/polkit/dbus/apparmor/coredump/pipewire configuration the distro owns |
| `branding/` | os-release, plymouth, icons, palette |
| `release/` | versioning, support window, cadence |
| `hardware/` | declarative profiles + test matrix |
| `deploy/` | boot deployment/rollback design |
| `app-manifests/` | curated application manifests |
| `forks/` | manifest + fetch script for upstream forks |
