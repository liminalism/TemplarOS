# crates/ — pure library cores

Every OS domain is a library first: I/O-free policy engines with unit tests, hosted by the thin daemons in
`services/`. `templar-types` is the shared vocabulary; `templar-window` must stay free of async runtimes/D-Bus
(render-thread rule), and so must `templar-field`, the Home Field model the shell renders and sessiond persists
(`cargo xtask deps-check` enforces both); `templar-client` is what applications and UIs link. See
`ARCHITECTURE.md` for the table.
