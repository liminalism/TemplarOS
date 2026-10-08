//! Independent Desktop staging shelf. Filed owns discovery, previews and every file mutation.
//! The controller only remembers order and viewport; nothing here observes garden activity.
use crate::file_browser::{BrowseEntry, DirectoryListing, EntryKind};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashMap,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};
use templar_client::{
    file_index::{decode_path, encode_path},
    proxies::{FilesBrowseProxy, FilesIndexProxy, FilesOperationsProxy, FilesPreviewsProxy},
};

#[derive(Clone, Debug, Deserialize)]
pub struct DockEntry {
    pub path_b64: String,
    pub name: String,
    pub kind: EntryKind,
    pub is_symlink: bool,
    pub hidden: bool,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
}
impl DockEntry {
    pub fn path(&self) -> Result<PathBuf> {
        native_path(&self.path_b64)
    }
    fn identity(&self) -> Option<(u64, u64)> {
        self.dev.zip(self.ino)
    }
    pub fn browse_entry(&self) -> Result<BrowseEntry> {
        Ok(BrowseEntry {
            path: self.path()?,
            name: self.name.clone(),
            kind: self.kind,
            is_symlink: self.is_symlink,
            hidden: self.hidden,
            size_bytes: self.size_bytes,
            mtime_ms: self.mtime_ms,
        })
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct NativeListing {
    pub directory_b64: String,
    pub watch_directory_b64: String,
    pub watch_dev: u64,
    pub entries: Vec<DockEntry>,
    pub truncated: bool,
    pub limit: usize,
}
#[derive(Clone, Debug)]
pub struct DesktopDock {
    pub path: PathBuf,
    pub entries: Vec<DockEntry>,
    pub scroll: usize,
    pub truncated: bool,
    order: Vec<String>,
}
impl DesktopDock {
    pub fn new(path: PathBuf, order: Vec<String>) -> Self {
        Self {
            path,
            entries: Vec::new(),
            scroll: 0,
            truncated: false,
            order,
        }
    }
    /// Retain the user's existing slots; a renamed inode keeps its place. New files append.
    /// Inodes are only advisory within one live listing, never privilege or durable identity.
    pub fn apply_listing(&mut self, listing: NativeListing) -> Result<bool> {
        ensure!(
            native_path(&listing.directory_b64)? == self.path,
            "Desktop listing belongs to a different directory"
        );
        for entry in &listing.entries {
            ensure!(
                entry.path()?.parent() == Some(self.path.as_path()),
                "Desktop listing contains a non-child path"
            );
        }
        Ok(self.arrange_entries(listing.entries, listing.truncated))
    }

    /// A field Undo/order update rearranges an already-authoritative listing; it must not invent
    /// new filesystem or watch metadata just to change the presentation order.
    pub fn apply_saved_order(&mut self, order: Vec<String>) -> Result<bool> {
        if self.order == order {
            return Ok(false);
        }
        self.order = order;
        if !self.entries.is_empty() {
            let entries = std::mem::take(&mut self.entries);
            self.arrange_entries(entries, self.truncated);
        }
        Ok(true)
    }

    fn arrange_entries(&mut self, mut remaining: Vec<DockEntry>, truncated: bool) -> bool {
        let previous: HashMap<_, _> = self
            .entries
            .iter()
            .map(|entry| (entry.path_b64.clone(), entry.identity()))
            .collect();
        let mut ordered = Vec::with_capacity(remaining.len());
        let mut retained = truncated.then(|| self.order.clone());
        for (slot, old) in self.order.iter().enumerate() {
            let index = remaining
                .iter()
                .position(|entry| &entry.path_b64 == old)
                .or_else(|| {
                    let identity = previous.get(old).copied().flatten()?;
                    // Hardlinks cannot supply an unambiguous rename identity.
                    let mut matches = remaining
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| entry.identity() == Some(identity));
                    let first = matches.next()?.0;
                    matches.next().is_none().then_some(first)
                });
            if let Some(index) = index {
                let entry = remaining.remove(index);
                if let Some(retained) = &mut retained {
                    retained[slot].clone_from(&entry.path_b64);
                }
                ordered.push(entry);
            }
        }
        ordered.extend(remaining);
        let order = if let Some(mut retained) = retained {
            // An omitted row is not a deletion and retains its original slot, not a new tail slot.
            for entry in &ordered {
                if !retained.contains(&entry.path_b64) {
                    retained.push(entry.path_b64.clone());
                }
            }
            retained
        } else {
            ordered
                .iter()
                .map(|entry| entry.path_b64.clone())
                .collect::<Vec<_>>()
        };
        let changed = order != self.order;
        self.order = order;
        self.entries = ordered;
        self.truncated = truncated;
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(1));
        changed
    }
    pub fn reorder(&mut self, path: &Path, index: usize) -> bool {
        let key = encode_path(path);
        let Some(old) = self.entries.iter().position(|entry| entry.path_b64 == key) else {
            return false;
        };
        let index = index.min(self.entries.len().saturating_sub(1));
        if old == index {
            return false;
        }
        let entry = self.entries.remove(old);
        self.entries.insert(index, entry);
        if self.truncated {
            let mut visible = self.entries.iter();
            for saved in &mut self.order {
                if self.entries.iter().any(|entry| &entry.path_b64 == saved)
                    && let Some(entry) = visible.next()
                {
                    saved.clone_from(&entry.path_b64);
                }
            }
            for entry in visible {
                if !self.order.contains(&entry.path_b64) {
                    self.order.push(entry.path_b64.clone());
                }
            }
        } else {
            self.order = self
                .entries
                .iter()
                .map(|entry| entry.path_b64.clone())
                .collect();
        }
        true
    }
    pub fn order(&self) -> Vec<String> {
        self.order.clone()
    }
    pub fn scroll_by(&mut self, rows: isize, visible: usize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(rows)
            .min(self.entries.len().saturating_sub(visible));
    }
    pub fn visible(&self, count: usize) -> &[DockEntry] {
        &self.entries[self.scroll.min(self.entries.len())
            ..self.scroll.saturating_add(count).min(self.entries.len())]
    }
    /// Ordered adapter for Home's existing filed row/icon/preview renderer.
    pub fn browse_listing(&self) -> Result<DirectoryListing> {
        Ok(DirectoryListing {
            directory: self.path.clone(),
            parent: self.path.parent().map(Path::to_path_buf),
            entries: self
                .entries
                .iter()
                .map(DockEntry::browse_entry)
                .collect::<Result<_>>()?,
            truncated: self.truncated,
            limit: 500,
        })
    }
}
fn native_path(encoded: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decode_path(
        encoded,
    )?)))
}

