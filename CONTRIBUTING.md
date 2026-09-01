# Contributing

* **Read `AGENTS.md`** (project knowledge protocol) and `docs/adr/` before proposing structure changes.
* **Every component is a Debian package.** No `/usr/local`, no untracked home-directory state.
* **Forks**: never vendored. Add to `distro/forks/manifest.toml`; keep `upstream` and `legeos` branches; rebase
  monthly; patches must be explainable in one sentence each.
* **Interfaces**: change `interfaces/` first; `cargo xtask check-interfaces` must pass. Shipped signatures are
  frozen — bump the interface version.
* **`lege-window` stays free of tokio/zbus** (`cargo xtask deps-check`).
* **Tests**: policy engines are pure and unit-tested; geometry is property-tested; `lege doctor` is the one
  integration checker (runtime, image, hardware).
* **Style**: `just check` (fmt, clippy `-D warnings`, tests, MSRV 1.97). Edition 2024, AGPL-3.0-only header not
  required (licence is repository-wide), `#![forbid(unsafe_code)]` unless a crate documents why not.
* **Commits**: one concern per commit; reference the ADR when a decision changes; AKR records for durable
  knowledge (`knowledge.propose` / `revise`), papercuts for friction.
