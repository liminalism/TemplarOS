//! Proxies for `lege-hostd` (`org.legeos.Host`, **system bus**): updates, deployments,
//! drivers, storage reserve, the hardware profile, and the session health report.
//!
//! Kept in sync with `interfaces/dbus/org.legeos.Host.*.xml`
//! (`cargo xtask check-interfaces`).

use zbus::proxy;

#[proxy(
    interface = "org.legeos.Host.Hardware1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostHardware {
    /// `GraphicsProfile` JSON (`crates/lege-hwdetect`).
    fn graphics_profile(&self) -> zbus::Result<String>;

    /// Declared GPU mode (`hybrid`/`integrated`/`discrete`), or an empty string if unset.
    fn gpu_mode(&self) -> zbus::Result<String>;

    /// Set the GPU mode; rejected if unsupported by the detected hardware.
    fn set_gpu_mode(&self, mode: &str) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.legeos.Host.Drivers1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostDrivers {
    /// `DriverPlan` JSON: metapackage, default mode, support violations.
    fn plan(&self) -> zbus::Result<String>;

    /// Just the support violations, as JSON (`Vec<SupportViolation>`).
    fn violations(&self) -> zbus::Result<String>;

    /// Accept the currently-installed NVIDIA versions as the new locked baseline; returns the
    /// written `VersionLock` as JSON.
    fn repair(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.legeos.Host.Updates1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostUpdates {
    /// The current transaction as JSON, or an empty string when idle.
    fn current(&self) -> zbus::Result<String>;

    /// Plan a transaction from a JSON change set; returns the transaction ID.
    fn plan(&self, changes_json: &str) -> zbus::Result<String>;

    /// Every transaction the journal remembers, as JSON (`Vec<Transaction>`).
    fn history(&self) -> zbus::Result<String>;

    /// One past (or current) transaction by id, as JSON, or an empty string if unknown.
    fn transaction(&self, id: &str) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.legeos.Host.Deployments1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostDeployments {
    /// Boot-manager backend in use (`systemd-boot` or `grub`).
    fn backend(&self) -> zbus::Result<String>;

    /// Known deployments as JSON.
    fn list(&self) -> zbus::Result<String>;

    /// Boot `id` next, at most `tries` times, before falling back.
    fn stage(&self, id: &str, tries: u8) -> zbus::Result<()>;

    /// Bless the booted deployment.
    fn mark_good(&self) -> zbus::Result<()>;

    /// Return to `id`.
    fn rollback(&self, id: &str) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.legeos.Host.Storage1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostStorage {
    /// Cache directories to trim, most valuable to reclaim first.
    fn trim_order(&self) -> zbus::Result<Vec<String>>;

    /// `VolumeUsage` JSON (total/free/reserved bytes) for the volume mounted at `mount_point`.
    fn usage(&self, mount_point: &str) -> zbus::Result<String>;

    /// Run the trim order against `mount_point` until healthy or exhausted; returns what ran,
    /// as JSON (`Vec<TrimReport>`).
    fn enforce(&self, mount_point: &str) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.legeos.Host.Session1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostSession {
    /// Report that the compositor's Wayland socket is accepting connections. Not polkit-gated —
    /// hostd verifies the caller is `lege-sessiond.service` by cgroup, not by admin privilege
    /// (`crates/lege-hostd/src/session_report.rs`), so calling this from anything else fails.
    fn report_compositor_active(&self) -> zbus::Result<()>;

    /// Report that the session's own startup sequence finished and a client frame was observed.
    fn report_session_healthy(&self) -> zbus::Result<()>;
}
