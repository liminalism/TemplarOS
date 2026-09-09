# Security

* Report vulnerabilities to security@templaros.org (PGP key published with the archive key).
* Privileged surface: only `templar-hostd` runs as root, behind polkit actions `org.templaros.host.*` and an
  AppArmor profile; apt runs in a child transient unit. Everything else is a user process with systemd
  sandboxing (`NoNewPrivileges`, `ProtectSystem=strict`, …).
* Identity is never trusted from a client's self-declared `app_id`; privileged actions require
  `verified-launch` confidence.
* Third-party driver repositories and NVIDIA's standalone installer are unsupported and pinned out
  (`distro/apt/preferences.d/templaros-nvidia`); `templar doctor` reports them.
* Secure Boot posture is decided in `docs/adr/0004-boot-deploy-secure-boot.md`.
