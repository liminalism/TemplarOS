# tests/

* `integration/` — session-level tests (nested compositor / `mkosi vm`).
* `distro-checks/` — image-time checks: `lege doctor --root <unpacked-image>`.
* `hardware/` — the laptop test matrix as check definitions run by `lege doctor --hardware`.
* `fixtures/` — sample manifests, `mimeapps.list`, output topologies.

Unit and property tests live next to the code (`cargo test --workspace`).
