# Contributing

* **Read `AGENTS.md`** (project knowledge protocol) and `docs/adr/` before proposing structure changes.
* **Every component is a Debian package.** No `/usr/local`, no untracked home-directory state.
* **Forks**: never vendored. Add to `distro/forks/manifest.toml`; keep `upstream` and `templaros` branches; rebase
  monthly; patches must be explainable in one sentence each.
* **Interfaces**: change `interfaces/` first; `cargo xtask check-interfaces` must pass. Shipped signatures are
  frozen — bump the interface version.
* **`templar-window` stays free of tokio/zbus** (`cargo xtask deps-check`).
* **Tests**: policy engines are pure and unit-tested; geometry is property-tested; `templar doctor` is the one
  integration checker (runtime, image, hardware).
* **Style**: `just check` (fmt, clippy `-D warnings`, tests, MSRV 1.97). Edition 2024, AGPL-3.0-only header not
  required (licence is repository-wide), `#![forbid(unsafe_code)]` unless a crate documents why not.
* **Commits**: one concern per commit; reference the ADR when a decision changes; AKR records for durable
  knowledge (`knowledge.propose` / `revise`), papercuts for friction.

`just msrv` reads the patch-pinned minimum compiler from `rust-toolchain.toml` using Python 3.11+
and runs the check explicitly with that toolchain; a missing compiler fails the gate.

Native builds use `target/native/<os-release ID>-<VERSION_ID>` to avoid reusing system-library
links from another OS mounted on the same checkout. `just` sets this automatically; for direct
Cargo commands use `export CARGO_TARGET_DIR="$(scripts/native-target-dir.sh)"` once in your
shell, then `cargo build -p templar-shell`, or use `scripts/cargo-native.sh build -p templar-shell`.
An explicit `CARGO_TARGET_DIR` is never overridden. `debian/rules` retains its explicit `target/`
for `.install` paths; cargo-deb honors the native target for binary assets, while build-set output
remains in `target/debian/`.

The native Rust archive path requires `libarchive-dev` (including its pkg-config metadata)
and `libclang-dev`. The maintained network fork additionally needs `cmake`, `qt6-base-dev`,
`qt6-tools-dev`, `qt6-tools-dev-tools` and `qt6-l10n-tools`: the tools-only packages do not
provide `Qt6LinguistToolsConfig.cmake`. Its Debian overlay also requires `debhelper`.
Install these on the native OS, not on a mounted Lubuntu/WSL root.
`scripts/reinstall-templaros-desktop.sh --skip-build` reuses `target/debian/` packages,
including the maintained network fork; deployment and logout remain the owner's action.
