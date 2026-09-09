# ADR-0005: Upstream forks are pinned by manifest, not submodules

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
The user's COSMIC changes live in four separately maintained submodules with documented rebase and
detached-HEAD pain. Stage 2 wants each fork as a proper downstream repo with `upstream` + patch branches.

## Decision
`distro/forks/manifest.toml` pins URL/branch/commit per remaining fork;
`just forks-fetch` clones into gitignored `forks/`.

## Consequences
No submodule state in this repo; CI checks pins and rebases monthly; the distribution build is reproducible
from the manifest.

## Amendment (2026-08-21, ADR-0012)
`cosmic-comp` and `cosmic-greeter` have left the manifest. The compositor is written against
Smithay and is an ordinary workspace member, not a fork; the greeter is our own. The mechanism
here — manifest pins rather than submodules — is unchanged and still governs the forks that
remain. `compositor/templar-comp-integration` is now linked by a workspace member rather than by an
external fork, which is a simplification: the dependency no longer crosses a repository boundary.

## Amendment (2026-08-24, standalone Files)

`cosmic-files` has also left the manifest. Its declared `templaros/cosmic-files` remote was never
materialised and does not exist, while the service-owned browsing and journalled-operation model
now makes the GUI a bounded presentation client rather than a second filesystem implementation.
`apps/templar-files` is therefore a first-party Pixelkit workspace member over `templar-client`; the
portal and applets remain governed by this ADR's external-fork mechanism.
