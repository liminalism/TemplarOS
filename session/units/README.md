# session/units — how the session is assembled

* `user/` — `legeos-session.target` and the core user units with their protection values.
* `system/` — `lege-hostd.service`, `lege-boot-health.service`.
* `wayland-sessions/legeos.desktop` — what the greeter offers; runs `lege-session`.
* `greetd/` — greeter configuration.

The Debian packaging (`debian/*.install`, `[package.metadata.deb]`) installs these; nothing is ever placed
under `/usr/local` or in home directories.
