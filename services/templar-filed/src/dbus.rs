//! The D-Bus face of the file service: `org.templaros.Files`.
//!
//! Interfaces at one object path, one per thing a client actually wants — browsing, volumes and
//! the reserve, journalled operations, recents and last-used directories, and search. The file
//! manager, the Home Surface and the file-chooser portal are all clients of exactly these, so
//! they cannot hold different opinions about the same question.
//!
//! Operations run on a blocking thread and journal their own progress, so a slow copy never
//! occupies the bus and a crash mid-copy leaves a resumable record rather than a mystery.

use crate::{
    actions::{self, PathForm},
    browse,
    execute::{self, Cancel, ExecutorConfig},
    journal::Journal,
    lastdir::{LastDirectoryStore, OperationType},
    operations::{self, Operation, OperationId, OperationKind, OperationState, Options},
    preview,
    recents::{RecentAction, Recents},
    search::{Index, SavedQuery},
    storage, trash,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use templar_types::{AppId, dbus::files as names};
use zbus::fdo;

/// Everything the interfaces share.
#[derive(Clone)]
pub struct FileService {
    pub journal: Arc<dyn Journal>,
    pub config: ExecutorConfig,
    pub recents: Recents,
    pub index: Index,
    pub directories: LastDirectoryStore,
    pub reserve_percent: u64,
    running: Arc<Mutex<HashMap<OperationId, Cancel>>>,
    connection: Arc<Mutex<Option<zbus::Connection>>>,
}

impl std::fmt::Debug for FileService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileService")
            .field("reserve_percent", &self.reserve_percent)
            .field("running", &self.running.lock().len())
            .finish_non_exhaustive()
    }
}

impl FileService {
    pub fn new(
        journal: Arc<dyn Journal>,
        mut config: ExecutorConfig,
        recents: Recents,
        index: Index,
        directories: LastDirectoryStore,
        reserve_percent: u64,
    ) -> Self {
        config.journal = Some(Arc::clone(&journal));
        Self {
            journal,
            config,
            recents,
            index,
            directories,
            reserve_percent,
            running: Arc::default(),
            connection: Arc::default(),
        }
    }

    /// Give the service the connection it emits progress on. Signals are best-effort: an
    /// operation must never fail because nobody was listening.
    pub fn attach(&self, connection: zbus::Connection) {
        *self.connection.lock() = Some(connection);
    }

    /// Plan, journal and start `kind`. Returns as soon as the work is scheduled.
    pub fn start(
        &self,
        mut kind: OperationKind,
        options: Options,
        app_id: Option<&AppId>,
    ) -> anyhow::Result<OperationId> {
        if matches!(kind, OperationKind::RestoreMoveV1 { .. } | OperationKind::UndoCopyV1 { .. })
            && options.conflict != operations::ConflictPolicy::Fail
        {
            anyhow::bail!("an inverse must refuse conflicts rather than overwrite or rename original locations");
        }
        if let OperationKind::UndoCopyV1 { original_operation, roots } = &mut kind {
            let original = self.journal.get(*original_operation)?
                .ok_or_else(|| anyhow::anyhow!("original copy journal record is unavailable"))?;
            crate::transfer_undo::validate(&original)?;
            *roots = original.transfers.into_iter().map(|entry| entry.destination).collect();
        }
        operations::validate(&kind)?;
        let id = OperationId::new();
        let surveyed = match &kind {
            OperationKind::RestoreMoveV1 { entries } => {
                let mut combined = operations::Survey::default();
                for entry in entries {
                    let mut part = operations::survey(std::slice::from_ref(&entry.source), entry.destination.parent());
                    for source in &mut part.sources {
                        source.destination_exists = entry.destination.symlink_metadata().is_ok();
                    }
                    combined.sources.extend(part.sources);
                    combined.errors.extend(part.errors);
                }
                combined
            }
            _ => operations::survey(&sources_of(&kind), destination_of(&kind)),
        };
        if !surveyed.errors.is_empty() {
            anyhow::bail!("could not survey operation: {}", surveyed.errors.join("; "));
        }
        let operation = operations::plan(id, kind, &surveyed, options);
        self.journal.put(&operation, app_id)?;

        let cancel = Cancel::new();
        self.running.lock().insert(id, cancel.clone());
        self.spawn(operation, cancel);
        Ok(id)
    }

    fn spawn(&self, mut operation: Operation, cancel: Cancel) {
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let id = operation.id;
            let journal = Arc::clone(&service.journal);
            let emitter = service.clone();
            let outcome = execute::run(&mut operation, &service.config, &cancel, |progress| {
                // Durable first, then announced: a listener that hears about a step the journal
                // has not recorded would be told something a restart cannot confirm.
                if let Err(error) = journal.progress(progress) {
                    tracing::warn!(%error, %id, "could not journal progress");
                }
                emitter.emit_progress(progress);
            });

            if operation.state == OperationState::Committed
                && let Some(undo) = operations::undo_of(&operation)
                && let Err(error) = service.journal.put_undo(id, &undo)
            {
                tracing::warn!(%error, %id, "could not record the undo plan");
            }
            if let Err(error) = &outcome {
                tracing::warn!(%error, %id, "operation failed");
            }
            service.running.lock().remove(&id);
            service.emit_finished(&operation);
            if touches_trash(&operation.kind) {
                service.emit_trash_changed();
            }
        });
    }

    /// The trash's contents changed. Emitted from the one place operations finish, so a drawer
    /// and a file manager stay live whichever of them did the mutating — and so no caller has to
    /// remember to announce it.
    ///
    /// Deliberately generous: every permanent delete announces it, not only the ones aimed at a
    /// trash, because the cost of a client re-listing an unchanged trash is nothing and the cost
    /// of a missed change is a stale listing that offers to restore a file that is gone.
    fn emit_trash_changed(&self) {
        self.emit_on(names::TRASH_IFACE, "Changed", &());
    }

    fn emit_progress(&self, operation: &Operation) {
        self.emit(
            "Progress",
            &(
                operation.id.to_string(),
                operation.state.as_str().to_owned(),
                operation.done_bytes,
                operation.total_bytes,
                operation.done_items,
                operation.total_items,
            ),
        );
    }

    fn emit_finished(&self, operation: &Operation) {
        self.emit(
            "Finished",
            &(
                operation.id.to_string(),
                operation.state.as_str().to_owned(),
                operation.error.clone().unwrap_or_default(),
            ),
        );
    }

    fn emit<B>(&self, member: &'static str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + Clone + 'static,
    {
        self.emit_on(names::OPERATIONS_IFACE, member, body);
    }

    fn emit_on<B>(&self, interface: &'static str, member: &'static str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + Clone + 'static,
    {
        let Some(connection) = self.connection.lock().clone() else {
            return;
        };
        let body = body.clone();
        tokio::spawn(async move {
            if let Err(error) = connection
                .emit_signal(None::<&str>, names::ROOT_PATH, interface, member, &body)
                .await
            {
                tracing::debug!(%error, member, "no listener for the signal");
            }
        });
    }

    /// Resume everything the journal shows unfinished. Called once at start: the whole point of
    /// journalling is that this is possible.
    pub fn resume_unfinished(&self) -> anyhow::Result<usize> {
        let pending = self.journal.unfinished()?;
        for operation in &pending {
            let cancel = Cancel::new();
            self.running.lock().insert(operation.id, cancel.clone());
            self.spawn(operation.clone(), cancel);
        }
        Ok(pending.len())
    }
}

fn sources_of(kind: &OperationKind) -> Vec<PathBuf> {
    match kind {
        OperationKind::Copy { sources, .. } | OperationKind::Move { sources, .. } => {
            sources.clone()
        }
        OperationKind::Delete { paths } => paths.clone(),
        OperationKind::RestoreMoveV1 { entries } => entries.iter().map(|entry| entry.source.clone()).collect(),
        OperationKind::UndoCopyV1 { .. } => Vec::new(),
        // Trashing and restoring never traverse: they are renames, and surveying a large
        // directory before trashing it would make "delete" mysteriously slow.
        OperationKind::Trash { .. }
        | OperationKind::RestoreFromTrash { .. }
        | OperationKind::Rename { .. } => Vec::new(),
    }
}

/// Whether finishing this operation could have changed what is in a trash.
fn touches_trash(kind: &OperationKind) -> bool {
    matches!(
        kind,
        OperationKind::Trash { .. }
            | OperationKind::RestoreFromTrash { .. }
            | OperationKind::Delete { .. }
            | OperationKind::UndoCopyV1 { .. }
    )
}

fn destination_of(kind: &OperationKind) -> Option<&Path> {
    match kind {
        OperationKind::Copy {
            destination_dir, ..
        }
        | OperationKind::Move {
            destination_dir, ..
        } => Some(destination_dir),
        _ => None,
    }
}

pub(crate) fn failed(error: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(error.to_string())
}

pub(crate) fn json<T: serde::Serialize>(value: &T) -> fdo::Result<String> {
    serde_json::to_string(value).map_err(failed)
}

fn native_absolute_path(encoded: &str) -> fdo::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    templar_client::file_index::decode_path(encoded)
        .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes)))
        .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))
}

