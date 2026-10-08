# cli/ — `templar`

One command line over the OS: `app`, `open`, `terminal`, `res`, `files`, `archive`, `pcmanfm`, `update`, `history`, `providers`, `settings`,
`wallpaper`, `project`, `power`, `dev`, `doctor`. `templar doctor` is the single integration-check engine (library in
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

`templar terminal [--directory DIR] [--hold] [--dry-run] [-e CMD ARGS...]` starts the Terminal
provider role through `Launcher1` in DIR (default: the current directory), so the terminal is its
own application rather than a child of whoever asked. `templar-terminal`, shipped beside it, is
PCManFM-Qt's `Terminal=` command. `templar pcmanfm repair [--backup]` rewrites a PCManFM-Qt
`Terminal` / `FallbackIconThemeName` that cannot work on this machine, as the session does at
startup; see
[`docs/architecture/files.md`](../docs/architecture/files.md#terminal-and-icons).

## Wallpaper

`templar wallpaper [inspect] --json` reports desired policy, pause/pending/error state
and the compositor's separate accepted/presented generations. Acceptance is not proof
of a visible frame. `next`, `previous`, `pause`, `resume` and `refresh` call Wallpaper1;
`enable`, `disable` and `reset-enabled` change the existing Settings1 enable gate.

`templar wallpaper config show` prints the committed policy as JSON.
`config set FILE` (or `config set -` for stdin) replaces it through Settings1 validation;
`config reset` restores the schema default without changing the enable gate.
For example, save `config show` to a file, edit `sizing` to `fit`, `order` to
`sequential`, `interval_seconds` to `300` (or `null` for off), and set
`texture` to `{"kind":"max_edge","pixels":1920}` or
`{"kind":"max_pixels","pixels":2073600}`, then submit that file.
Native storage is `{"kind":"native"}` and is independent of placement/sizing.
Budgets are byte counts; `quantize_colors` is `null` by default or an explicit
experimental WuDP palette count. Indexed output is opt-in, not a claim of measured savings.
Output overrides are keyed by compositor output name and can independently set
`sizing`, `ground`, `texture` and `source`; null/absent overrides inherit shared policy.

`config add-source PATH` accepts a file or directory with Unix pathname bytes
preserved from the shell argument, including non-UTF8 bytes. `config add-source-bytes
2fff80` accepts the same bytes in hexadecimal. JSON `sources` and output `source`
are byte arrays, never lossy pathname strings. `config remove-source INDEX` removes
the zero-based entry shown in `config show`.

Settings exposes the same controls under **Appearance**, not Background applications:
source add/remove (UTF8 paths or `hex:` bytes), order/recursion, off or 5-minute–1-hour
rotation, six sizing modes, ground, native/max-edge/max-pixel texture storage, both
budgets, opt-in WuDP and per-output overrides. Apply an output override to the draft,
then Save policy. Enable changes immediately; policy Save/Reset use Settings1 and
its existing History/undo authority. Runtime controls and actual applied/error
observations use Wallpaper1 subscriptions, including session-owner recovery.

Headless rendering/action coverage is in `templar-settings`'s
`wallpaper::tests::headless_panel_renders_fields_and_forwards_next_without_a_bus`.
For a real GUI smoke, use an isolated session bus and nested compositor, run
`templar-settings --page appearance`, exercise Save/Reset and all five action
buttons, and compare `templar wallpaper inspect --json` plus presented output
generations/screenshots. `templar-settings --print --page appearance` is a text
diagnostic, not GUI/rendering evidence; do not exercise these mutations on a live
user session as a test.
