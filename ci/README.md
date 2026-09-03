`ci/run.sh` is the whole pipeline (called by `.github/workflows/ci.yml` and usable locally): fmt, clippy
`-D warnings`, tests, MSRV 1.97 check, cargo-deny (if installed), xtask checks, integration smoke, cargo-deb smoke.

`ci/source-build.sh` builds the *source* package in a disposable Ubuntu 26.04 root (`mmdebstrap`, else
`podman`), generating `debian/changelog` from the build-set id into the throwaway copy of the tree —
the one version authority (`@legeos.decision.build-set-id-is-the-version-authority`). With neither backend
installed it prints `NOT EXECUTED` and exits 0; that line is never a pass. `--plan` prints what it would do.
