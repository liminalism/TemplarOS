# cli/ — `templar`

One command line over the OS: `app`, `open`, `res`, `files`, `archive`, `update`, `history`, `providers`, `settings`,
`project`, `power`, `dev`, `doctor`. `templar doctor` is the single integration-check engine (library in
`cli/templar/src/doctor/`), reused by `tests/distro-checks` (`--root <image>`) and `tests/hardware`
(`--hardware`). Fluent strings: `cli/templar/i18n/`.

`templar doctor freeze [--boot -1]` is what to run after a hard restart: it collects what the frozen
boot left — the journal slices, the compositor watchdog's `templar_comp::watchdog` lines, kernel
GPU/hang/OOM lines, unit stop times, coredumps, pstore — into
`$XDG_STATE_HOME/templaros/freeze-reports/boot-<id>/` and prints the triage answer (`summary.md`).
It never needs root, but the system journal is readable only to `adm`; `sudo templar doctor freeze`
writes the same report for the invoking user with the kernel side included, and
`scripts/freeze-evidence-setup.sh` (root) adds the user to `adm` and arms SysRq dumps, lockup
reporting, systemd-coredump and a 15 s journal sync.

`templar dev` is the developer's deployment side: `status` (what is installed, what is running, and the
gap between them), `repo` (the local archive at `/var/lib/templaros/devrepo`), `install` / `rollback`
(one pinned apt transaction either way) and `deploy` (publish, gate, install, activate, report).
The set it deploys is built by `cargo xtask build-set`, which stays the developer's own command —
deploying needs root, and building as root leaves a target directory nobody can write to.

`templar archive compress [--output PATH] [--gui] -- FILE_OR_FOLDER...` creates one ZIP
from the selection. `templar archive extract --mode here|smart|directory [--directory PATH]
[--gui] -- ARCHIVE...` extracts every selected archive. Smart mode uses an archive-name
folder without a redundant sole-root wrapper; directory mode opens a folder chooser
unless `--directory` is supplied. `--gui` adds progress, cancellation, error dialogs and
supported encrypted-ZIP password entry through Zenity. Outputs never overwrite existing
files. These local commands back PCManFM-Qt's custom archive menu; see
[`docs/architecture/files.md`](../docs/architecture/files.md#archive-context-menu) for
supported formats and safety limits.
