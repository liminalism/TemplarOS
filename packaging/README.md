# Packaging paths (ADR-0009)

**Stage 1 — quick local packages.** Every binary crate carries `[package.metadata.deb]`; `cargo deb -p <crate>`
produces a `.deb` in `target/debian/`. `just deb` builds them all; `just repo-local` publishes them into a
local aptly repo (`distro/apt/aptly.conf`) so the dev host installs through apt — never `/usr/local`.

**Stage 2 — the archive.** One source package `legeos` (`debian/` at the repo root): `just vendor` writes
`vendor/` + `.cargo/config.toml` into the orig tarball; `debian/rules` overrides `dh_auto_build` with
`cargo build --release --frozen --offline --workspace`. Toolchain target is Ubuntu 26.04's rustc 1.93
(`rust-version` in `Cargo.toml`; CI's `msrv` job). Metapackages: `distro/metapackages/`.

**Why not dh-cargo?** It expects every dependency to be a Debian-packaged crate — impractical for zbus 5,
tokio 1.5x, smithay. Vendoring is what COSMIC's own packaging does.
