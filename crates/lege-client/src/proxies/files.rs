//! Proxies for `lege-filed` (`org.legeos.Files`): volumes and the storage reserve, journalled
//! operations with progress and undo, recents and last-used directories, search, and the small
//! path actions.
//!
//! Kept in sync with `interfaces/dbus/org.legeos.Files.*.xml`
//! (`cargo xtask check-interfaces`).

use zbus::proxy;

#[proxy(
    interface = "org.legeos.Files.Browse1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesBrowse {
    /// A `BrowserSources` JSON snapshot: XDG places and service-approved drives.
    fn roots(&self) -> zbus::Result<String>;

    /// A `DirectoryListing` JSON snapshot: immediate children, folder-first natural order.
    fn list_directory(
        &self,
        directory: &str,
        show_hidden: bool,
        limit: u32,
    ) -> zbus::Result<String>;

    /// `FileInfo` JSON for a properties pane, including recursive totals for real directories.
    fn inspect(&self, path: &str) -> zbus::Result<String>;

    /// Make a directory under `parent`; returns the name it actually got, which differs from
    /// `preferred_name` when that one was taken.
    fn create_directory(&self, parent: &str, preferred_name: &str) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.legeos.Files.Previews1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesPreviews {
    /// `(PreviewMetadata JSON, bounded PNG bytes)`; both are empty for unsupported files.
    fn render(&self, path: &str, max_edge: u32) -> zbus::Result<(String, Vec<u8>)>;
}

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
    interface = "org.legeos.Files.Trash1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesTrash {
    /// Every trash this user has — the home trash and each mounted volume's — newest deletion
    /// first, as a JSON array of `TrashEntry`.
    fn list(&self) -> zbus::Result<String>;

    /// Put trashed items back where they came from; returns the operation ID.
    fn restore(&self, trashed_paths: Vec<String>, app_id: &str) -> zbus::Result<String>;

    /// Remove trashed items for good, with their `.trashinfo` records. No undo.
    fn delete_permanently(&self, trashed_paths: Vec<String>, app_id: &str) -> zbus::Result<String>;

    /// Empty every trash. The operation ID is empty when there was nothing to empty.
    fn empty(&self, app_id: &str) -> zbus::Result<String>;

    /// Something was trashed, restored or deleted for good. Carries nothing — call `list`.
    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
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
    interface = "org.legeos.Files.Index1",
    default_service = "org.legeos.Files",
    default_path = "/org/legeos/Files"
)]
pub trait FilesIndex {
    /// Every registered root's `RootStatus` plus the core's own `Diagnostics`, as JSON. Native
    /// path bytes are base64 in a `path_b64` field beside a lossy `display` string.
    fn roots(&self) -> zbus::Result<String>;

    /// One JSON `Query` argument, so filters/sort/field-mask can grow without a signature
    /// change. `(cursor, generation, page_json)`.
    fn search(&self, query_json: &str) -> zbus::Result<(String, u64, String)>;

    /// Random access inside a pinned result set; the cursor's own generation, never a newer one.
    /// `Index1.Error.SnapshotRequired` when the cursor aged out.
    fn fetch_page(&self, cursor: &str, offset: u32, count: u32) -> zbus::Result<String>;

    /// Best effort; a client that vanishes is covered by idle expiry regardless.
    fn close_cursor(&self, cursor: &str) -> zbus::Result<()>;

    /// A sealed `LEGEIDX1` bulk snapshot: `(fd, manifest_json)`.
    fn open_snapshot(
        &self,
        request_json: &str,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, String)>;

    /// A bounded page of the change log after `seq`. `Index1.Error.SnapshotRequired` below the
    /// retained floor.
    fn changes_since(&self, seq: u64, limit: u32) -> zbus::Result<String>;

    /// Schedules a background repair of the given roots (a JSON array of root ids; empty means
    /// every registered root) and returns an opaque request id immediately. Progress is observed
    /// through `roots()` and the signals below.
    fn reconcile(&self, roots_json: &str) -> zbus::Result<String>;

    /// A generation committed.
    #[zbus(signal)]
    fn generation_advanced(&self, generation: u64, change_count: u32) -> zbus::Result<()>;

    /// A root's freshness state changed.
    #[zbus(signal)]
    fn root_state_changed(&self, root_id: i64, state: String, reason: String) -> zbus::Result<()>;
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
