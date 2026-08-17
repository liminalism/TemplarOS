# cli/ — `lege`

One command line over the OS: `app`, `open`, `res`, `files`, `update`, `history`, `providers`, `settings`,
`project`, `power`, `doctor`. `lege doctor` is the single integration-check engine (library in
`cli/lege/src/doctor/`), reused by `tests/distro-checks` (`--root <image>`) and `tests/hardware`
(`--hardware`). Fluent strings: `cli/lege/i18n/`.
