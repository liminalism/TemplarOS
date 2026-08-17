# ADR-0005: Upstream forks are pinned by manifest, not submodules

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
The user's COSMIC changes live in four separately maintained submodules with documented rebase and
detached-HEAD pain. Stage 2 wants each fork as a proper downstream repo with `upstream` + patch branches.

## Decision
`distro/forks/manifest.toml` pins URL/branch/commit per fork (cosmic-comp, cosmic-files,
xdg-desktop-portal-cosmic, cosmic-greeter, cosmic-applets); `just forks-fetch` clones into gitignored `forks/`.
`compositor/lege-comp-integration` is a path crate the compositor fork depends on; the files fork depends on
`lege-client`.

## Consequences
No submodule state in this repo; CI checks pins and rebases monthly; the distribution build is reproducible
from the manifest.
