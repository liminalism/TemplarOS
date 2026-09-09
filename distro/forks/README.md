# distro/forks — upstream forks, pinned not vendored (ADR-0005)

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
