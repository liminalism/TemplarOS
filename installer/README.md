# installer/ — the installation image's installer (later stage)

Decision deferred (ADR to come): Calamares (Lubuntu lineage) vs subiquity/`ubuntu-desktop-provision`
(autoinstall). Requirements either way: use `lege-hwdetect` to select exactly one NVIDIA flavour and the
hardware profile; write the btrfs deployment layout (`distro/deploy/`); enrol the MOK if ADR-0004 picks
LegeOS-signed systemd-boot; write `os-release` from `distro/branding`; hand first-run to sessiond's
migration module (imports `mimeapps.list`/autostart/dconf with a conflict page in Settings). No separate
OOBE application.
