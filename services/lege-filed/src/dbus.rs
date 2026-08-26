//! The D-Bus face of the file service: `org.legeos.Files`.
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
    storage,
};
use lege_types::{AppId, dbus::files as names};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
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
        config: ExecutorConfig,
        recents: Recents,
        index: Index,
        directories: LastDirectoryStore,
        reserve_percent: u64,
    ) -> Self {
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
        kind: OperationKind,
        options: Options,
        app_id: Option<&AppId>,
    ) -> anyhow::Result<OperationId> {
        operations::validate(&kind)?;
        let id = OperationId::new();
        let surveyed = operations::survey(&sources_of(&kind), destination_of(&kind));
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
        });
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
        let Some(connection) = self.connection.lock().clone() else {
            return;
        };
        let body = body.clone();
        tokio::spawn(async move {
            if let Err(error) = connection
                .emit_signal(
                    None::<&str>,
                    names::ROOT_PATH,
                    names::OPERATIONS_IFACE,
                    member,
                    &body,
                )
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
        // Trashing and restoring never traverse: they are renames, and surveying a large
        // directory before trashing it would make "delete" mysteriously slow.
        OperationKind::Trash { .. }
        | OperationKind::RestoreFromTrash { .. }
        | OperationKind::Rename { .. } => Vec::new(),
    }
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

fn failed(error: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(error.to_string())
}

fn json<T: serde::Serialize>(value: &T) -> fdo::Result<String> {
    serde_json::to_string(value).map_err(failed)
}

/// `org.legeos.Files.Volumes1` — storage information that must be unavoidable in every file UI.
#[derive(Debug)]
pub struct VolumesApi {
    pub reserve_percent: u64,
}

#[zbus::interface(name = "org.legeos.Files.Volumes1")]
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

/// `org.legeos.Files.Browse1` — immediate, bounded directory snapshots. The blocking directory
/// read stays off the async executor, just like file operations do.
#[derive(Debug)]
pub struct BrowseApi {
    pub reserve_percent: u64,
}

#[zbus::interface(name = "org.legeos.Files.Browse1")]
impl BrowseApi {
    /// XDG places and every user-visible mounted drive, with volume data attached to drives.
    async fn roots(&self) -> fdo::Result<String> {
        let reserve_percent = self.reserve_percent;
        let sources = tokio::task::spawn_blocking(move || browse::sources(reserve_percent))
            .await
            .map_err(failed)?
            .map_err(failed)?;
        json(&sources)
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

/// `org.legeos.Files.Previews1` — bounded thumbnails rendered away from GUI and D-Bus threads.
#[derive(Debug, Default)]
pub struct PreviewsApi;

#[zbus::interface(name = "org.legeos.Files.Previews1")]
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
}

/// `org.legeos.Files.Operations1` — journalled transactions with progress and undo.
#[derive(Debug)]
pub struct OperationsApi(pub FileService);

#[zbus::interface(name = "org.legeos.Files.Operations1")]
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
        self.0
            .start(kind, options, app_id.as_ref())
            .map(|id| id.to_string())
            .map_err(failed)
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
        match self.0.journal.undo_of(id).map_err(failed)? {
            Some(_) => Ok((true, String::new())),
            None => Ok((false, "no undo plan was recorded".to_owned())),
        }
    }

    /// Undo a committed operation; returns the ID of the operation that reverses it.
    async fn undo(&self, op_id: &str) -> fdo::Result<String> {
        let id = OperationId::parse(op_id)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("bad operation id {op_id}")))?;
        let undo = self
            .0
            .journal
            .undo_of(id)
            .map_err(failed)?
            .ok_or_else(|| fdo::Error::Failed(format!("{op_id} cannot be undone")))?;
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

/// `org.legeos.Files.Recents1` — recent files and the shared last-used directory.
#[derive(Debug)]
pub struct RecentsApi(pub FileService);

#[zbus::interface(name = "org.legeos.Files.Recents1")]
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

/// `org.legeos.Files.Search1` — what makes every drawer searchable.
#[derive(Debug)]
pub struct SearchApi(pub FileService);

#[zbus::interface(name = "org.legeos.Files.Search1")]
impl SearchApi {
    /// Index (or re-index) a root; returns how many entries it holds.
    async fn index_root(&self, root: &str) -> fdo::Result<u32> {
        self.0
            .index
            .index_root(Path::new(root))
            .map(|count| count as u32)
            .map_err(failed)
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
        self.0.index.forget_root(Path::new(root)).map_err(failed)
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

/// `org.legeos.Files.Actions1` — the small actions that are otherwise a service menu in every
/// file manager, each implemented slightly differently.
#[derive(Debug)]
pub struct ActionsApi;

#[zbus::interface(name = "org.legeos.Files.Actions1")]
impl ActionsApi {
    /// Render `path` in one of the forms from `Forms`.
    async fn render(&self, path: &str, form: &str) -> fdo::Result<String> {
        let form = PathForm::parse(form)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown path form {form}")))?;
        actions::render(Path::new(path), form).map_err(failed)
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
    use lege_state::Store;
    use std::time::Duration;

    fn service(dir: &Path) -> FileService {
        let store = Store::in_memory().unwrap();
        FileService::new(
            Arc::new(MemoryJournal::default()),
            ExecutorConfig {
                data_home: dir.join("data"),
                reserve_percent: 0,
                progress_interval: Duration::from_millis(0),
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
}
