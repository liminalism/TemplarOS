# Versioning

* **Distribution**: `MAJOR.MINOR` (`0.1` = first scaffold; `1.0` = first release proving the five claims in
  `docs/planning/01-initial-shape.md`). Each distribution version is bound to one Ubuntu LTS series
  (`base.toml`), and carries a codename (`aurora`).
* **Components** (this workspace): SemVer per crate/binary, all released together (`workspace.package.version`);
  Debian version `<upstream>~dev<N>` before `1.0`, then `<upstream>-<rev>`.
* **Forks**: `<upstream-version>+legeos<N>` so the upstream base is always visible.
* **Interfaces**: D-Bus interfaces carry a trailing integer (`…1`); a changed signature is a new interface.
