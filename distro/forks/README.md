# distro/forks — upstream forks, pinned not vendored (ADR-0005)

`manifest.toml` lists each fork (upstream URL, our fork URL, patch branch, pinned commit, purpose,
Debian package). `fetch.sh` (via `just forks-fetch`) clones/updates them into `forks/` at the repo root
(gitignored). No submodules — the detached-HEAD/rebase pain the planning docs describe is exactly what this
avoids. Each fork repo keeps `upstream` (tracking) and `legeos` (patches) branches; CI runs a rebase check
monthly (`release/CADENCE.md`).

The compositor is not a fork (ADR-0012): `lege-comp` is a workspace member built on Smithay. The files
fork depends by path on `lege-client`.
