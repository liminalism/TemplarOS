# session/

The TemplarOS session: `templar-session/` (the entry binary run from `wayland-sessions/templaros.desktop`) and
`units/` (systemd user/system units, greeter config). See `units/README.md`.

## Authentication agent and drive automount

`templar-session` starts `/usr/bin/lxqt-policykit-agent` and `udiskie` as direct children after
the compositor is ready. The agent must belong to the greetd PAM login session: a systemd
user-manager service cannot register the same UnixSession subject. These children end when the
session launcher ends; no process-name killing or live-session service restart is used.

The packaged udiskie configuration automounts ordinary removable filesystems. Internal data
volumes require explicit UUID opt-in; root, EFI, recovery and other-seat devices are not enabled
implicitly. UDisks operations outside this allowlist retain their normal polkit authentication.
PCManFM's prompt is supplied by the session authentication agent, not a separate `sudo` dialog.

For the reference machine's **Second** data filesystem, review and install this configuration:

```sh
scripts/configure-desktop-access.py --user dk --mount-uuid 2EEE1B6CEE1B2C19 --dry-run
sudo scripts/configure-desktop-access.py --user dk --mount-uuid 2EEE1B6CEE1B2C19
```

The helper writes `/etc/templaros/udiskie.json` and a narrowly scoped polkit rule. Only the
specified active local account may mount the specified UUID without another prompt; formatting,
unlocking, other-user and other-seat operations are not granted. Repeat `--mount-uuid` for another
deliberately selected data volume. This replaces the helper-managed allowlist rather than merging
arbitrary system permissions. No device is mounted and no running session is changed by the helper.

After installing the rebuilt packages, log out and back in yourself. Check that Second appears in
PCManFM's Places and opens without an administrator prompt, and that another protected volume
still presents the authentication agent. NTFS left hibernated by Windows may refuse a writable
mount; shut Windows down fully rather than overriding its filesystem safety check.

`just predeploy-checklist` prints every outstanding manual acceptance check from active AKR heads,
grouped by what to try in the new session. It does not deploy or terminate anything.

## Terminal and editor session hooks

The packaged Bash, zsh and fish hooks report prompt-directory changes and foreground
completion events; they do not keep a polling process alive. Each shell gets a private,
bounded 32-slot journal under its runtime directory. Exit status and every pipeline
element's status are captured before other prompt hooks run. Only short command labels
enter the session catalogue. `TEMPLAR_GARDEN_FULL_COMMANDS=1` opts into bounded command
detail in that private journal, never into the HomeGarden D-Bus payload.

Leading-space commands remain excluded from the journal. Bash also honors this when
`HISTCONTROL` is unset or only ignores duplicates: it inspects the admitted raw history line
without changing history settings. Disabled or unavailable history fails closed for recording.
The hooks preserve the user's prompt chain and job control. Bash arms recording at its
PS0 command-read boundary, not by guessing which DEBUG commands belong to a prompt:
compound prompt hooks and Kitty's shell-integration internals do not create journal rows.
The PS0 addition emits nothing and does not fork; existing PS0 content and DEBUG handlers
remain chained. DEBUG capture/installation waits for the first prompt so that Bash has
restored any caller trap hidden during `source`; only the completion prompt function
inherits that handler, without enabling shell-wide functrace. Bash's DEBUG trap does not
time a bare subshell group; do not interpret an absent journal entry as success.

`tests/integration/garden-session-hooks.py --shell bash|zsh|fish` runs real interactive
terminal readers in an owned PTY, with isolated home/config/runtime directories and
without user startup files. It checks journal permissions, leading-space privacy and
pipeline status. The Bash run also sources the installed Kitty Bash integration through
its real ENV injection in an isolated startup environment, exercising scalar/array
compound prompts, both hook load orders, PS0 preservation and inherited DEBUG/extdebug
behavior. It checks empty prompt cycles, real command output, pipeline status, privacy
and Kitty's actual prompt/command/CWD protocol marks. `--kitty-bash PATH` selects a
different installed integration script; absence is reported rather than replaced by a
copy or disabled integration. Its `templar` command recorder is only a hook boundary
fixture; it does not certify broker identity, editor projection or real application
Resume. Those require an isolated session with the real session daemon and user manager.
