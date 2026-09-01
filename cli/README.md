# cli/ — `lege`

One command line over the OS: `app`, `open`, `res`, `files`, `update`, `history`, `providers`, `settings`,
`project`, `power`, `dev`, `doctor`. `lege doctor` is the single integration-check engine (library in
`cli/lege/src/doctor/`), reused by `tests/distro-checks` (`--root <image>`) and `tests/hardware`
(`--hardware`). Fluent strings: `cli/lege/i18n/`.

`lege dev` is the developer's deployment side: `status` (what is installed, what is running, and the
gap between them), `repo` (the local archive at `/var/lib/legeos/devrepo`), `install` / `rollback`
(one pinned apt transaction either way) and `deploy` (publish, gate, install, activate, report).
The set it deploys is built by `cargo xtask build-set`, which stays the developer's own command —
deploying needs root, and building as root leaves a target directory nobody can write to.