/// `org.templaros.Files.Volumes1` — storage information that must be unavoidable in every file UI.
#[derive(Debug)]
pub struct VolumesApi {
    pub reserve_percent: u64,
}

#[zbus::interface(name = "org.templaros.Files.Volumes1")]
impl VolumesApi {
    /// `VolumeInfo` JSON for the volume containing `path`.
    async fn info_for_path(&self, path: &str) -> fdo::Result<String> {
        let info = storage::query(Path::new(path), self.reserve_percent).map_err(failed)?;
        json(&info)
    }

    /// Every user-visible volume, as a JSON array.
    async fn list(&self) -> fdo::Result<String> {
        json(&storage::volumes(self.reserve_percent).map_err(failed)?)
    }

    /// Whether writing `pending_bytes` under `path` is allowed: `ok`, `warn` or `refuse`, with
    /// the volume it was judged against.
    async fn check_reserve(&self, path: &str, pending_bytes: u64) -> fdo::Result<(String, String)> {
        let volume = storage::query(Path::new(path), self.reserve_percent).map_err(failed)?;
        let verdict = storage::check_reserve(&volume, pending_bytes);
        Ok((
            serde_json::to_value(verdict)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "ok".to_owned()),
            json(&volume)?,
        ))
    }
}

/// `org.templaros.Files.Browse1` — immediate, bounded directory snapshots. The blocking directory
/// read stays off the async executor, just like file operations do.
#[derive(Debug)]
pub struct BrowseApi {
    pub reserve_percent: u64,
    pub bookmarks: crate::bookmarks::Bookmarks,
}

#[zbus::interface(name = "org.templaros.Files.Browse1")]
impl BrowseApi {
    /// XDG places and every user-visible mounted drive, with volume data attached to drives.
    async fn roots(&self) -> fdo::Result<String> {
        let reserve_percent = self.reserve_percent;
        let bookmarks = self.bookmarks.clone();
        let sources = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut sources = browse::sources(reserve_percent)?;
            sources.roots.extend(bookmarks.roots()?);
            Ok(sources)
        })
        .await
        .map_err(failed)?
        .map_err(failed)?;
        json(&sources)
    }

    async fn set_bookmark(&self, path: &str, bookmarked: bool) -> fdo::Result<()> {
        let bookmarks = self.bookmarks.clone();
        let path = PathBuf::from(path);
        tokio::task::spawn_blocking(move || bookmarks.set(&path, bookmarked))
            .await
            .map_err(failed)?
            .map_err(failed)
    }

    /// Explicit additive interoperability, never an automatic consequence of SetBookmark.
    async fn export_bookmarks(&self) -> fdo::Result<String> {
        let bookmarks = self.bookmarks.clone();
        let result = tokio::task::spawn_blocking(move || bookmarks.export())
            .await.map_err(failed)?.map_err(failed)?;
        json(&result)
    }

    async fn list_directory(
        &self,
        directory: &str,
        show_hidden: bool,
        limit: u32,
    ) -> fdo::Result<String> {
        let directory = PathBuf::from(directory);
        let listing = tokio::task::spawn_blocking(move || {
            browse::list_directory(&directory, show_hidden, limit as usize)
        })
        .await
        .map_err(failed)?
        .map_err(failed)?;
        json(&listing)
    }
    /// Native Desktop authority; an XDG Desktop disabled by binding it to HOME is not a dock.
    async fn desktop_directory_v1(&self) -> fdo::Result<String> {
        tokio::task::spawn_blocking(|| -> anyhow::Result<String> {
            let home = std::env::var_os("HOME").map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("HOME is unavailable"))?;
            let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            let text = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
            let sources = browse::sources_from(Some(&home), Some(&text), Vec::new());
            Ok(sources.roots.iter().find(|root| root.key == "desktop")
                .map(|root| templar_client::file_index::encode_path(&root.path)).unwrap_or_default())
        }).await.map_err(failed)?.map_err(failed)
    }

    /// Lossless immediate listing; display names do not identify filesystem objects.
    async fn list_directory_native_v1(
        &self, directory_b64: &str, show_hidden: bool, limit: u32,
    ) -> fdo::Result<String> {
        let directory = native_absolute_path(directory_b64)?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            use std::os::unix::fs::MetadataExt;
            let listing = browse::list_directory(&directory, show_hidden, limit as usize)?;
            // The dock binds lexical XDG identity, while the index must watch the real
            // directory: its scanner deliberately does not descend through symlinks.
            let watched = std::fs::canonicalize(&directory)?;
            let watched_dev = std::fs::metadata(&watched)?.dev();
            let entries: Vec<_> = listing.entries.iter().map(|entry| {
                let identity = std::fs::symlink_metadata(&entry.path).ok();
                serde_json::json!({
                    "path_b64": templar_client::file_index::encode_path(&entry.path),
                    "name": entry.name, "kind": entry.kind, "is_symlink": entry.is_symlink,
                    "hidden": entry.hidden, "size_bytes": entry.size_bytes, "mtime_ms": entry.mtime_ms,
                    "dev": identity.as_ref().map(MetadataExt::dev),
                    "ino": identity.as_ref().map(MetadataExt::ino),
                })
            }).collect();
            Ok(serde_json::to_string(&serde_json::json!({
                "directory_b64": templar_client::file_index::encode_path(&listing.directory),
                "watch_directory_b64": templar_client::file_index::encode_path(&watched),
                "watch_dev": watched_dev,
                "entries": entries, "truncated": listing.truncated, "limit": listing.limit,
            }))?)
        }).await.map_err(failed)?.map_err(failed)
    }

    /// Make a directory under `parent`; returns the name it actually got.
    async fn create_directory(&self, parent: &str, preferred_name: &str) -> fdo::Result<String> {
        let parent = PathBuf::from(parent);
        let preferred = preferred_name.to_owned();
        tokio::task::spawn_blocking(move || browse::create_directory(&parent, &preferred))
            .await
            .map_err(failed)?
            .map_err(failed)
    }

    /// One metadata record, with symlink-safe recursive totals for a real directory.
    async fn inspect(&self, path: &str) -> fdo::Result<String> {
        let path = PathBuf::from(path);
        let info = tokio::task::spawn_blocking(move || browse::inspect(&path))
            .await
            .map_err(failed)?
            .map_err(failed)?;
        json(&info)
    }
}

