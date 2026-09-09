# session/units — how the session is assembled

* `user/` — `templaros-session.target` and the core user units with their protection values.
* `system/` — `templar-hostd.service`, `templar-boot-health.service`.
* `wayland-sessions/templaros.desktop` — what the greeter offers; runs `templar-session`.
* `greetd/` — greeter configuration.

The Debian packaging (`debian/*.install`, `[package.metadata.deb]`) installs these; nothing is ever placed
under `/usr/local` or in home directories.
