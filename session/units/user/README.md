# User session units

`legeos-session.target` pulls in the compositor core, the policy daemon, the governor, the file service and
the shell as **systemd user units** (systemd's DESKTOP_ENVIRONMENTS convention). `lege-session` starts the
target and waits. Applications are launched by `lege-sessiond` as transient units
`app-legeos-<AppId>@<InstanceId>.service` under `app.slice`.

Protection values (`MemoryLow`, `CPUWeight`, `IOWeight`, `OOMScoreAdjust`) implement "protect the interface
explicitly" from the planning docs; `lege doctor` (`protected-units`) verifies they are present. Audio
(`pipewire`, `wireplumber`) is protected by drop-ins in `distro/policy/systemd/user/`.