/// `org.templaros.Files.Previews1` — bounded thumbnails rendered away from GUI and D-Bus threads.
#[derive(Debug, Default)]
pub struct PreviewsApi;

#[zbus::interface(name = "org.templaros.Files.Previews1")]
impl PreviewsApi {
    /// Return `(PreviewMetadata JSON, PNG bytes)`. Unsupported file types return two empty
    /// values; supported but malformed files and unavailable helpers are explicit errors.
    async fn render(&self, path: &str, max_edge: u32) -> fdo::Result<(String, Vec<u8>)> {
        let path = PathBuf::from(path);
        let rendered = tokio::task::spawn_blocking(move || preview::render(&path, max_edge))
            .await
            .map_err(failed)?
            .map_err(failed)?;
        match rendered {
            Some(rendered) => Ok((json(&rendered.metadata)?, rendered.png)),
            None => Ok((String::new(), Vec::new())),
        }
    }

    async fn render_native_v1(&self, path_b64: &str, max_edge: u32) -> fdo::Result<(String, Vec<u8>)> {
        let path = native_absolute_path(path_b64)?;
        let rendered = tokio::task::spawn_blocking(move || preview::render(&path, max_edge))
            .await.map_err(failed)?.map_err(failed)?;
        match rendered {
            Some(rendered) => Ok((json(&rendered.metadata)?, rendered.png)),
            None => Ok((String::new(), Vec::new())),
        }
    }
}

/// `org.templaros.Files.Operations1` — journalled transactions with progress and undo.
#[derive(Debug)]
pub struct OperationsApi(pub FileService);

#[zbus::interface(name = "org.templaros.Files.Operations1")]
impl OperationsApi {
    /// Start an operation. `kind_json` is an `OperationKind`, `options_json` an `Options`
    /// (an empty string means the defaults). Returns the operation ID.
    async fn start(
        &self,
        kind_json: &str,
        options_json: &str,
        app_id: &str,
    ) -> fdo::Result<String> {
        let kind: OperationKind = serde_json::from_str(kind_json).map_err(failed)?;
        let options: Options = if options_json.trim().is_empty() {
            Options::default()
        } else {
            serde_json::from_str(options_json).map_err(failed)?
        };
        let app_id = (!app_id.is_empty()).then(|| app_id.parse().ok()).flatten();
        let service = self.0.clone();
        tokio::task::spawn_blocking(move || service.start(kind, options, app_id.as_ref()))
            .await.map_err(failed)?.map(|id| id.to_string()).map_err(failed)
    }

