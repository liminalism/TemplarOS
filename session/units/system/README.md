# System units

* `lege-hostd.service` — the privileged host daemon (system bus `org.legeos.Host`, polkit-gated).
* `lege-boot-health.service` — **system scope** (a user unit cannot satisfy `boot-complete.target`).
  Probes DRM/NVIDIA/systemd health, and (once the session-side reporter ships) requires
  `/run/legeos/session-healthy` written by hostd when `lege-sessiond` reports the compositor up and
  Vulkan/GL probes passed. Success lets `systemd-bless-boot` mark the boot entry good; failure plus
  `FailureAction=reboot-force` lets the boot counter roll back (ADR-0004).
