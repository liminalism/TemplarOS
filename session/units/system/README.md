# System units

* `templar-hostd.service` — the privileged host daemon (system bus `org.templaros.Host`, polkit-gated).
  `RuntimeDirectory=templaros` makes `/run/templaros/` hostd-owned; that is also where
  `templar-boot-health`'s two marker-file checks live (below), since a root-owned runtime directory
  cannot be written by the unprivileged session.
* `templar-boot-health.service` — **system scope** (a user unit cannot satisfy `boot-complete.target`).
  Six bounded checks (`services/templar-boot-health/src/main.rs`): DRM connected output, NVIDIA
  module loaded and version-matched to the installed userspace, DRM modesetting initialised, a
  Vulkan/OpenGL probe (`vulkaninfo`/`eglinfo`, degrading to a reported `Skipped` rather than a
  silent pass when neither tool exists), and two marker-file reads —
  `/run/templaros/compositor-active` and `/run/templaros/session-healthy`. Both markers are **written by
  hostd**, through `org.templaros.Host.Session1` (`interfaces/dbus/org.templaros.Host.Session1.xml`,
  `crates/templar-hostd/src/{api,session_report}.rs`): `ReportCompositorActive`/
  `ReportSessionHealthy`, gated by caller identity (the calling process's cgroup must name
  `templar-sessiond.service`) rather than polkit, since this is a report from an unprivileged process
  on every boot, not an administrative action.

  **The interface exists; nothing calls it yet.** Wiring `templar-sessiond` to call
  `ReportCompositorActive` once the compositor's Wayland socket accepts connections, and
  `ReportSessionHealthy` once the session's own startup finished and a client frame was observed,
  is the session-daemon track's work. Until it lands, `templar-boot-health` treats an absent marker
  as `Skipped` (`SESSION_REPORT_WIRED = false` in `services/templar-boot-health/src/main.rs`), not
  `Fail` — flipping that constant to `true` once the caller is wired makes an absent marker mean
  "the session did not report healthy" again. Either way, only a boot that is actually trialling a
  candidate deployment (`Updates1.Current().phase == "trial"`) can have its checks force a
  reboot — an ordinary boot always exits 0 and reports, never reboots, no matter what it finds.
  Success lets `systemd-bless-boot` mark the boot entry good; failure on a trial boot, plus
  `FailureAction=reboot-force`, lets the boot counter roll back (ADR-0004).