    /// Ask a running operation to stop at its next step boundary. Whatever it has already
    /// exposed stays exposed — the journal explains what was done.
    async fn cancel(&self, op_id: &str) -> fdo::Result<()> {
        let id = OperationId::parse(op_id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad operation id {op_id}")))?;
        match self.0.running.lock().get(&id) {
            Some(cancel) => {
                cancel.cancel();
                Ok(())
            }
            None => Err(fdo::Error::Failed(format!("{op_id} is not running"))),
        }
    }

    /// The full journalled record of one operation.
    async fn get(&self, op_id: &str) -> fdo::Result<String> {
        let id = OperationId::parse(op_id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad operation id {op_id}")))?;
        let operation = self
            .0
            .journal
            .get(id)
            .map_err(failed)?
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no operation {op_id}")))?;
        json(&operation)
    }

    /// Recent operations, newest first — the Operations drawer.
    async fn recent(&self, limit: u32) -> fdo::Result<String> {
        json(&self.0.journal.recent(limit as usize).map_err(failed)?)
    }

    /// Whether this operation can still be undone, and if not, why not. A delete is the honest
    /// "no": the UI is expected to have said so before running it.
    async fn can_undo(&self, op_id: &str) -> fdo::Result<(bool, String)> {
        let id = OperationId::parse(op_id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad operation id {op_id}")))?;
        let Some(operation) = self.0.journal.get(id).map_err(failed)? else {
            return Ok((false, format!("no operation {op_id}")));
        };
        if !operation.kind.reversible() {
            return Ok((false, "this operation cannot be reversed".to_owned()));
        }
        if operation.state != OperationState::Committed {
            return Ok((
                false,
                format!("the operation is {}", operation.state.as_str()),
            ));
        }
        if self.0.journal.undo_of(id).map_err(failed)?.is_none() {
            return Ok((false, "no undo plan was recorded".to_owned()));
        }
        // Never execute a pre-cutover stored Delete inverse of Copy, or guess original Move
        // names. Availability is stored; the committed record supplies the current safe plan.
        match operations::undo_of(&operation) {
            Some(OperationKind::UndoCopyV1 { .. }) => {
                match crate::transfer_undo::validate(&operation) {
                    Ok(()) => Ok((true, String::new())),
                    Err(error) => Ok((false, format!("{error:#}"))),
                }
            }
            Some(inverse @ OperationKind::RestoreMoveV1 { .. }) => match operations::validate(&inverse) {
                Ok(()) => Ok((true, String::new())),
                Err(error) => Ok((false, format!("{error:#}"))),
            },
            Some(_) => Ok((true, String::new())),
            None => Ok((false, "the journal has no guarded or exact inverse for this operation".to_owned())),
        }
    }

    /// Undo a committed operation; returns the ID of the operation that reverses it.
    async fn undo(&self, op_id: &str) -> fdo::Result<String> {
        let id = OperationId::parse(op_id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad operation id {op_id}")))?;
        if self.0.journal.undo_of(id).map_err(failed)?.is_none() {
            return Err(fdo::Error::Failed(format!("{op_id} cannot be undone")));
        }
        let operation = self.0.journal.get(id).map_err(failed)?
            .ok_or_else(|| fdo::Error::Failed(format!("no operation {op_id}")))?;
        let undo = operations::undo_of(&operation)
            .ok_or_else(|| fdo::Error::Failed("the journal has no guarded or exact inverse for this operation".into()))?;
        let new_id = self
            .0
            .start(undo, Options::default(), None)
            .map_err(failed)?;
        self.0.journal.clear_undo(id).map_err(failed)?;
        Ok(new_id.to_string())
    }

    /// How far along an operation is: state, bytes and items.
    #[zbus(signal)]
    async fn progress(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        op_id: String,
        state: String,
        done_bytes: u64,
        total_bytes: u64,
        done_items: u64,
        total_items: u64,
    ) -> zbus::Result<()>;

    /// An operation reached a terminal state.
    #[zbus(signal)]
    async fn finished(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        op_id: String,
        state: String,
        error: String,
    ) -> zbus::Result<()>;
}

/// Which mount points [`TrashApi`] looks on for a per-volume trash.
///
/// A test must never be able to reach the developer's own removable disks — `Empty` deletes what
/// it finds — so the mount table is a value here rather than a call inside the sweep.
#[derive(Clone, Debug)]
pub enum TrashMounts {
    /// Every user-visible mounted volume, as `Files.Volumes1` reports them. What the daemon uses.
    System,
    /// Exactly these mount points, and nothing else.
    Fixed(Vec<PathBuf>),
}

impl TrashMounts {
    fn mount_points(&self) -> Vec<PathBuf> {
        match self {
            // A failed mount-table read costs the volume trashes, not the home trash: a listing
            // that shows most of the trash beats one that shows an error.
            Self::System => storage::volumes(0)
                .unwrap_or_default()
                .into_iter()
                .map(|volume| volume.mount_point)
                .collect(),
            Self::Fixed(points) => points.clone(),
        }
    }
}

/// `org.templaros.Files.Trash1` — what is in the trash, and the three things done with it.
///
/// Every mutating method is a validating wrapper over [`FileService::start`], the same entry
/// point `Operations1.Start` uses. That is the whole design: a large empty keeps its progress,
/// its journal and its crash-resume, and a restore keeps its undo, because there is exactly one
/// execution path and this is not a second one.
#[derive(Debug)]
pub struct TrashApi {
    pub service: FileService,
    pub mounts: TrashMounts,
}

impl TrashApi {
    /// The daemon's configuration: the home trash plus every mounted volume's.
    pub fn new(service: FileService) -> Self {
        Self {
            service,
            mounts: TrashMounts::System,
        }
    }

    fn trash_dirs(&self) -> Vec<trash::TrashDir> {
        trash::all_trash_dirs(&self.service.config.data_home, &self.mounts.mount_points())
    }

    /// Turn client-named paths into trash entries, refusing anything that is not one.
    ///
    /// Any process on the session bus can call this, so "it came from List" is not something the
    /// service may assume: a path that is not a direct child of one of this user's trashes is
    /// rejected before anything is journalled.
    fn resolve(paths: &[String], dirs: &[trash::TrashDir]) -> fdo::Result<Vec<PathBuf>> {
        if paths.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "at least one trashed path is required".to_owned(),
            ));
        }
        let mut resolved = Vec::with_capacity(paths.len());
        for path in paths {
            let path = PathBuf::from(path);
            let Some(trash) = trash::owning_trash(&path, dirs) else {
                return Err(fdo::Error::InvalidArgs(format!(
                    "{} is not an item in this user's trash",
                    path.display()
                )));
            };
            if path.symlink_metadata().is_err() {
                return Err(fdo::Error::FileNotFound(format!(
                    "{} is no longer in the trash",
                    path.display()
                )));
            }
            resolved.push((path, trash));
        }
        Ok(resolved
            .into_iter()
            .flat_map(|(path, trash)| {
                let info = trash::info_path_of(&path, trash);
                std::iter::once(path).chain(info)
            })
            .collect())
    }

    fn begin(&self, kind: OperationKind, app_id: &str) -> fdo::Result<String> {
        let app_id = (!app_id.is_empty()).then(|| app_id.parse().ok()).flatten();
        self.service
            .start(kind, Options::default(), app_id.as_ref())
            .map(|id| id.to_string())
            .map_err(failed)
    }
}

#[zbus::interface(name = "org.templaros.Files.Trash1")]
impl TrashApi {
    /// Every trash this user has, newest deletion first, as `TrashEntry` JSON.
    async fn list(&self) -> fdo::Result<String> {
        let dirs = self.trash_dirs();
        let entries = tokio::task::spawn_blocking(move || trash::list_all(&dirs))
            .await
            .map_err(failed)?;
        json(&entries)
    }

    /// Put items back where they came from; returns the operation ID.
    async fn restore(&self, trashed_paths: Vec<String>, app_id: &str) -> fdo::Result<String> {
        let dirs = self.trash_dirs();
        // The `.trashinfo` is not restored — it is consumed by the restore itself — so only the
        // items go into the operation.
        let paths: Vec<PathBuf> = Self::resolve(&trashed_paths, &dirs)?
            .into_iter()
            .filter(|path| path.extension().is_none_or(|kind| kind != "trashinfo"))
            .collect();
        self.begin(OperationKind::RestoreFromTrash { paths }, app_id)
    }

    /// Remove items from the trash for good, with their records. No undo.
    async fn delete_permanently(
        &self,
        trashed_paths: Vec<String>,
        app_id: &str,
    ) -> fdo::Result<String> {
        let dirs = self.trash_dirs();
        let paths = Self::resolve(&trashed_paths, &dirs)?;
        self.begin(OperationKind::Delete { paths }, app_id)
    }

    /// Empty every trash this user has. The sweep is here rather than in the client, so nothing
    /// trashed between a listing and a delete is missed. An empty trash returns an empty ID.
    async fn empty(&self, app_id: &str) -> fdo::Result<String> {
        let dirs = self.trash_dirs();
        let paths = tokio::task::spawn_blocking(move || {
            dirs.iter()
                .flat_map(trash::everything_in)
                .collect::<Vec<_>>()
        })
        .await
        .map_err(failed)?;
        if paths.is_empty() {
            return Ok(String::new());
        }
        self.begin(OperationKind::Delete { paths }, app_id)
    }

    /// The trash's contents changed — call `List`.
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

/// `org.templaros.Files.Recents1` — recent files and the shared last-used directory.
#[derive(Debug)]
pub struct RecentsApi(pub FileService);

#[zbus::interface(name = "org.templaros.Files.Recents1")]
impl RecentsApi {
    async fn record(&self, path: &str, app_id: &str, action: &str, mime: &str) -> fdo::Result<()> {
        let app_id: Option<AppId> = (!app_id.is_empty()).then(|| app_id.parse().ok()).flatten();
        let action = RecentAction::parse(action)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown action {action}")))?;
        self.0
            .recents
            .record(
                Path::new(path),
                app_id.as_ref(),
                action,
                (!mime.is_empty()).then_some(mime),
            )
            .map_err(failed)
    }

