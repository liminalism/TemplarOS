# services/ — daemons

`lege-sessiond` (session policy, `org.legeos.Session`), `lege-resourced` (governor, `org.legeos.Resources`,
separate/protected), `lege-filed` (`org.legeos.Files`), `lege-hostd` (root, `org.legeos.Host`),
`lege-boot-health` (system oneshot). Units: `session/units/`. Each binary crate carries
`[package.metadata.deb]` for `cargo deb`.
