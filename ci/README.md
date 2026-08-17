`ci/run.sh` is the whole pipeline (called by `.github/workflows/ci.yml` and usable locally): fmt, clippy
`-D warnings`, tests, MSRV 1.93 check, cargo-deny (if installed), xtask checks, integration smoke, cargo-deb smoke.