    /// Recent entries as JSON, newest first; an empty `app_id` means every application.
    async fn list(&self, app_id: &str, limit: u32) -> fdo::Result<String> {
        let app_id: Option<AppId> = (!app_id.is_empty()).then(|| app_id.parse().ok()).flatten();
        json(
            &self
                .0
                .recents
                .list(app_id.as_ref(), limit as usize)
                .map_err(failed)?,
        )
    }

    async fn forget(&self, path: &str) -> fdo::Result<()> {
        self.0.recents.forget(Path::new(path)).map_err(failed)
    }

    /// Drop entries whose file is gone; returns how many.
    async fn prune_missing(&self) -> fdo::Result<u32> {
        self.0
            .recents
            .prune_missing()
            .map(|count| count as u32)
            .map_err(failed)
    }

    /// Which directory a chooser should open at, and why — the one answer the file manager,
    /// the Home Surface and the portal all use.
    async fn suggest_directory(&self, app_id: &str, operation: &str) -> fdo::Result<String> {
        let app_id: Option<AppId> = (!app_id.is_empty()).then(|| app_id.parse().ok()).flatten();
        let operation = OperationType::parse(operation)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown operation {operation}")))?;
        json(
            &self
                .0
                .directories
                .suggest(app_id.as_ref(), operation)
                .map_err(failed)?,
        )
    }

    async fn record_directory(
        &self,
        app_id: &str,
        operation: &str,
        directory: &str,
    ) -> fdo::Result<()> {
        let app_id: Option<AppId> = (!app_id.is_empty()).then(|| app_id.parse().ok()).flatten();
        let operation = OperationType::parse(operation)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown operation {operation}")))?;
        self.0
            .directories
            .record(app_id.as_ref(), operation, Path::new(directory))
            .map_err(failed)
    }

    /// Note the open project, so its root is offered before the general fallback.
    async fn set_project(&self, project_id: &str, root: &str) -> fdo::Result<()> {
        self.0
            .directories
            .set_project(project_id, Path::new(root))
            .map_err(failed)
    }

    async fn clear_project(&self, project_id: &str) -> fdo::Result<()> {
        self.0.directories.clear_project(project_id).map_err(failed)
    }
}

/// `org.templaros.Files.Search1` — what makes every drawer searchable.
#[derive(Debug)]
pub struct SearchApi(pub FileService);

#[zbus::interface(name = "org.templaros.Files.Search1")]
impl SearchApi {
    /// Register a root and queue the watcher; returns the last committed entry count, not a
    /// synchronous tree walk. Index1.Roots reports the resulting building/stale/current state.
    async fn index_root(&self, root: &str) -> fdo::Result<u32> {
        let control = crate::observer::control().ok_or_else(|| failed("index watcher is not ready"))?;
        let native = templar_file_index::NativePath::from_path(Path::new(root));
        let existing = self.0.index.configured_policy(&native).map_err(failed)?;
        let expected = existing.as_ref().map_or_else(|| "absent".to_owned(), templar_file_index::RootPolicy::hash);
        let status = self.0.index.configure_root(&native, existing.unwrap_or_default(), &expected).map_err(failed)?;
        control.send_required(crate::observer::Command::Reconcile(status.root,
            templar_file_index::reconcile::Trigger::Requested)).map_err(failed)?;
        u32::try_from(status.entry_count).map_err(failed)
    }

    async fn roots(&self) -> fdo::Result<Vec<String>> {
        Ok(self
            .0
            .index
            .roots()
            .map_err(failed)?
            .into_iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect())
    }

    async fn forget_root(&self, root: &str) -> fdo::Result<()> {
        let control = crate::observer::control().ok_or_else(|| failed("index watcher is not ready"))?;
        let native = templar_file_index::NativePath::from_path(Path::new(root));
        let policy = self.0.index.configured_policy(&native).map_err(failed)?
            .ok_or_else(|| fdo::Error::InvalidArgs("unknown indexed native root".to_owned()))?;
        if let Some(root) = self.0.index.forget_root_native(&native, &policy.hash()).map_err(failed)? {
            control.send_required(crate::observer::Command::ForgetRoot(root)).map_err(failed)?;
        }
        Ok(())
    }

    /// Hits as JSON, best match first. An empty `roots` searches everything indexed.
    async fn search(&self, query: &str, roots: Vec<String>, limit: u32) -> fdo::Result<String> {
        let roots: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
        let hits = self
            .0
            .index
            .search(query, &roots, limit as usize)
            .map_err(failed)?;
        json(
            &hits
                .into_iter()
                .map(|hit| (hit.entry, format!("{:?}", hit.rank).to_lowercase()))
                .collect::<Vec<_>>(),
        )
    }

    async fn save_query(&self, name: &str, query: &str, roots: Vec<String>) -> fdo::Result<()> {
        self.0
            .index
            .save_query(&SavedQuery {
                name: name.to_owned(),
                query: query.to_owned(),
                roots: roots.into_iter().map(PathBuf::from).collect(),
            })
            .map_err(failed)
    }

    async fn saved_queries(&self) -> fdo::Result<String> {
        json(&self.0.index.saved_queries().map_err(failed)?)
    }
}

/// `org.templaros.Files.Actions1` — the small actions that are otherwise a service menu in every
/// file manager, each implemented slightly differently.
#[derive(Debug)]
pub struct ActionsApi;

#[zbus::interface(name = "org.templaros.Files.Actions1")]
impl ActionsApi {
    /// Render `path` in one of the forms from `Forms`.
    async fn render(&self, path: &str, form: &str) -> fdo::Result<String> {
        let form = PathForm::parse(form)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown path form {form}")))?;
        actions::render(Path::new(path), form).map_err(failed)
    }

    /// Native byte path, independent of the legacy UTF-8 Render signature.
    async fn render_native(&self, path_b64: &str, form: &str) -> fdo::Result<String> {
        let path = templar_client::file_index::decode_path(path_b64)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let path = templar_file_index::NativePath::from_bytes(path);
        let form = PathForm::parse(form)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown path form {form}")))?;
        actions::render(path.as_path(), form).map_err(failed)
    }

    /// Every available form.
    async fn forms(&self) -> Vec<String> {
        PathForm::ALL
            .iter()
            .map(|form| form.as_str().to_owned())
            .collect()
    }

