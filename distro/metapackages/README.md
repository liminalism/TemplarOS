# Metapackages (source package `templaros-meta`)

| Package | Pulls in |
|---|---|
| `templaros-desktop` | the `session`, `desktop`, `hardware`, `graphics`, `media`, `opt-ime`, `opt-a11y` and `opt-bluetooth` layers |
| `templaros-lxqt` | the complete LXQt 2.4 alternative, Xorg/Openbox, XScreenSaver, and conffiles selecting Openbox/Papirus; also selected by the desktop layer |
| `templaros-session-full` | the `session` layer only (Stage 1: a replacement session on an existing Ubuntu) |
| `templaros-dev-rust` | the `dev-rust` layer — build this workspace on the machine it runs on |
| `templaros-dev-cuda` | the `dev-cuda` layer — CUDA, deliberately separable from `templaros-dev-rust` |
| `templaros-kernel` | `linux-generic` (7.0), `linux-firmware`, and — as **versioned** Depends — the LRM `linux-modules-nvidia-<branch>-generic` matching the selected flavour, so kernel + modules + userspace move as one tested set |
| `templaros-nvidia-open` / `templaros-nvidia-proprietary` | the NVIDIA userspace + LRM for one flavour; `Conflicts:` each other and `nvidia-driver-*` from third-party repos |
| `templaros-archive-keyring` | the archive signing key + `sources.list.d/templaros.sources` + `preferences.d/` pins |

Each directory holds a `control.in`, and **every one of them is generated**: `cargo xtask seeds`
renders it from the `[[metapackage]]` stanzas in `distro/seeds/selection.toml`, so a metapackage's
dependency list cannot drift from the image's package list the way it had. Do not edit a
`control.in`; edit the declaration and re-render. `@KERNEL_MIN@`, `@NVIDIA_BRANCH@`,
`@NVIDIA_LRM_VERSION@` and `@NVIDIA_USERSPACE_VERSION@` are substituted at package-build time from
`distro/base.toml` (`[kernel] min_version`, `[nvidia] branch/lrm_version/userspace_version`, taken
from the resolved closure in `distro/seeds/closures/`).

`cargo xtask build-set` (`just deb`) builds every directory here with `dpkg-deb` at the set's
version, after the cargo-deb packages, so one build set holds all of them. `templaros-archive-keyring`
carries `distro/apt/preferences.d/*`; `templaros-lxqt` carries the defaults from
`distro/lxqt/{session,lxqt}.conf` under `/etc/xdg/lxqt`; the other metapackages are
dependencies only. User LXQt settings override these defaults.

LXQt source versions and archive hashes live in `distro/lxqt/backport.json`.
`scripts/build-lxqt-backport.py --root <offline-root> --state <offline-var>` rebuilds
those sources with the Resolute toolchain in a disposable overlay and retains the
Debian packages under `target/lxqt-2.4/debs`. The build does not upgrade the supplied
root or select binaries from another Ubuntu release. Image/archive builders must
provide these backports alongside the native packages: the frozen Resolute archive
alone contains LXQt 2.3 and cannot satisfy the alternative's versioned dependencies.
