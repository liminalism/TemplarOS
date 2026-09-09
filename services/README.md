# services/ — daemons

`templar-sessiond` (session policy, `org.templaros.Session`), `templar-resourced` (governor, `org.templaros.Resources`,
separate/protected), `templar-filed` (`org.templaros.Files`), `templar-hostd` (root, `org.templaros.Host`),
`templar-boot-health` (system oneshot). Units: `session/units/`. Each binary crate carries
`[package.metadata.deb]` for `cargo deb`.
