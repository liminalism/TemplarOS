//! Proxies for `lege-filed` (`org.legeos.Files`): volumes and the storage reserve, journalled
//! operations with progress and undo, recents and last-used directories, search, and the small
//! path actions.
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

    /// Every user-visible volume as a JSON array.
    fn list(&self) -> zbus::Result<String>;

    /// `(verdict, volume_json)`; verdict is `ok`, `warn` or `refuse`.
    fn check_reserve(&self, path: &str, pending_bytes: u64) -> zbus::Result<(String, String)>;
}

#[proxy(
    interface = "org.legeos.Files.Operations1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesOperations {
    /// Start a journalled operation; returns its ID immediately.
    fn start(&self, kind_json: &str, options_json: &str, app_id: &str) -> zbus::Result<String>;

    /// Stop at the next step boundary.
    fn cancel(&self, op_id: &str) -> zbus::Result<()>;

    /// The full journalled record, including the resume cursor.
    fn get(&self, op_id: &str) -> zbus::Result<String>;

    /// Newest first.
    fn recent(&self, limit: u32) -> zbus::Result<String>;

    /// `(can_undo, reason)` — the reason is what a UI shows when it cannot.
    fn can_undo(&self, op_id: &str) -> zbus::Result<(bool, String)>;

    /// Reverse a committed operation; returns the ID of the operation that does it.
    fn undo(&self, op_id: &str) -> zbus::Result<String>;

    #[zbus(signal)]
    fn progress(
        &self,
        op_id: String,
        state: String,
        done_bytes: u64,
        total_bytes: u64,
        done_items: u64,
        total_items: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn finished(&self, op_id: String, state: String, error: String) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.legeos.Files.Recents1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesRecents {
    fn record(&self, path: &str, app_id: &str, action: &str, mime: &str) -> zbus::Result<()>;

    fn list(&self, app_id: &str, limit: u32) -> zbus::Result<String>;

    fn forget(&self, path: &str) -> zbus::Result<()>;

    fn prune_missing(&self) -> zbus::Result<u32>;

    /// The one answer to "which folder should this chooser open at", with its reason.
    fn suggest_directory(&self, app_id: &str, operation: &str) -> zbus::Result<String>;

    fn record_directory(&self, app_id: &str, operation: &str, directory: &str) -> zbus::Result<()>;

    fn set_project(&self, project_id: &str, root: &str) -> zbus::Result<()>;

    fn clear_project(&self, project_id: &str) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.legeos.Files.Search1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesSearch {
    fn index_root(&self, root: &str) -> zbus::Result<u32>;

    fn roots(&self) -> zbus::Result<Vec<String>>;

    fn forget_root(&self, root: &str) -> zbus::Result<()>;

    /// Hits as JSON, best match first.
    fn search(&self, query: &str, roots: Vec<String>, limit: u32) -> zbus::Result<String>;

    fn save_query(&self, name: &str, query: &str, roots: Vec<String>) -> zbus::Result<()>;

    fn saved_queries(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.legeos.Files.Actions1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesActions {
    /// `canonical`, `as-shown`, `uri`, `shell-quoted`, `parent-directory` or `name`.
    fn render(&self, path: &str, form: &str) -> zbus::Result<String>;

    fn forms(&self) -> zbus::Result<Vec<String>>;

    /// Where "open a terminal here" should start.
    fn terminal_directory(&self, path: &str) -> zbus::Result<String>;
}
