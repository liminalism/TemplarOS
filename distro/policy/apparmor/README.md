Profiles for the daemons; start in complain mode, enforce once `templar doctor` and the integration tests
exercise every path. `usr.libexec.templar-hostd` first (root, wraps apt). User daemons (`templar-sessiond`,
`templar-filed`, `templar-resourced`) rely on systemd sandboxing in their units and get profiles later.
