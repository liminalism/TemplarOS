//! Proxies for `lege-hostd` (`org.legeos.Host`, **system bus**): updates, deployments,
//! drivers, storage reserve and the hardware profile.
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
}

#[proxy(
    interface = "org.legeos.Host.Drivers1",
    default_service = "org.legeos.Host",
    default_path = "/org/legeos/Host"
)]
pub trait HostDrivers {
    /// `DriverPlan` JSON: metapackage, default mode, support violations.
    fn plan(&self) -> zbus::Result<String>;
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
}
