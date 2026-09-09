# cli/ — `templar`

One command line over the OS: `app`, `open`, `res`, `files`, `update`, `history`, `providers`, `settings`,
`project`, `power`, `dev`, `doctor`. `templar doctor` is the single integration-check engine (library in
`cli/templar/src/doctor/`), reused by `tests/distro-checks` (`--root <image>`) and `tests/hardware`
(`--hardware`). Fluent strings: `cli/templar/i18n/`.

`templar dev` is the developer's deployment side: `status` (what is installed, what is running, and the
gap between them), `repo` (the local archive at `/var/lib/templaros/devrepo`), `install` / `rollback`
(one pinned apt transaction either way) and `deploy` (publish, gate, install, activate, report).
The set it deploys is built by `cargo xtask build-set`, which stays the developer's own command —
deploying needs root, and building as root leaves a target directory nobody can write to.
