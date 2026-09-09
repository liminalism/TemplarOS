# User session units

`templaros-session.target` pulls in the compositor core, the policy daemon, the governor, the file service and
the shell as **systemd user units** (systemd's DESKTOP_ENVIRONMENTS convention). `templar-session` starts the
target and waits. Applications are launched by `templar-sessiond` as transient units
`app-templaros-<AppId>@<InstanceId>.service` under `app.slice`.

Protection values (`MemoryLow`, `CPUWeight`, `IOWeight`, `OOMScoreAdjust`) implement "protect the interface
explicitly" from the planning docs; `templar doctor` (`protected-units`) verifies they are present. Audio
(`pipewire`, `wireplumber`) is protected by drop-ins in `distro/policy/systemd/user/`.
