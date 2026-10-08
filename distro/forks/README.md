# distro/forks — external sources, not vendored application code

`manifest.toml` lists each fork (upstream URL, our fork URL, patch branch, pinned commit, purpose,
Debian package). `fetch.sh` (via `just forks-fetch`) clones/updates them into `forks/` at the repo root
(gitignored). No submodules — the detached-HEAD/rebase pain the planning docs describe is exactly what this
avoids. Each fork repo keeps `upstream` (tracking) and `templaros` (patches) branches; CI runs a rebase check
monthly (`release/CADENCE.md`).

The compositor is not a fork (ADR-0012): `templar-comp` is a workspace member built on Smithay.
`templar-files` is likewise a first-party workspace client over `templar-client`/`templar-filed`; keeping it
here makes its service contracts, desktop integration and Debian package testable with the OS.

## Packaging lives here; source does not

A fork may carry `distro/forks/<name>/debian/` — the control file, rules and changelog that turn
its source into a TemplarOS package. That is not vendoring: the code stays in the fork, and what lands
here are the facts TemplarOS is responsible for — the package's name, its dependencies, and where its
files install — so `cargo xtask check-packaging` can see them and `distro/seeds/selection.toml` can
name the package without anything having to guess.

```sh
just forks-fetch                                   # clone/update into forks/ at the repo root
scripts/build-fork-deb.sh nm-tray-2                # -> target/debian/, beside the cargo-deb output
scripts/build-fork-deb.sh nm-tray-2 --source DIR   # from a checkout you already have
```

The built version carries the fork's short commit (`0.5.1+templaros1~g0965344`), and `.dirty` when the
checkout has uncommitted changes, so an installed package names what produced it.

A fork may also carry `distro/forks/<name>/preflight.sh`, which is given the source directory and
may refuse the build. `nm-tray-2` uses one: the Home Field has no system tray and drives the binary
with `--popup`, so a checkout without that option would install cleanly, resolve as the network
provider and then do nothing when clicked. Failing at build time is the cheaper place to find out.

## Local source closure: LiminalView

The owner-supplied `liminalview` directory is **not a Git checkout**. Its manifest
entry therefore declares `source_kind = "local"` and a repository-relative
source directory, not invented upstream/fork/branch/pin fields. `forks-fetch`
reports this distinction and does not pretend to download it.

```sh
scripts/build-fork-deb.sh liminalview --source ../liminalview
scripts/build-fork-deb.sh liminalview --source ../liminalview --dry-run
scripts/build-fork-deb.sh liminalview --source target/source-archives/liminalview-HASH.tar.gz
```

Before closing a live source tree for the first time, populate missing locked
downloads with `cargo fetch --locked --manifest-path ../liminalview/Cargo.toml`.
The closure builder then runs offline; rebuilding a retained complete archive
does not need the original Cargo cache or sibling directories.

The local builder resolves the locked Cargo graph, preserves each path
dependency's package source/assets (including embedded PixelKit fonts) and
owning workspace manifests/licenses, then vendors locked registry/Git inputs.
It retains the sibling layout, including `pixelkit`, `Lege-ecosystem/lege-codecs/jp2lam`,
`jpegXL-rs/JPXL` and `bpg-rs`; copying the viewer alone is not a source recipe.
Missing locked downloads, source paths, fonts or native build tools fail
instead of being replaced with a shim or an installed binary.
It excludes assistant settings, credentials, dotenv files, private runtime
environment files and source working caches; unrelated workspace tools and
large conformance corpora are not a backup input for the native package.

The complete bundle includes the Debian overlay and a relative-path,
content-hashed source/dependency/license inventory. A deterministic,
SHA256-addressed `.tar.gz` plus checksum is retained in `target/source-archives/`.
The actual package builds **from that archive**, with an empty Cargo home,
vendored dependencies and offline resolution, rather than from live sibling
trees. It installs `/usr/bin/liminalview`, regenerates the Linux desktop entry
from the built executable's `--print-formats` inventory with the explicit installed
`--bin-path /usr/bin/liminalview`, and ships source provenance. No installed package
or user desktop is modified.

The native developer lane checks the actual Rust/Cargo binaries against the
locked dependency graph's maximum MSRV and runs `dpkg-checkbuilddeps` for every
non-Rust build dependency before invoking the ordinary debhelper binary rules.
This supports the project's Rustup-managed compiler without pretending it is
a dpkg-installed Rust package. Conventional source builds still declare
`rustc` and `cargo` >= 1.97 in `Build-Depends`.
An explicit `CARGO_BUILD_TARGET` must match the native compiler host. After that
check, the builder clears this equivalent override for the binary rules so even
a retained archive uses the canonical `target/release` payload location.

`cargo xtask build-set` includes selected local-source registrations at the
same BuildSetId as the rest of the set. To rebuild from a retained archive
without sibling sources, set `TEMPLAROS_SOURCE_ARCHIVE_LIMINALVIEW` to its path.
Standalone builds stamp the archive hash in the Debian version. Each package
also has a `.source.json` sidecar linking it to the complete archive SHA256.
`--dry-run` closes and inspects the real offline source bundle, but does not
claim native compilation or a package artifact.

A retained content-addressed archive is the honest local reproduction input.
The absent public release origin and immutable Git pin remain a separate
checkout-only release prerequisite; no public-source or distribution-wide
license-review claim follows from this local build.

