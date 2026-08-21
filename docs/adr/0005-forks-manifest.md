# ADR-0005: Upstream forks are pinned by manifest, not submodules

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
The user's COSMIC changes live in four separately maintained submodules with documented rebase and
detached-HEAD pain. Stage 2 wants each fork as a proper downstream repo with `upstream` + patch branches.

## Decision
`distro/forks/manifest.toml` pins URL/branch/commit per fork (cosmic-files, xdg-desktop-portal-cosmic,
cosmic-applets); `just forks-fetch` clones into gitignored `forks/`. The files fork depends on `lege-client`.

## Consequences
No submodule state in this repo; CI checks pins and rebases monthly; the distribution build is reproducible
from the manifest.

## Amendment (2026-08-21, ADR-0012)
`cosmic-comp` and `cosmic-greeter` have left the manifest. The compositor is written against
Smithay and is an ordinary workspace member, not a fork; the greeter is our own. The mechanism
here — manifest pins rather than submodules — is unchanged and still governs the forks that
remain. `compositor/lege-comp-integration` is now linked by a workspace member rather than by an
external fork, which is a simplification: the dependency no longer crosses a repository boundary.
