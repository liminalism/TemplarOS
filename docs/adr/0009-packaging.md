# ADR-0009: Debian packaging: cargo-deb now, vendored source package for the archive

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Decision
Stage 1: `cargo deb -p <bin>` (`[package.metadata.deb]` in every binary crate) into a local aptly repo.
Stage 2: one source package `legeos` with `debian/` at the repo root, `vendor/` in the orig tarball,
`dh_auto_build` → `cargo build --release --frozen --offline --workspace`; metapackages in `legeos-meta`;
`legeos-archive-keyring` ships the key, deb822 `legeos.sources` and pins rejecting third-party driver repos.
dh-cargo is not used (needs Debian-packaged crates).