pub async fn desktop_directory() -> Result<Option<PathBuf>> {
    let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
    let encoded = FilesBrowseProxy::new(&connection)
        .await?
        .desktop_directory_v1()
        .await?;
    if encoded.is_empty() {
        return Ok(None);
    }
    Ok(Some(native_path(&encoded)?))
}
/// Subscribe before listing. The existing filed kernel change feed is the only wake source:
/// no GUI directory walks, timer or polling. Owner changes invalidate and rebind the observer.
pub async fn watch_directory(
    path: PathBuf,
    emit: impl Fn(Result<NativeListing, String>) + Send + Sync,
) {
    use futures_util::StreamExt;
    let result = async {
        let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        let mut owners = bus
            .receive_name_owner_changed_with_args(&[(0, "org.templaros.Files")])
            .await?;
        loop {
            tokio::select! {
                result = watch_owner(&connection, &path, &emit) => {
                    if let Err(error) = result { emit(Err(error.to_string())); }
                    ensure!(owners.next().await.is_some(), "file service owner stream ended");
                }
                owner = owners.next() => {
                    ensure!(owner.is_some(), "file service owner stream ended");
                    emit(Err("Desktop file service restarted; refreshing its directory".into()));
                }
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        emit(Err(error.to_string()));
    }
}
static NEXT_OBSERVATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0x4445_534b_544f_5001);

async fn watch_owner(
    connection: &zbus::Connection,
    path: &Path,
    emit: &impl Fn(Result<NativeListing, String>),
) -> Result<()> {
    use futures_util::StreamExt;
    let browse = FilesBrowseProxy::new(connection).await?;
    let index = FilesIndexProxy::new(connection).await?;
    let mut changes = index.receive_generation_advanced().await?;
    let mut roots_changed = index.receive_root_state_changed().await?;
    let observation = NEXT_OBSERVATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    index.observe(true, observation).await?;
    let _observation = Observation {
        connection: connection.clone(),
        token: observation,
    };
    let encoded = encode_path(path);
    let mut covered: Option<(String, u64)> = None;
    loop {
        let snapshot = async {
            let listing: NativeListing = serde_json::from_str(
                &browse
                    .list_directory_native_v1(&encoded, false, 500)
                    .await?,
            )?;
            if covered.as_ref().is_none_or(|(path, dev)| {
                path != &listing.watch_directory_b64 || *dev != listing.watch_dev
            }) {
                ensure_directory_coverage(
                    &index,
                    &native_path(&listing.watch_directory_b64)?,
                    listing.watch_dev,
                )
                .await?;
                covered = Some((listing.watch_directory_b64.clone(), listing.watch_dev));
            }
            Ok::<_, anyhow::Error>(listing)
        }
        .await;
        emit(snapshot.map_err(|error| error.to_string()));
        tokio::select! {
            signal = changes.next() => ensure!(signal.is_some(), "Desktop change-feed stream ended"),
            signal = roots_changed.next() => {
                ensure!(signal.is_some(), "Desktop freshness stream ended");
                covered = None;
            },
        }
    }
}

/// Register only if an existing monitored policy really descends into this resolved directory,
/// including its device boundary. This is derived index policy, never garden evidence.
async fn ensure_directory_coverage(
    index: &FilesIndexProxy<'_>,
    path: &Path,
    device: u64,
) -> Result<()> {
    let roots: serde_json::Value = serde_json::from_str(&index.roots().await?)?;
    let mut exact_root_exists = false;
    if let Some(roots) = roots.get("roots").and_then(serde_json::Value::as_array) {
        for root in roots {
            let Some(encoded) = root.get("path_b64").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let native = native_path(encoded)?;
            if path == native {
                exact_root_exists = true;
            }
            if !path.starts_with(&native) {
                continue;
            }
            let hash = root
                .get("policy_hash")
                .and_then(serde_json::Value::as_str)
                .context("Index root has no policy hash")?;
            let policy: templar_file_index::RootPolicy =
                serde_json::from_str(&index.root_policy(encoded, hash).await?)?;
            let root_device = root
                .get("volume")
                .and_then(|volume| volume.get("dev_id"))
                .and_then(serde_json::Value::as_u64);
            let same_device = policy_device_matches(path, &native, root_device, device);
            if policy.monitoring
                && (policy.cross_mount || same_device)
                && policy
                    .stored_coverage_within(
                        &templar_file_index::NativePath::from_path(&native),
                        &templar_file_index::NativePath::from_path(path),
                        templar_file_index::EntryKind::Directory,
                    )
                    .is_some_and(templar_file_index::Coverage::materialises_children)
            {
                return Ok(());
            }
        }
    }
    ensure!(
        !exact_root_exists,
        "Desktop live updates are unavailable under its Files indexing policy"
    );
    let policy = serde_json::to_string(&templar_file_index::RootPolicy::default())?;
    index
        .configure_root(&encode_path(path), &policy, "absent")
        .await?;
    Ok(())
}

// An exact resolved root crosses no device boundary. Optional volume metadata
// cannot make its own directory a different filesystem (e.g. a tmpfs root).
fn policy_device_matches(path: &Path, root: &Path, root_device: Option<u64>, device: u64) -> bool {
    path == root || root_device == Some(device)
}

struct Observation {
    connection: zbus::Connection,
    token: u64,
}
impl Drop for Observation {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        let token = self.token;
        // Cancellation must release only this token, never a restarted watcher's newer token.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Ok(proxy) = FilesIndexProxy::new(&connection).await {
                    let _ = proxy.observe(false, token).await;
                }
            });
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferMode {
    Copy,
    Move,
}
/// Journalled all-source transfer. Default collision policy is Fail: no overwrites, silent
/// omissions or reinterpretation of remote URIs. UI may explicitly choose KeepBoth and retry.
pub async fn transfer(
    paths: Vec<PathBuf>,
    destination: PathBuf,
    mode: TransferMode,
    conflict: templar_client::files::ConflictPolicy,
    started: impl FnOnce(&str),
) -> Result<templar_client::files::OperationRecord> {
    ensure!(!paths.is_empty(), "No files were dropped");
    ensure!(
        paths.iter().all(|path| path.is_absolute()) && destination.is_absolute(),
        "File transfer needs absolute native paths"
    );
    let request = match mode {
        TransferMode::Copy => templar_client::files::OperationRequest::Copy {
            sources: paths,
            destination_dir: destination,
        },
        TransferMode::Move => templar_client::files::OperationRequest::Move {
            sources: paths,
            destination_dir: destination,
        },
    };
    let options = templar_client::files::OperationOptions {
        conflict,
        verify: templar_client::files::Verification::Checksum,
        override_reserve: false,
    };
    let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
    let proxy = FilesOperationsProxy::new(&connection).await?;
    let id = proxy
        .start(
            &serde_json::to_string(&request)?,
            &serde_json::to_string(&options)?,
            crate::data::SHELL_APP_ID,
        )
        .await?;
    started(&id);
    crate::data::follow_operation(&proxy, &id).await
}
pub async fn undo(operation_id: &str) -> Result<String> {
    let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
    let proxy = FilesOperationsProxy::new(&connection).await?;
    let (allowed, reason) = proxy.can_undo(operation_id).await?;
    ensure!(allowed, "{reason}");
    Ok(proxy.undo(operation_id).await?)
}
pub async fn preview(path: &Path, max_edge: u32) -> Result<Vec<u8>> {
    let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
    Ok(FilesPreviewsProxy::new(&connection)
        .await?
        .render_native_v1(&encode_path(path), max_edge)
        .await?
        .1)
}
/// Double-click and Open with use Associations/Launcher, carrying native bytes in a file URI.
pub async fn open(path: &Path, project_id: &str, app_override: Option<&str>) -> Result<String> {
    use templar_client::proxies::{AssociationsProxy, LauncherProxy};
    let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
    let uri = templar_types::uri_list::encode_path(path);
    let (app, _, reason) = AssociationsProxy::new(&connection)
        .await?
        .resolve_file_uri_v1(&uri, project_id)
        .await?;
    let app = app_override.unwrap_or(&app);
    ensure!(
        !app.is_empty(),
        "No application can open this file: {reason}"
    );
    Ok(LauncherProxy::new(&connection)
        .await?
        .launch_files_v1(app, vec![uri], "")
        .await?)
}
pub async fn open_with_handlers(path: &Path, project_id: &str) -> Result<Vec<String>> {
    let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
    let proxy = templar_client::proxies::AssociationsProxy::new(&connection).await?;
    let (_, mime, _) = proxy
        .resolve_file_uri_v1(&templar_types::uri_list::encode_path(path), project_id)
        .await?;
    Ok(proxy.handlers(&mime).await?)
}
/// Folder presentation is an explicit action, never a visit emitted by dock drawing.
pub fn show_in_folder(path: &Path) -> Result<PathBuf> {
    path.parent()
        .map(Path::to_path_buf)
        .context("File has no parent directory")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_root_needs_no_volume_metadata_but_ancestor_still_needs_device_evidence() {
        let desktop = Path::new("/home/u/Desktop");
        let home = Path::new("/home/u");
        assert!(policy_device_matches(desktop, desktop, None, 27));
        assert!(!policy_device_matches(desktop, home, None, 27));
        assert!(!policy_device_matches(desktop, home, Some(28), 27));
        assert!(policy_device_matches(desktop, home, Some(27), 27));
    }
    fn item(path: &Path, inode: u64) -> DockEntry {
        DockEntry {
            path_b64: encode_path(path),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            kind: EntryKind::File,
            is_symlink: false,
            hidden: false,
            size_bytes: 1,
            mtime_ms: 0,
            dev: Some(1),
            ino: Some(inode),
        }
    }
    fn listing(entries: Vec<DockEntry>) -> NativeListing {
        NativeListing {
            directory_b64: encode_path(Path::new("/Desktop")),
            watch_directory_b64: encode_path(Path::new("/Desktop")),
            watch_dev: 1,
            entries,
            truncated: false,
            limit: 500,
        }
    }
    #[test]
    fn stable_order_appends_new_names_and_preserves_rename_inode_slot() {
        let a = item(Path::new("/Desktop/a"), 1);
        let b = item(Path::new("/Desktop/b"), 2);
        let c = item(Path::new("/Desktop/c"), 3);
        let mut dock = DesktopDock::new(
            "/Desktop".into(),
            vec![b.path_b64.clone(), a.path_b64.clone()],
        );
        dock.apply_listing(listing(vec![a.clone(), b.clone()]))
            .unwrap();
        dock.apply_listing(listing(vec![
            a,
            c.clone(),
            item(Path::new("/Desktop/z"), 2),
        ]))
        .unwrap();
        assert_eq!(
            dock.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["z", "a", "c"]
        );
        assert!(dock.reorder(Path::new("/Desktop/c"), 0));
        assert_eq!(dock.entries[0].name, "c");
    }
    #[test]
    fn raw_paths_remain_distinct_despite_same_display_name_and_reordering() {
        let a = PathBuf::from(std::ffi::OsString::from_vec(b"/Desktop/a-\xff".to_vec()));
        let b = PathBuf::from(std::ffi::OsString::from_vec(b"/Desktop/a-\xfe".to_vec()));
        let mut dock = DesktopDock::new("/Desktop".into(), Vec::new());
        dock.apply_listing(listing(vec![item(&a, 1), item(&b, 2)]))
            .unwrap();
        assert_eq!(dock.entries[0].name, dock.entries[1].name);
        dock.reorder(&b, 0);
        assert_eq!(dock.entries[0].path().unwrap(), b);
        assert_eq!(dock.browse_listing().unwrap().entries[1].path, a);
    }
    #[test]
    fn truncated_snapshot_and_user_reorder_preserve_unseen_saved_slots() {
        let a = item(Path::new("/Desktop/a"), 1);
        let b = item(Path::new("/Desktop/b"), 2);
        let c = item(Path::new("/Desktop/c"), 3);
        let mut dock = DesktopDock::new(
            "/Desktop".into(),
            vec![c.path_b64.clone(), a.path_b64.clone(), b.path_b64.clone()],
        );
        dock.apply_listing(listing(vec![a.clone(), b.clone(), c.clone()]))
            .unwrap();
        let mut truncated = listing(vec![a.clone(), b.clone()]);
        truncated.truncated = true;
        dock.apply_listing(truncated).unwrap();
        assert_eq!(
            dock.order(),
            vec![c.path_b64.clone(), a.path_b64.clone(), b.path_b64.clone()]
        );
        dock.reorder(Path::new("/Desktop/b"), 0);
        assert_eq!(
            dock.order(),
            vec![c.path_b64.clone(), b.path_b64.clone(), a.path_b64.clone()]
        );
        dock.apply_listing(listing(vec![a.clone(), b.clone(), c.clone()]))
            .unwrap();
        assert_eq!(
            dock.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["c", "b", "a"]
        );
        dock.apply_saved_order(vec![a.path_b64, c.path_b64, b.path_b64])
            .unwrap();
        assert_eq!(
            dock.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
    }
}
