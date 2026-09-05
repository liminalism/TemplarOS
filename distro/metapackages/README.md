# Metapackages (source package `legeos-meta`)

| Package | Pulls in |
|---|---|
| `legeos-desktop` | the `session`, `desktop`, `hardware`, `graphics`, `media`, `opt-ime`, `opt-a11y` and `opt-bluetooth` layers |
| `legeos-session-full` | the `session` layer only (Stage 1: a replacement session on an existing Ubuntu) |
| `legeos-dev-rust` | the `dev-rust` layer — build this workspace on the machine it runs on |
| `legeos-dev-cuda` | the `dev-cuda` layer — CUDA, deliberately separable from `legeos-dev-rust` |
| `legeos-kernel` | `linux-generic` (7.0), `linux-firmware`, and — as **versioned** Depends — the LRM `linux-modules-nvidia-<branch>-generic` matching the selected flavour, so kernel + modules + userspace move as one tested set |
| `legeos-nvidia-open` / `legeos-nvidia-proprietary` | the NVIDIA userspace + LRM for one flavour; `Conflicts:` each other and `nvidia-driver-*` from third-party repos |
| `legeos-archive-keyring` | the archive signing key + `sources.list.d/legeos.sources` + `preferences.d/` pins |

Each directory holds a `control.in`, and **every one of them is generated**: `cargo xtask seeds`
renders it from the `[[metapackage]]` stanzas in `distro/seeds/selection.toml`, so a metapackage's
dependency list cannot drift from the image's package list the way it had. Do not edit a
`control.in`; edit the declaration and re-render. `@KERNEL_MIN@`, `@NVIDIA_BRANCH@`,
`@NVIDIA_LRM_VERSION@` and `@NVIDIA_USERSPACE_VERSION@` are substituted at package-build time from
`distro/base.toml` (`[kernel] min_version`, `[nvidia] branch/lrm_version/userspace_version`, taken
from the resolved closure in `distro/seeds/closures/`).

`cargo xtask build-set` (`just deb`) builds every directory here with `dpkg-deb` at the set's
version, after the cargo-deb packages, so one build set holds all of them. `legeos-archive-keyring`
carries `distro/apt/preferences.d/*`; every other metapackage is dependencies only.
