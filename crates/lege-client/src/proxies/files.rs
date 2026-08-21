//! Proxies for `lege-filed` (`org.legeos.Files`): volumes and the storage reserve, journalled
//! operations with progress and undo, recents and last-used directories, search.
//!
//! Kept in sync with `interfaces/dbus/org.legeos.Files.*.xml`
//! (`cargo xtask check-interfaces`).

use zbus::proxy;

#[proxy(
    interface = "org.legeos.Files.Volumes1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesVolumes {
    /// `VolumeInfo` JSON for the volume containing `path`.
    fn info_for_path(&self, path: &str) -> zbus::Result<String>;
}