    /// The working directory "open a terminal here" should use.
    async fn terminal_directory(&self, path: &str) -> fdo::Result<String> {
        Ok(actions::terminal_directory(Path::new(path))
            .to_string_lossy()
            .into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{journal::MemoryJournal, recents::Recents, search::Index};
    use std::time::Duration;
    use templar_state::Store;

    fn service(dir: &Path) -> FileService {
        let store = Store::in_memory().unwrap();
        FileService::new(
            Arc::new(MemoryJournal::default()),
            ExecutorConfig {
                data_home: dir.join("data"),
                reserve_percent: 0,
                progress_interval: Duration::from_millis(0),
                ..ExecutorConfig::default()
            },
            Recents::new(store.clone()),
            Index::new(store.clone()),
            LastDirectoryStore::new(store, dir.to_path_buf()),
            0,
        )
    }

    async fn settle(service: &FileService, id: OperationId) -> Operation {
        for _ in 0..200 {
            if let Some(operation) = service.journal.get(id).unwrap()
                && operation.state.terminal()
                && !service.running.lock().contains_key(&id)
            {
                return operation;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("operation {id} never finished");
    }

    #[tokio::test]
    async fn a_copy_runs_journals_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/a.txt");
        let destination = dir.path().join("to");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(&source, b"payload").unwrap();

        let service = service(dir.path());
        let id = service
            .start(
                OperationKind::Copy {
                    sources: vec![source.clone()],
                    destination_dir: destination.clone(),
                },
                Options::default(),
                None,
            )
            .unwrap();

        let operation = settle(&service, id).await;
        assert_eq!(operation.state, OperationState::Committed);
        assert_eq!(
            std::fs::read(destination.join("a.txt")).unwrap(),
            b"payload"
        );
        assert_eq!(operation.done_bytes, operation.total_bytes);
        assert_eq!(service.journal.recent(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_committed_move_can_be_undone() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/a.txt");
        let destination = dir.path().join("to");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(&source, b"payload").unwrap();

        let service = service(dir.path());
        let id = service
            .start(
                OperationKind::Move {
                    sources: vec![source.clone()],
                    destination_dir: destination.clone(),
                },
                Options::default(),
                None,
            )
            .unwrap();
        settle(&service, id).await;
        assert!(!source.exists());

        let undo = service.journal.undo_of(id).unwrap().expect("undo plan");
        let undo_id = service.start(undo, Options::default(), None).unwrap();
        settle(&service, undo_id).await;
        assert_eq!(std::fs::read(&source).unwrap(), b"payload");
        assert!(!destination.join("a.txt").exists());
    }

    #[tokio::test]
    async fn a_delete_records_no_undo_plan() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("gone.txt");
        std::fs::write(&victim, b"x").unwrap();

        let service = service(dir.path());
        let id = service
            .start(
                OperationKind::Delete {
                    paths: vec![victim.clone()],
                },
                Options::default(),
                None,
            )
            .unwrap();
        settle(&service, id).await;
        assert!(!victim.exists());
        assert_eq!(service.journal.undo_of(id).unwrap(), None);
    }

    /// A `Trash1` over a service whose only trashes are the fixture's: the home trash under the
    /// temporary `data_home`, and whichever fake volumes the test names.
    fn trash_api(service: &FileService, volumes: &[PathBuf]) -> TrashApi {
        TrashApi {
            service: service.clone(),
            mounts: TrashMounts::Fixed(volumes.to_vec()),
        }
    }

    /// Trash `path` through the same journalled path a file manager uses, and wait for it.
    async fn trash_it(service: &FileService, path: &Path) {
        let id = service
            .start(
                OperationKind::Trash {
                    paths: vec![path.to_path_buf()],
                },
                Options::default(),
                None,
            )
            .unwrap();
        assert_eq!(settle(service, id).await.state, OperationState::Committed);
    }

    fn entries(listing: &str) -> Vec<crate::trash::TrashEntry> {
        serde_json::from_str(listing).expect("Trash1.List returns TrashEntry JSON")
    }

    #[tokio::test]
    async fn listing_the_trash_shows_what_was_trashed_and_where_it_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let api = trash_api(&service, &[]);
        assert_eq!(entries(&api.list().await.unwrap()), Vec::new());

        let source = dir.path().join("work/report.pdf");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"contents").unwrap();
        trash_it(&service, &source).await;

        let listed = entries(&api.list().await.unwrap());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "report.pdf");
        assert_eq!(listed[0].original_path, source);
        assert_eq!(listed[0].size_bytes, 8);
        assert!(listed[0].trashed_path.exists());
    }

    #[tokio::test]
    async fn restoring_puts_an_item_back_and_leaves_the_trash_empty() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let api = trash_api(&service, &[]);
        let source = dir.path().join("notes.txt");
        std::fs::write(&source, b"keep me").unwrap();
        trash_it(&service, &source).await;
        assert!(!source.exists());

        let listed = entries(&api.list().await.unwrap());
        let op = api
            .restore(vec![listed[0].trashed_path.display().to_string()], "")
            .await
            .unwrap();
        let operation = settle(&service, OperationId::parse(&op).unwrap()).await;
        assert_eq!(operation.state, OperationState::Committed);
        assert_eq!(std::fs::read(&source).unwrap(), b"keep me");
        assert!(entries(&api.list().await.unwrap()).is_empty());
        // Restoring is journalled like anything else, so it can be undone: the file goes back
        // into the trash rather than being lost.
        assert!(service.journal.undo_of(operation.id).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_permanent_delete_takes_the_item_and_its_record_and_offers_no_undo() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let api = trash_api(&service, &[]);
        let source = dir.path().join("draft.txt");
        std::fs::write(&source, b"x").unwrap();
        trash_it(&service, &source).await;

        let listed = entries(&api.list().await.unwrap());
        let op = api
            .delete_permanently(vec![listed[0].trashed_path.display().to_string()], "")
            .await
            .unwrap();
        let operation = settle(&service, OperationId::parse(&op).unwrap()).await;
        assert_eq!(operation.state, OperationState::Committed);
        assert!(!listed[0].trashed_path.exists());
        assert!(!listed[0].info_path.exists(), "the record went with it");
        assert!(entries(&api.list().await.unwrap()).is_empty());
        assert_eq!(service.journal.undo_of(operation.id).unwrap(), None);
    }

    #[tokio::test]
    async fn emptying_sweeps_every_trash_and_says_nothing_to_do_when_there_is_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let volume = dir.path().join("mnt/stick");
        std::fs::create_dir_all(&volume).unwrap();
        let volume_trash = crate::trash::volume_trash(&volume);
        let api = trash_api(&service, std::slice::from_ref(&volume));
        assert_eq!(
            api.empty("").await.unwrap(),
            "",
            "an empty trash is not an error"
        );

        let home_item = dir.path().join("one.txt");
        std::fs::write(&home_item, b"a").unwrap();
        trash_it(&service, &home_item).await;
        // Trashed straight into the volume's own trash, the way a file on a second disk is.
        let on_volume = volume.join("two.txt");
        std::fs::write(&on_volume, b"b").unwrap();
        crate::trash::trash(&on_volume, &volume_trash, "2026-08-30T10:00:00").unwrap();
        assert_eq!(entries(&api.list().await.unwrap()).len(), 2);

        let op = api.empty("").await.unwrap();
        let operation = settle(&service, OperationId::parse(&op).unwrap()).await;
        assert_eq!(operation.state, OperationState::Committed);
        assert!(entries(&api.list().await.unwrap()).is_empty());
        assert!(
            std::fs::read_dir(volume_trash.files())
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            std::fs::read_dir(volume_trash.info())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_path_that_is_not_in_the_trash_is_refused_before_anything_is_journalled() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let api = trash_api(&service, &[]);
        let outsider = dir.path().join("precious.txt");
        std::fs::write(&outsider, b"do not touch").unwrap();

        for paths in [Vec::new(), vec![outsider.display().to_string()]] {
            assert!(matches!(
                api.delete_permanently(paths.clone(), "").await,
                Err(fdo::Error::InvalidArgs(_))
            ));
            assert!(matches!(
                api.restore(paths, "").await,
                Err(fdo::Error::InvalidArgs(_))
            ));
        }
        assert!(outsider.exists());
        assert!(service.journal.recent(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn unfinished_operations_are_resumed_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/a.txt");
        let destination = dir.path().join("to");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(&source, b"payload").unwrap();

        // A journal left behind by a service that died mid-operation.
        let service = service(dir.path());
        let surveyed = operations::survey(std::slice::from_ref(&source), Some(&destination));
        let operation = operations::plan(
            OperationId::new(),
            OperationKind::Copy {
                sources: vec![source.clone()],
                destination_dir: destination.clone(),
            },
            &surveyed,
            Options::default(),
        );
        service.journal.put(&operation, None).unwrap();

        assert_eq!(service.resume_unfinished().unwrap(), 1);
        let finished = settle(&service, operation.id).await;
        assert_eq!(finished.state, OperationState::Committed);
        assert_eq!(
            std::fs::read(destination.join("a.txt")).unwrap(),
            b"payload"
        );
    }

    fn native_service(dir: &Path) -> FileService {
        let store = Store::open(&dir.join("state.sqlite")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        FileService::new(
            Arc::new(crate::journal::SqliteJournal::new(store.clone())),
            ExecutorConfig { data_home: dir.join("data"), reserve_percent: 0,
                progress_interval: Duration::ZERO, ..ExecutorConfig::default() },
            Recents::new(store.clone()), Index::new(store.clone()),
            LastDirectoryStore::new(store, dir.to_path_buf()), 0,
        )
    }

    fn native_name(bytes: &[u8]) -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(bytes.to_vec())
    }

    async fn native_start(api: &OperationsApi, request: templar_client::files::OperationRequest,
        conflict: templar_client::files::ConflictPolicy) -> OperationId {
        let options = templar_client::files::OperationOptions {
            conflict, verify: templar_client::files::Verification::Checksum, override_reserve: false,
        };
        let id = api.start(&serde_json::to_string(&request).unwrap(),
            &serde_json::to_string(&options).unwrap(), "").await.unwrap();
        OperationId::parse(&id).unwrap()
    }

    #[tokio::test]
    async fn native_copy_wire_journal_undo_and_restore_keep_equal_display_names_distinct() {
        use templar_client::files::{ConflictPolicy, OperationRecord, OperationRequest};
        let fixture = tempfile::tempdir().unwrap();
        let from = fixture.path().join("from");
        let to = fixture.path().join("Desktop");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let paths: Vec<_> = [b"name-\xff.pdf".as_slice(), b"name-\xfe.pdf".as_slice()]
            .into_iter().enumerate().map(|(index, name)| {
                let path = from.join(native_name(name));
                std::fs::write(&path, [index as u8, 0, 255]).unwrap();
                path
            }).collect();
        assert_eq!(paths[0].file_name().unwrap().to_string_lossy(), paths[1].file_name().unwrap().to_string_lossy());
        let service = native_service(fixture.path());
        let api = OperationsApi(service.clone());
        let id = native_start(&api, OperationRequest::Copy {
            sources: paths.clone(), destination_dir: to.clone(),
        }, ConflictPolicy::Fail).await;
        assert_eq!(settle(&service, id).await.state, OperationState::Committed);
        let wire: OperationRecord = serde_json::from_str(&api.get(&id.to_string()).await.unwrap()).unwrap();
        assert!(matches!(wire.kind, OperationRequest::Copy { sources, .. } if sources == paths));
        // Open a second service over the same real on-disk journal, not a reconstructed plan.
        let restarted = native_service(fixture.path());
        let api = OperationsApi(restarted.clone());
        assert!(api.can_undo(&id.to_string()).await.unwrap().0);
        let inverse = OperationId::parse(&api.undo(&id.to_string()).await.unwrap()).unwrap();
        assert_eq!(settle(&restarted, inverse).await.state, OperationState::Committed);
        assert!(std::fs::read_dir(&to).unwrap().next().is_none());
        let trashed = crate::trash::list(&crate::trash::home_trash(&fixture.path().join("data"))).unwrap();
        assert_eq!(trashed.len(), 2);
        for (index, source) in paths.iter().enumerate() {
            assert_eq!(std::fs::read(source).unwrap(), [index as u8, 0, 255]);
            let item = trashed.iter().find(|item| item.original_path == to.join(source.file_name().unwrap())).unwrap();
            assert_eq!(std::fs::read(&item.trashed_path).unwrap(), [index as u8, 0, 255]);
        }
        let restore = OperationId::parse(&api.undo(&inverse.to_string()).await.unwrap()).unwrap();
        assert_eq!(settle(&restarted, restore).await.state, OperationState::Committed);
        for (index, source) in paths.iter().enumerate() {
            assert_eq!(std::fs::read(to.join(source.file_name().unwrap())).unwrap(), [index as u8, 0, 255]);
        }
    }

    #[tokio::test]
    async fn native_move_keep_both_inverse_restores_each_original_and_refuses_recreated_names() {
        use templar_client::files::{ConflictPolicy, OperationRequest};
        let fixture = tempfile::tempdir().unwrap();
        let to = fixture.path().join("Desktop");
        std::fs::create_dir(&to).unwrap();
        let name = native_name(b"same-\xff.txt");
        let old = to.join(&name);
        std::fs::write(&old, b"old collision").unwrap();
        let paths: Vec<_> = ["one", "two"].into_iter().enumerate().map(|(index, parent)| {
            let parent = fixture.path().join(parent);
            std::fs::create_dir(&parent).unwrap();
            let path = parent.join(&name);
            std::fs::write(&path, [index as u8, 255]).unwrap();
            path
        }).collect();
        let service = native_service(fixture.path());
        let api = OperationsApi(service.clone());
        let id = native_start(&api, OperationRequest::Move {
            sources: paths.clone(), destination_dir: to.clone(),
        }, ConflictPolicy::KeepBoth).await;
        let moved = settle(&service, id).await;
        assert_eq!(moved.state, OperationState::Committed);
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(std::fs::read(&old).unwrap(), b"old collision");
        std::fs::write(&paths[0], b"recreated").unwrap();
        assert!(!api.can_undo(&id.to_string()).await.unwrap().0);
        assert!(api.undo(&id.to_string()).await.is_err());
        assert!(moved.transfers.iter().all(|entry| entry.destination.exists()));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"recreated");
        std::fs::remove_file(&paths[0]).unwrap();
        let inverse = OperationId::parse(&api.undo(&id.to_string()).await.unwrap()).unwrap();
        assert_eq!(settle(&service, inverse).await.state, OperationState::Committed);
        for (index, path) in paths.iter().enumerate() {
            assert_eq!(std::fs::read(path).unwrap(), [index as u8, 255]);
        }
        assert_eq!(std::fs::read(&old).unwrap(), b"old collision");
        assert!(moved.transfers.iter().all(|entry| !entry.destination.exists()));
    }

    #[tokio::test]
    async fn native_directory_symlink_copy_refuses_collisions_and_changed_destination_undo() {
        use templar_client::files::{ConflictPolicy, OperationRequest};
        use std::os::unix::fs::symlink;
        for change in ["file", "child", "symlink", "none"] {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join(native_name(b"tree-\xff"));
            let to = fixture.path().join("Desktop");
            std::fs::create_dir_all(source.join("nested")).unwrap();
            std::fs::create_dir(&to).unwrap();
            let child = native_name(b"child-\xfe.txt");
            std::fs::write(source.join("nested").join(&child), b"body").unwrap();
            symlink(PathBuf::from("nested").join(&child), source.join("link")).unwrap();
            let service = native_service(fixture.path());
            let api = OperationsApi(service.clone());
            let id = native_start(&api, OperationRequest::Copy {
                sources: vec![source.clone()], destination_dir: to.clone(),
            }, ConflictPolicy::Fail).await;
            assert_eq!(settle(&service, id).await.state, OperationState::Committed);
            let copied = to.join(source.file_name().unwrap());
            assert_eq!(std::fs::read_link(copied.join("link")).unwrap(), PathBuf::from("nested").join(&child));
            let collision = native_start(&api, OperationRequest::Copy {
                sources: vec![source.clone()], destination_dir: to.clone(),
            }, ConflictPolicy::Fail).await;
            assert_eq!(settle(&service, collision).await.state, OperationState::Failed);
            if change == "none" {
                let inverse = OperationId::parse(&api.undo(&id.to_string()).await.unwrap()).unwrap();
                assert_eq!(settle(&service, inverse).await.state, OperationState::Committed);
                assert!(!copied.exists());
                let restore = OperationId::parse(&api.undo(&inverse.to_string()).await.unwrap()).unwrap();
                assert_eq!(settle(&service, restore).await.state, OperationState::Committed);
                assert_eq!(std::fs::read(copied.join("nested").join(&child)).unwrap(), b"body");
                assert_eq!(std::fs::read_link(copied.join("link")).unwrap(), PathBuf::from("nested").join(&child));
                continue;
            }
            match change {
                "file" => std::fs::write(copied.join("nested").join(&child), b"edited after copy").unwrap(),
                "child" => std::fs::write(copied.join("new-file"), b"new child").unwrap(),
                "symlink" => {
                    std::fs::remove_file(copied.join("link")).unwrap();
                    symlink("other-target", copied.join("link")).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(!api.can_undo(&id.to_string()).await.unwrap().0, "{change}");
            assert!(api.undo(&id.to_string()).await.is_err(), "{change}");
            assert!(copied.exists());
            assert!(crate::trash::list(&crate::trash::home_trash(&fixture.path().join("data"))).unwrap().is_empty());
            assert_eq!(std::fs::read(source.join("nested").join(&child)).unwrap(), b"body");
        }
    }

    #[tokio::test]
    async fn native_replacement_transfers_have_no_inverse_and_identical_symlink_is_a_collision() {
        use templar_client::files::{ConflictPolicy, OperationRequest};
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let from = fixture.path().join("from");
        let to = fixture.path().join("Desktop");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let source = from.join(native_name(b"link-\xff"));
        let destination = to.join(source.file_name().unwrap());
        symlink(native_name(b"target-\xfe"), &source).unwrap();
        symlink(native_name(b"target-\xfe"), &destination).unwrap();
        let service = native_service(fixture.path());
        let api = OperationsApi(service.clone());
        let id = native_start(&api, OperationRequest::Copy {
            sources: vec![source.clone()], destination_dir: to.clone(),
        }, ConflictPolicy::Fail).await;
        assert_eq!(settle(&service, id).await.state, OperationState::Failed);
        assert!(source.symlink_metadata().unwrap().file_type().is_symlink());
        for moving in [false, true] {
            let request = if moving { OperationRequest::Move { sources: vec![source.clone()], destination_dir: to.clone() } }
                else { OperationRequest::Copy { sources: vec![source.clone()], destination_dir: to.clone() } };
            let id = native_start(&api, request, ConflictPolicy::Replace).await;
            assert_eq!(settle(&service, id).await.state, OperationState::Committed);
            assert!(!api.can_undo(&id.to_string()).await.unwrap().0);
            assert!(api.undo(&id.to_string()).await.is_err());
        }
    }

    #[tokio::test]
    async fn native_browse_and_preview_keep_lexical_symlink_desktop_and_raw_image_path() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("staging");
        let desktop = fixture.path().join("Desktop");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &desktop).unwrap();
        let name = native_name(b"image-\xff.png");
        let path = desktop.join(&name);
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255])).save(&path).unwrap();
        let api = BrowseApi { reserve_percent: 0, bookmarks: crate::bookmarks::Bookmarks(Store::in_memory().unwrap()) };
        let json: serde_json::Value = serde_json::from_str(&api.list_directory_native_v1(
            &templar_client::file_index::encode_path(&desktop), false, 500).await.unwrap()).unwrap();
        assert_eq!(json["directory_b64"], templar_client::file_index::encode_path(&desktop));
        assert_eq!(json["watch_directory_b64"], templar_client::file_index::encode_path(&target));
        assert_eq!(json["entries"][0]["path_b64"], templar_client::file_index::encode_path(&path));
        let (metadata, png) = PreviewsApi.render_native_v1(&templar_client::file_index::encode_path(&path), 64).await.unwrap();
        assert!(!metadata.is_empty());
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn pre_cutover_transfer_records_cannot_execute_stored_permanent_or_guessed_inverses() {
        use templar_client::files::{ConflictPolicy, OperationRequest};
        for moving in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("from/file");
            let destination = fixture.path().join("Desktop");
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::create_dir(&destination).unwrap();
            std::fs::write(&source, b"payload").unwrap();
            let service = native_service(fixture.path());
            let api = OperationsApi(service.clone());
            let request = if moving {
                OperationRequest::Move { sources: vec![source.clone()], destination_dir: destination.clone() }
            } else {
                OperationRequest::Copy { sources: vec![source.clone()], destination_dir: destination.clone() }
            };
            let id = native_start(&api, request, ConflictPolicy::Fail).await;
            let mut legacy = settle(&service, id).await;
            assert_eq!(legacy.state, OperationState::Committed);
            legacy.created.clear();
            legacy.transfers.clear();
            service.journal.progress(&legacy).unwrap();
            let copied = destination.join("file");
            let old_inverse = if moving {
                OperationKind::Move { sources: vec![copied.clone()], destination_dir: source.parent().unwrap().to_path_buf() }
            } else {
                OperationKind::Delete { paths: vec![copied.clone()] }
            };
            service.journal.put_undo(id, &old_inverse).unwrap();
            assert!(!api.can_undo(&id.to_string()).await.unwrap().0);
            assert!(api.undo(&id.to_string()).await.is_err());
            assert_eq!(std::fs::read(&copied).unwrap(), b"payload");
        }
    }
}
