# distro/forks — upstream forks, pinned not vendored (ADR-0005)

`manifest.toml` lists each fork (upstream URL, our fork URL, patch branch, pinned commit, purpose,
Debian package). `fetch.sh` (via `just forks-fetch`) clones/updates them into `forks/` at the repo root
(gitignored). No submodules — the detached-HEAD/rebase pain the planning docs describe is exactly what this
avoids. Each fork repo keeps `upstream` (tracking) and `legeos` (patches) branches; CI runs a rebase check
monthly (`release/CADENCE.md`).

The compositor fork depends by path on `compositor/lege-comp-integration`; the files fork on `lege-client`.
