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
