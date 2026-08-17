# Metapackages (source package `legeos-meta`)

| Package | Pulls in |
|---|---|
| `legeos-desktop` | `session.seed` + `desktop.seed` + `hardware.seed` + `ime.seed` + `a11y.seed` |
| `legeos-kernel` | `linux-generic` (7.0), `linux-firmware`, and — as **versioned** Depends — the LRM `linux-modules-nvidia-<branch>-generic` matching the selected flavour, so kernel + modules + userspace move as one tested set |
| `legeos-nvidia-open` / `legeos-nvidia-proprietary` | the NVIDIA userspace + LRM for one flavour; `Conflicts:` each other and `nvidia-driver-*` from third-party repos |
| `legeos-archive-keyring` | the archive signing key + `sources.list.d/legeos.sources` + `preferences.d/` pins |

Each directory holds a `control.in`; `distro/metapackages/build.sh` (later) renders `debian/control`
for `legeos-meta` from `distro/base.toml`.
