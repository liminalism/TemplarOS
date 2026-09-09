# tests/

* `integration/` — session-level tests (nested compositor / `mkosi vm`).
* `distro-checks/` — image-time checks: `templar doctor --root <unpacked-image>`.
* `hardware/` — the laptop test matrix as check definitions run by `templar doctor --hardware`.
* `fixtures/` — sample manifests, `mimeapps.list`, output topologies.

Unit and property tests live next to the code (`cargo test --workspace`).
