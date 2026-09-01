# Packaging paths (ADR-0009)

**Stage 1 — quick local packages.** Every binary crate carries `[package.metadata.deb]`; `cargo deb -p <crate>`
produces a `.deb` in `target/debian/`. `just deb` builds them all; `just repo-local` publishes them into a
local aptly repo (`distro/apt/aptly.conf`) so the dev host installs through apt — never `/usr/local`.

**Stage 2 — the archive.** One source package `legeos` (`debian/` at the repo root): `just vendor` writes
`vendor/` + `.cargo/config.toml` into the orig tarball; `debian/rules` overrides `dh_auto_build` with
`cargo build --release --frozen --offline --workspace`. Toolchain target is rustc 1.97, the workspace MSRV
(`rust-version` in `Cargo.toml`; CI's `msrv` job) — raised from Ubuntu 26.04's 1.93 by ADR-0008 because the
resolved preview-codec graph requires it. Metapackages: `distro/metapackages/`.

**Why not dh-cargo?** It expects every dependency to be a Debian-packaged crate — impractical for zbus 5,
tokio 1.5x, smithay. Vendoring is what COSMIC's own packaging does.

## Privileged-daemon policy files live in `distro/policy/`

`lege-hostd`'s polkit actions, AppArmor profile and system-bus policy are **not** kept here.
They live in `distro/policy/{polkit,apparmor,dbus}/`, which is the one home for system
configuration the distribution owns, and both packaging paths install from exactly those files:
`debian/legeos-hostd.install` for the source package, and `[package.metadata.deb]` assets in
`services/lege-hostd/Cargo.toml` for `cargo deb`.

A second copy under `packaging/` existed briefly and was removed. Two divergent copies of a root
daemon's privilege policy, one shipped by each build path, is precisely the class of problem this
distribution exists to remove — and it would have shipped a different AppArmor profile depending
on which command built the package.
