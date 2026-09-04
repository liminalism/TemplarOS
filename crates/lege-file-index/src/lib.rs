//! The shared file index: one derived, per-user picture of the filesystem that every LegeOS
//! search surface reads instead of walking the disk itself.
//!
//! Three things distinguish it from the delete-and-reinsert index it replaces.
//!
//! **Paths are bytes.** A Unix path is not text, and the moment one becomes a `String` it stops
//! being able to name the file it came from. [`model::NativePath`] is the storage and IPC form;
//! the display string is drawn, never opened.
//!
//! **Freshness is a state, not a promise.** No watcher can prove what happened while the daemon
//! was stopped, and no scan of a live tree is instantaneous. So a root is `current` only after a
//! *complete* pass, and every other condition — a budget, a permission denial, an overflow, an
//! unmounted volume — has its own state and its own reason. A truncated root is never returned as
//! complete; that is the bug this crate exists to fix.
//!
//! **A rebuild cannot cost you the index you already have.** A scan writes into staging rows under
//! a new generation and publishes them in one transaction. Kill it at any point and the previous
//! generation is still there, still searchable, still correct.
//!
//! The crate is pure: no async runtime, no D-Bus, no toolkit. `lege-filed` hosts it.

#![forbid(unsafe_code)]

pub mod btrfs_delta;
pub mod coalesce;
pub mod error;
pub mod generation;
pub mod model;
pub mod mutation;
pub mod query;
pub mod reconcile;
pub mod scan;
pub mod schema;
pub mod snapshot;
pub mod watch;

pub use error::{IndexError, Result};
pub use generation::{
    CancelToken, ScanBudget, ScanOptions, ScanOutcome, ScanSession, ScannedEntry,
};
pub use model::{
    BudgetKind, EntryId, EntryKind, EntryMeta, EntryRow, Generation, NativePath, ObjectKey, RootId,
    RootPolicy, RootState, RootStatus, ScanScope, StaleReason, VolumeIdentity, WatchStatus,
};
pub use mutation::{
    Applied, Change, ChangeOp, ChangeSink, ChangesBatch, Mutation, MutationBatch, MutationSource,
};
pub use query::{Cursor, CursorId, FieldMask, Hit, Page, Query, Rank, SearchResult, Sort};

use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

/// How the index behaves, as opposed to what it holds.
#[derive(Clone, Debug)]
pub struct IndexConfig {
    /// Read connections kept open alongside the single writer.
    pub read_connections: usize,
    /// Rows kept in the bounded change log before the oldest are dropped.
    pub max_changes: u64,
    /// Live search cursors kept; the least recently used is evicted beyond this.
    pub max_cursors: usize,
    /// Rows a cursor may pin.
    pub max_cursor_rows: usize,
    /// How long an idle cursor survives.
    pub cursor_ttl: std::time::Duration,
    /// Generations a tombstone outlives before a commit may sweep it.
    pub tombstone_retention_generations: u64,
    /// Rows a `LIKE` fallback may examine before it declares the answer truncated.
    pub candidate_limit: usize,
    /// Entries staged before a scan flushes them.
    pub scan_flush_rows: usize,
    /// Mutations applied in one transaction before a batch is split.
    pub max_batch: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            read_connections: 4,
            max_changes: 100_000,
            max_cursors: 32,
            max_cursor_rows: 100_000,
            cursor_ttl: std::time::Duration::from_secs(300),
            tombstone_retention_generations: 2,
            candidate_limit: 50_000,
            scan_flush_rows: 4_096,
            max_batch: 10_000,
        }
    }
}

/// What `lege doctor` and `lege files index status` report.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Diagnostics {
    pub path: String,
    pub schema_version: u32,
    pub format_version: u32,
    pub scanner_version: u32,
    pub db_bytes: u64,
    pub generation: Generation,
    pub planner: String,
    pub fts_available: bool,
    pub change_seq_floor: u64,
    pub change_seq_head: u64,
    pub cursor_count: usize,
    pub roots: Vec<RootStatus>,
}

struct Inner {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    config: IndexConfig,
    fts: bool,
    sink: RwLock<Option<Arc<dyn ChangeSink>>>,
    cursors: Mutex<query::CursorStore>,
    scanning: Mutex<HashSet<RootId>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Index")
            .field("path", &self.path)
            .field("fts", &self.fts)
            .finish_non_exhaustive()
    }
}

/// The index handle. Cheap to clone, `Send + Sync`, and entirely blocking: one writer serialized
/// behind a mutex, reads from a WAL pool that a long scan never blocks.
#[derive(Clone, Debug)]
pub struct Index(Arc<Inner>);

impl Index {
    /// Opens (creating and migrating if needed) the index at `path`.
    ///
    /// A database this build cannot understand is **rebuilt**, not rejected: these rows are
    /// derived from the filesystem, so the worst a rebuild costs is a rescan. The old file is
    /// moved aside as `*.corrupt-<ms>` when that is possible, so a bug leaves evidence.
    pub fn open(path: &Path, config: IndexConfig) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let writer = match Self::open_migrated(path) {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "rebuilding the file index");
                Self::move_aside(path);
                Self::open_migrated(path)?
            }
        };
        let fts = schema::ensure_fts(&writer);

        let index = Self(Arc::new(Inner {
            path: path.to_path_buf(),
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
            config,
            fts,
            sink: RwLock::new(None),
            cursors: Mutex::new(query::CursorStore::default()),
            scanning: Mutex::new(HashSet::new()),
        }));
        index.sweep_aborted_scans()?;
        Ok(index)
    }

    fn open_migrated(path: &Path) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&connection)?;
        Ok(connection)
    }

    fn move_aside(path: &Path) {
        let aside = path.with_extension(format!("corrupt-{}", model::now_ms()));
        if std::fs::rename(path, &aside).is_err() {
            let _ = std::fs::remove_file(path);
        }
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(sidecar));
        }
    }

    #[must_use]
    pub fn config(&self) -> &IndexConfig {
        &self.0.config
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Whether the FTS5 trigram accelerator is present. When it is not, substring search still
    /// works — through a bounded candidate scan that says so.
    #[must_use]
    pub fn fts_available(&self) -> bool {
        self.0.fts
    }

    /// Where the maintenance lane, and the D-Bus host, learn that something changed.
    pub fn set_change_sink(&self, sink: Arc<dyn ChangeSink>) {
        *self.0.sink.write() = Some(sink);
    }

    pub(crate) fn sink(&self) -> Option<Arc<dyn ChangeSink>> {
        self.0.sink.read().clone()
    }

    pub(crate) fn with_write<T>(
        &self,
        body: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.0.writer.lock();
        body(&mut guard)
    }

    pub(crate) fn with_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = {
            let mut pool = self.0.readers.lock();
            pool.pop()
        };
        let connection = if let Some(connection) = connection {
            connection
        } else {
            let connection = Connection::open_with_flags(
                &self.0.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            connection
        };
        let outcome = body(&connection);
        let mut pool = self.0.readers.lock();
        if pool.len() < self.0.config.read_connections {
            pool.push(connection);
        }
        outcome
    }

    pub(crate) fn cursors(&self) -> &Mutex<query::CursorStore> {
        &self.0.cursors
    }

    /// Claims the right to walk a root. The maintenance lane treats the refusal as "someone else
    /// is walking this", arms its watches and leaves the state to that walker.
    pub(crate) fn claim_scan(&self, root: RootId) -> Result<ScanClaim> {
        let mut scanning = self.0.scanning.lock();
        if !scanning.insert(root) {
            return Err(IndexError::ScanInFlight(root));
        }
        Ok(ScanClaim {
            index: self.clone(),
            root,
        })
    }

    pub(crate) fn release_scan(&self, root: RootId) {
        self.0.scanning.lock().remove(&root);
    }

    /// Drops staging rows left by a scan that never committed, and returns each root it repaired
    /// to the state it had before that scan started.
    ///
    /// This is what makes a killed rebuild free: the committed generation was never touched, so
    /// there is nothing to recover — only rubbish to sweep.
    fn sweep_aborted_scans(&self) -> Result<usize> {
        self.with_write(|connection| {
            let transaction = connection.transaction()?;
            let abandoned: Vec<i64> = transaction
                .prepare("SELECT root_id FROM roots WHERE building_generation IS NOT NULL")?
                .query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            for root in &abandoned {
                transaction.execute("DELETE FROM entries_staging WHERE root_id = ?1", [root])?;
                // A root that was mid-first-scan has nothing committed: it is empty again.
                transaction.execute(
                    "UPDATE roots
                        SET building_generation = NULL,
                            state = CASE WHEN committed_generation = 0 THEN 'empty' ELSE 'stale' END,
                            state_reason = CASE WHEN committed_generation = 0
                                                THEN NULL ELSE 'interrupted-scan' END
                      WHERE root_id = ?1",
                    [root],
                )?;
            }
            transaction.commit()?;
            if !abandoned.is_empty() {
                tracing::info!(
                    roots = abandoned.len(),
                    "swept staging rows from an interrupted scan; the previous generation is intact"
                );
            }
            Ok(abandoned.len())
        })
    }

    // ---- roots -------------------------------------------------------------------------------

    /// Registers a root. Registering does not scan it: it starts `empty`, and something has to
    /// decide when walking it is appropriate.
    // `policy` is taken by value: this is the frozen C5 contract signature (F1-contract.md §4),
    // which F2/Files/CLI already code against, and only borrows `policy` internally.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_root(&self, path: &NativePath, policy: RootPolicy) -> Result<RootStatus> {
        let policy_json = serde_json::to_string(&policy)?;
        let root_id = self.with_write(|connection| {
            let existing: Option<i64> = connection
                .query_row(
                    "SELECT root_id FROM roots WHERE path_bytes = ?1",
                    [path.as_bytes()],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            if let Some(root_id) = existing {
                return Ok(RootId(root_id));
            }
            connection.execute(
                "INSERT INTO roots (path_bytes, display, cross_mount, policy_json, policy_hash,
                                    state, committed_generation, complete, entry_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'empty', 0, 0, 0)",
                rusqlite::params![
                    path.as_bytes(),
                    path.display(),
                    i64::from(policy.cross_mount),
                    policy_json,
                    policy.hash(),
                ],
            )?;
            Ok(RootId(connection.last_insert_rowid()))
        })?;
        self.root_status(root_id)
    }

    /// Forgets a root and every row derived from it.
    pub fn forget_root(&self, root: RootId) -> Result<()> {
        self.with_write(|connection| {
            connection.execute("DELETE FROM roots WHERE root_id = ?1", [root.0])?;
            connection.execute("DELETE FROM entries WHERE root_id = ?1", [root.0])?;
            connection.execute("DELETE FROM entries_staging WHERE root_id = ?1", [root.0])?;
            Ok(())
        })?;
        self.0.cursors.lock().invalidate_root(root);
        Ok(())
    }

    pub fn roots(&self) -> Result<Vec<RootStatus>> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {ROOT_COLUMNS} FROM roots ORDER BY root_id"
            ))?;
            let rows = statement.query_map([], root_status_from_row)?;
            let mut roots = Vec::new();
            for row in rows {
                roots.push(row?);
            }
            Ok(roots)
        })
    }

    pub fn root_status(&self, root: RootId) -> Result<RootStatus> {
        self.with_read(|connection| read_root(connection, root))
    }

    /// Looks a root up by the path it was registered as.
    pub fn root_by_path(&self, path: &NativePath) -> Result<Option<RootStatus>> {
        self.with_read(|connection| {
            connection
                .query_row(
                    &format!("SELECT {ROOT_COLUMNS} FROM roots WHERE path_bytes = ?1"),
                    [path.as_bytes()],
                    root_status_from_row,
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(IndexError::Sqlite(other)),
                })
        })
    }

    /// The root that owns `path`: the longest registered prefix of it.
    pub fn root_for_path(&self, path: &NativePath) -> Result<Option<RootStatus>> {
        let mut best: Option<RootStatus> = None;
        for root in self.roots()? {
            if path.starts_with(&root.path)
                && best.as_ref().is_none_or(|current| {
                    root.path.as_bytes().len() > current.path.as_bytes().len()
                })
            {
                best = Some(root);
            }
        }
        Ok(best)
    }

    pub fn root_policy(&self, root: RootId) -> Result<RootPolicy> {
        self.with_read(|connection| {
            let json: String = connection.query_row(
                "SELECT policy_json FROM roots WHERE root_id = ?1",
                [root.0],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str(&json)?)
        })
    }

    /// Changes what a root includes. The stored rows were produced under the old rules and cannot
    /// be compared with rows produced under the new ones, so this marks the root stale in the same
    /// transaction. It never deletes anything.
    // Frozen C5 contract signature (F1-contract.md §4); see `add_root` above.
    #[allow(clippy::needless_pass_by_value)]
    pub fn set_root_policy(&self, root: RootId, policy: RootPolicy) -> Result<RootStatus> {
        let json = serde_json::to_string(&policy)?;
        self.with_write(|connection| {
            let transaction = connection.transaction()?;
            let changed: usize = transaction.execute(
                "UPDATE roots SET policy_json = ?2, policy_hash = ?3, cross_mount = ?4
                  WHERE root_id = ?1 AND policy_hash <> ?3",
                rusqlite::params![root.0, json, policy.hash(), i64::from(policy.cross_mount)],
            )?;
            if changed > 0 {
                let current = read_root(&transaction, root)?;
                if current.state != RootState::Empty {
                    set_state(
                        &transaction,
                        root,
                        RootState::Stale,
                        Some(&StaleReason::PolicyChanged.as_string()),
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })?;
        let status = self.root_status(root)?;
        self.notify_root(&status);
        Ok(status)
    }

    pub fn set_volume_identity(&self, root: RootId, volume: &VolumeIdentity) -> Result<()> {
        self.with_write(|connection| {
            connection.execute(
                "UPDATE roots SET volume_dev_id = ?2, volume_uuid = ?3, volume_mount = ?4,
                                  volume_label = ?5
                  WHERE root_id = ?1",
                rusqlite::params![
                    root.0,
                    volume.dev_id.map(|id| id as i64),
                    volume.uuid,
                    volume.mount_point.as_ref().map(NativePath::as_bytes),
                    volume.label,
                ],
            )?;
            Ok(())
        })
    }

    /// The watcher's self-report, kept next to the rows it maintains.
    pub fn set_watch_status(&self, root: RootId, status: &WatchStatus) -> Result<()> {
        let json = serde_json::to_string(status)?;
        self.with_write(|connection| {
            connection.execute(
                "UPDATE roots SET watch_json = ?2 WHERE root_id = ?1",
                rusqlite::params![root.0, json],
            )?;
            Ok(())
        })
    }

    // ---- freshness transitions --------------------------------------------------------------

    pub fn mark_stale(&self, root: RootId, reason: &StaleReason) -> Result<RootStatus> {
        let reason = reason.as_string();
        self.transition(root, RootState::Stale, Some(reason.as_str()))
    }

    pub fn mark_offline(&self, root: RootId, reason: &str) -> Result<RootStatus> {
        self.transition(root, RootState::Offline, Some(reason))
    }

    /// A volume that came back. Deliberately lands in `stale`, never `current`: nothing has
    /// re-enumerated it yet, and claiming otherwise is exactly the lie this crate refuses.
    pub fn mark_online(&self, root: RootId) -> Result<RootStatus> {
        let reason = StaleReason::MountDisappeared.as_string();
        self.transition(root, RootState::Stale, Some(reason.as_str()))
    }

    /// The committed snapshot is good and a queued backlog is being applied.
    pub fn begin_catch_up(&self, root: RootId) -> Result<RootStatus> {
        self.transition(root, RootState::CatchingUp, None)
    }

    /// The backlog drained. Reaches `current` only if the last commit was complete; otherwise the
    /// root stays honestly stale.
    pub fn finish_catch_up(&self, root: RootId) -> Result<RootStatus> {
        let status = self.root_status(root)?;
        if status.complete {
            self.transition(root, RootState::Current, None)
        } else {
            let fallback = StaleReason::PermissionDenied.as_string();
            self.transition(
                root,
                RootState::Stale,
                status.reason.as_deref().or(Some(fallback.as_str())),
            )
        }
    }

    pub(crate) fn transition(
        &self,
        root: RootId,
        state: RootState,
        reason: Option<&str>,
    ) -> Result<RootStatus> {
        self.with_write(|connection| {
            let transaction = connection.transaction()?;
            set_state(&transaction, root, state, reason)?;
            transaction.commit()?;
            Ok(())
        })?;
        let status = self.root_status(root)?;
        self.notify_root(&status);
        Ok(status)
    }

    pub(crate) fn notify_root(&self, status: &RootStatus) {
        if let Some(sink) = self.sink() {
            sink.root_state_changed(status);
        }
    }

    // ---- point reads -------------------------------------------------------------------------

    /// One row by its path. The lookup the maintenance lane does for every watcher event.
    pub fn entry(&self, root: RootId, path: &NativePath) -> Result<Option<EntryRow>> {
        self.with_read(|connection| {
            connection
                .query_row(
                    &format!(
                        "SELECT {ENTRY_COLUMNS} FROM entries
                          WHERE root_id = ?1 AND path_bytes = ?2 AND deleted_generation IS NULL"
                    ),
                    rusqlite::params![root.0, path.as_bytes()],
                    entry_row_from_row,
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(IndexError::Sqlite(other)),
                })
        })
    }

    /// Every row sharing a filesystem object key. A `Vec`, because hard links and inode reuse make
    /// the key non-unique — treating it as unique is how an index deletes the wrong row.
    pub fn lookup_object(&self, root: RootId, key: ObjectKey) -> Result<Vec<EntryRow>> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM entries
                  WHERE root_id = ?1 AND dev = ?2 AND inode = ?3 AND deleted_generation IS NULL"
            ))?;
            let rows = statement.query_map(
                rusqlite::params![root.0, key.dev as i64, key.ino as i64],
                entry_row_from_row,
            )?;
            let mut entries = Vec::new();
            for row in rows {
                entries.push(row?);
            }
            Ok(entries)
        })
    }

    /// What the index currently holds directly under `directory` — the other half of a
    /// reconciliation diff.
    pub fn children(&self, root: RootId, directory: &NativePath) -> Result<Vec<EntryRow>> {
        let (low, high) = directory.subtree_bounds();
        let depth = separators(&low);
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM entries
                  WHERE root_id = ?1 AND path_bytes >= ?2 AND path_bytes < ?3
                    AND deleted_generation IS NULL
                  ORDER BY path_bytes"
            ))?;
            let rows =
                statement.query_map(rusqlite::params![root.0, low, high], entry_row_from_row)?;
            let mut entries = Vec::new();
            for row in rows {
                let entry = row?;
                if separators(entry.path.as_bytes()) == depth {
                    entries.push(entry);
                }
            }
            Ok(entries)
        })
    }

    /// How many live rows a root holds.
    pub fn entry_count(&self, root: RootId) -> Result<u64> {
        self.with_read(|connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM entries WHERE root_id = ?1 AND deleted_generation IS NULL",
                [root.0],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    /// The database-wide committed generation.
    pub fn generation(&self) -> Result<Generation> {
        self.with_read(|connection| {
            Ok(Generation(
                schema::read_meta(connection, "committed_generation")?
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
            ))
        })
    }

    pub fn diagnostics(&self) -> Result<Diagnostics> {
        let roots = self.roots()?;
        let cursor_count = self.0.cursors.lock().len();
        let db_bytes = std::fs::metadata(&self.0.path).map_or(0, |meta| meta.len());
        self.with_read(|connection| {
            let head: i64 = connection
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |row| {
                    row.get(0)
                })
                .unwrap_or(0);
            Ok(Diagnostics {
                path: self.0.path.display().to_string(),
                schema_version: schema::schema_version(),
                format_version: schema::FORMAT_VERSION,
                scanner_version: schema::SCANNER_VERSION,
                db_bytes,
                generation: Generation(
                    schema::read_meta(connection, "committed_generation")?
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0),
                ),
                planner: if self.0.fts {
                    "fts5-trigram".to_owned()
                } else {
                    "like-scan".to_owned()
                },
                fts_available: self.0.fts,
                change_seq_floor: schema::read_meta(connection, "min_retained_change_seq")?
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
                change_seq_head: head as u64,
                cursor_count,
                roots,
            })
        })
    }
}

/// Holds a root's scan claim for as long as the session lives.
#[derive(Debug)]
pub(crate) struct ScanClaim {
    index: Index,
    root: RootId,
}

impl Drop for ScanClaim {
    fn drop(&mut self) {
        self.index.release_scan(self.root);
    }
}

pub(crate) const ROOT_COLUMNS: &str = "root_id, path_bytes, display, volume_dev_id, volume_uuid, \
     volume_mount, volume_label, policy_hash, state, state_reason, committed_generation, complete, \
     entry_count, last_scan_ms, last_reconcile_started_ms, last_reconcile_finished_ms, last_error, \
     watch_json";

pub(crate) const ENTRY_COLUMNS: &str = "entry_id, root_id, parent_id, name_bytes, name_display, \
     path_bytes, path_display, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden, \
     dev, inode, nlink, mime, written_generation, last_seen_generation, deleted_generation";

pub(crate) fn root_status_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RootStatus> {
    let watch_json: Option<String> = row.get(17)?;
    Ok(RootStatus {
        root: RootId(row.get(0)?),
        path: NativePath::from_bytes(row.get::<_, Vec<u8>>(1)?),
        display: row.get(2)?,
        volume: VolumeIdentity {
            dev_id: row.get::<_, Option<i64>>(3)?.map(|id| id as u64),
            uuid: row.get(4)?,
            mount_point: row
                .get::<_, Option<Vec<u8>>>(5)?
                .map(NativePath::from_bytes),
            label: row.get(6)?,
        },
        policy_hash: row.get(7)?,
        state: RootState::from_str_or_stale(&row.get::<_, String>(8)?),
        reason: row.get(9)?,
        generation: Generation(row.get::<_, i64>(10)? as u64),
        complete: row.get::<_, i64>(11)? != 0,
        entry_count: row.get::<_, i64>(12)? as u64,
        last_scan_ms: row.get(13)?,
        last_reconcile_started_ms: row.get(14)?,
        last_reconcile_finished_ms: row.get(15)?,
        last_error: row.get(16)?,
        watch: watch_json.and_then(|json| serde_json::from_str(&json).ok()),
    })
}

pub(crate) fn entry_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    let dev: Option<i64> = row.get(14)?;
    let inode: Option<i64> = row.get(15)?;
    Ok(EntryRow {
        entry_id: EntryId(row.get(0)?),
        root: RootId(row.get(1)?),
        parent: row.get::<_, Option<i64>>(2)?.map(EntryId),
        name: NativePath::from_bytes(row.get::<_, Vec<u8>>(3)?),
        display: row.get(4)?,
        path: NativePath::from_bytes(row.get::<_, Vec<u8>>(5)?),
        meta: EntryMeta {
            kind: EntryKind::from_i64(row.get(7)?),
            size_bytes: row.get::<_, i64>(8)? as u64,
            alloc_bytes: row.get::<_, i64>(9)? as u64,
            mtime_ns: row.get(10)?,
            ctime_ns: row.get(11)?,
            mode: row.get::<_, i64>(12)? as u32,
            hidden: row.get::<_, i64>(13)? != 0,
            object_key: match (dev, inode) {
                (Some(dev), Some(ino)) => Some(ObjectKey {
                    dev: dev as u64,
                    ino: ino as u64,
                }),
                _ => None,
            },
            nlink: row.get::<_, i64>(16)? as u32,
            mime: row.get(17)?,
        },
        written_generation: Generation(row.get::<_, i64>(18)? as u64),
        last_seen_generation: Generation(row.get::<_, i64>(19)? as u64),
        deleted_generation: row
            .get::<_, Option<i64>>(20)?
            .map(|value| Generation(value as u64)),
    })
}

pub(crate) fn read_root(connection: &Connection, root: RootId) -> Result<RootStatus> {
    connection
        .query_row(
            &format!("SELECT {ROOT_COLUMNS} FROM roots WHERE root_id = ?1"),
            [root.0],
            root_status_from_row,
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => IndexError::UnknownRoot(root),
            other => IndexError::Sqlite(other),
        })
}

/// The single gate every state change goes through. An illegal pair is an error, not a silent
/// no-op: a state machine that accepts anything eventually claims `current` for an unscanned root.
pub(crate) fn set_state(
    connection: &Connection,
    root: RootId,
    state: RootState,
    reason: Option<&str>,
) -> Result<()> {
    let current = read_root(connection, root)?;
    if !current.state.can_transition_to(state) {
        return Err(IndexError::IllegalTransition {
            root,
            from: current.state,
            to: state,
        });
    }
    connection.execute(
        "UPDATE roots SET state = ?2, state_reason = ?3 WHERE root_id = ?1",
        rusqlite::params![root.0, state.as_str(), reason],
    )?;
    Ok(())
}

/// Allocates the next generation. Monotone across the whole database, so any two answers are
/// comparable regardless of which root produced them.
pub(crate) fn next_generation(connection: &Connection) -> Result<Generation> {
    let current: u64 = schema::read_meta(connection, "generation_counter")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let next = current + 1;
    schema::write_meta(connection, "generation_counter", &next.to_string())?;
    Ok(Generation(next))
}

// A path's separator count is a handful of bytes at most; pulling in the `bytecount` crate for
// this would violate the crate's minimal-dependency contract (F1-contract.md preamble) for no
// measurable gain.
#[allow(clippy::naive_bytecount)]
pub(crate) fn separators(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'/').count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::{Mutation, MutationBatch, MutationSource};
    use std::os::unix::ffi::OsStringExt;

    fn index_at(dir: &Path) -> Index {
        Index::open(&dir.join("file-index-v1.sqlite3"), IndexConfig::default()).unwrap()
    }

    fn tree(root: &Path) {
        std::fs::create_dir_all(root.join("notes/2026")).unwrap();
        std::fs::write(root.join("report.pdf"), b"a").unwrap();
        std::fs::write(root.join("notes/2026/annual-report.md"), b"b").unwrap();
        std::fs::write(root.join("notes/report_final"), b"c").unwrap();
    }

    fn scan(index: &Index, root: &Path) -> (RootId, ScanOutcome) {
        let native = NativePath::from_path(root);
        let status = index.add_root(&native, RootPolicy::default_home()).unwrap();
        let outcome = index
            .scan_root(status.root, ScanOptions::default())
            .unwrap();
        (status.root, outcome)
    }

    #[test]
    fn a_complete_scan_is_the_only_route_to_current() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, outcome) = scan(&index, &home);

        assert!(outcome.complete, "an unobstructed scan is complete");
        assert_eq!(index.root_status(root).unwrap().state, RootState::Current);
        assert!(
            !index.search(&Query::new("report")).unwrap().hits.is_empty(),
            "the scan indexed nothing"
        );
    }

    /// The invariant the old 50 000-entry cap violated: a truncated root must never be served as
    /// complete, and must never let collection delete what the pass did not look at.
    #[test]
    fn a_budget_makes_a_pass_incomplete_and_suppresses_collection() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        for child in 0..20 {
            std::fs::write(home.join(format!("file-{child}")), b"x").unwrap();
        }
        let index = index_at(dir.path());
        let native = NativePath::from_path(&home);
        let root = index
            .add_root(&native, RootPolicy::default_home())
            .unwrap()
            .root;

        let outcome = index
            .scan_root(
                root,
                ScanOptions {
                    budget: ScanBudget::unlimited().with_max_entries(5),
                    ..ScanOptions::default()
                },
            )
            .unwrap();
        assert!(
            !outcome.complete,
            "a budget-limited pass claimed completeness"
        );
        assert_eq!(outcome.limited, Some(BudgetKind::Entries));
        let status = index.root_status(root).unwrap();
        assert_eq!(status.state, RootState::Stale);
        assert_eq!(status.reason.as_deref(), Some("budget:entries"));
        assert!(!status.complete);

        // A second, unbudgeted pass converges without anyone deleting the database.
        let outcome = index.scan_root(root, ScanOptions::default()).unwrap();
        assert!(outcome.complete);
        assert_eq!(index.root_status(root).unwrap().state, RootState::Current);
        assert!(
            outcome.entries >= 21,
            "the whole tree should be indexed now"
        );
    }

    #[test]
    fn a_depth_budget_bounds_a_branch_without_ending_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mut deep = home.clone();
        for level in 0..10 {
            deep = deep.join(format!("level{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("buried.txt"), b"x").unwrap();
        std::fs::write(home.join("shallow.txt"), b"y").unwrap();

        let index = index_at(dir.path());
        let native = NativePath::from_path(&home);
        let root = index
            .add_root(&native, RootPolicy::default_home())
            .unwrap()
            .root;
        let outcome = index
            .scan_root(
                root,
                ScanOptions {
                    budget: ScanBudget::unlimited().with_max_depth(3),
                    ..ScanOptions::default()
                },
            )
            .unwrap();

        assert!(!outcome.complete, "a depth-bounded pass is not complete");
        assert_eq!(outcome.limited, Some(BudgetKind::Depth));
        assert!(
            index.search(&Query::new("buried")).unwrap().hits.is_empty(),
            "the depth budget did not bound the branch"
        );
        assert!(
            !index
                .search(&Query::new("shallow"))
                .unwrap()
                .hits
                .is_empty(),
            "a depth budget must not abandon the rest of the tree"
        );
        assert_eq!(
            index.root_status(root).unwrap().reason.as_deref(),
            Some("budget:depth")
        );
    }

    /// Kill a rebuild at its worst moment and the previous generation must still be there. This is
    /// the whole reason a scan writes to staging instead of deleting first.
    #[test]
    fn a_generation_commit_survives_a_killed_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);

        let index = index_at(dir.path());
        let (root, first) = scan(&index, &home);
        let before = index.search(&Query::new("report")).unwrap();
        assert!(!before.hits.is_empty());

        // A rebuild that gets as far as staging rows and then dies with the process.
        let mut session = index.begin_scan(root, ScanOptions::default()).unwrap();
        let doomed = first.generation;
        session
            .observe(ScannedEntry {
                path: NativePath::from_path(&home.join("half-written.txt")),
                parent: Some(NativePath::from_path(&home)),
                meta: EntryMeta::default(),
            })
            .unwrap();
        assert!(session.generation() > doomed);
        std::mem::forget(session); // no unwinding, no Drop: exactly what SIGKILL looks like
        drop(index);

        let index = index_at(dir.path());
        let after = index.search(&Query::new("report")).unwrap();
        assert_eq!(
            after.hits.len(),
            before.hits.len(),
            "the previous generation did not survive the killed rebuild"
        );
        assert_eq!(
            index.root_status(root).unwrap().generation,
            first.generation,
            "an uncommitted generation was published"
        );
        assert!(
            index
                .search(&Query::new("half-written"))
                .unwrap()
                .hits
                .is_empty(),
            "staging rows from a killed scan became visible"
        );
        // And the daemon converges without anyone deleting the database by hand.
        assert!(
            index
                .scan_root(root, ScanOptions::default())
                .unwrap()
                .complete
        );
    }

    /// A path is bytes. If it survives indexing, searching and the bulk snapshot unchanged, an
    /// action built on it opens the file the user pointed at rather than a lookalike.
    #[test]
    fn a_non_utf8_path_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let raw_name = b"caf\xff\xfe-report.txt".to_vec();
        let raw_path = home.join(std::ffi::OsString::from_vec(raw_name.clone()));
        std::fs::write(&raw_path, b"x").unwrap();
        let expected = NativePath::from_path(&raw_path);
        assert!(
            expected.display().contains('\u{fffd}'),
            "the fixture is not actually lossy"
        );

        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);

        let hits = index.search(&Query::new("report")).unwrap().hits;
        let found = hits
            .iter()
            .find(|hit| hit.row.path == expected)
            .expect("the non-UTF-8 path did not survive indexing and search");
        assert_eq!(found.row.path.as_bytes(), expected.as_bytes());
        assert_eq!(found.row.name.as_bytes(), raw_name.as_slice());
        assert!(
            std::fs::metadata(found.row.path.to_path()).is_ok(),
            "the stored path no longer opens the file"
        );

        let entry = index.entry(root, &expected).unwrap();
        assert_eq!(entry.map(|row| row.path), Some(expected.clone()));

        let mut frame = Vec::new();
        index
            .write_snapshot(&mut frame, &snapshot::SnapshotRequest::default())
            .unwrap();
        let reader = snapshot::SnapshotReader::new(&frame).unwrap();
        assert!(
            reader.entry_paths().unwrap().contains(&expected),
            "the non-UTF-8 path did not survive the snapshot"
        );
    }

    /// A cursor names one generation and keeps naming it. Scrolling a result list must not
    /// silently switch to newer data underneath the person reading it.
    #[test]
    fn a_cursor_stays_pinned_to_its_generation() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);

        let result = index
            .search(&Query::new("report").with_page_size(1))
            .unwrap();
        assert_eq!(result.hits.len(), 1, "page size was not honoured");
        let pinned = result.generation;
        let total = result.total.unwrap();
        assert!(total >= 3, "the fixture should hold three matching names");

        // The world moves on: a new file, and one of the pinned rows deleted.
        let new_path = NativePath::from_path(&home.join("report-extra.txt"));
        let applied = index
            .apply(MutationBatch::new(
                MutationSource::Operation,
                vec![
                    Mutation::Upsert {
                        root,
                        path: new_path,
                        meta: EntryMeta::default(),
                    },
                    Mutation::Remove {
                        root,
                        path: NativePath::from_path(&home.join("report.pdf")),
                    },
                ],
            ))
            .unwrap();
        assert!(
            applied.generation > pinned,
            "the batch did not advance the generation"
        );

        let page = index.fetch_page(&result.cursor, 0, 10).unwrap();
        assert_eq!(
            page.generation, pinned,
            "the cursor drifted to a newer generation"
        );
        assert_eq!(
            page.hits.len(),
            total as usize,
            "the pinned set changed size"
        );
        assert_eq!(page.dropped, 1, "a removed row was not reported as absent");
        assert!(
            page.hits.iter().any(|hit| !hit.present),
            "the removed row silently vanished from the page instead of being marked absent"
        );

        // A fresh search does see the new world.
        let again = index.search(&Query::new("report")).unwrap();
        assert_eq!(again.generation, applied.generation);

        index.close_cursor(&result.cursor);
        assert!(
            matches!(
                index.fetch_page(&result.cursor, 0, 10),
                Err(IndexError::SnapshotRequired { .. })
            ),
            "a closed cursor must ask for a new snapshot, not answer from newer data"
        );
    }

    /// The point of a durable index: starting again serves what was already known, immediately,
    /// without walking a single directory.
    #[test]
    fn a_warm_start_serves_without_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);

        let index = index_at(dir.path());
        let (root, first) = scan(&index, &home);
        let expected = index.search(&Query::new("report")).unwrap().hits.len();
        drop(index);

        // A file appears while nothing is running. A warm start must serve the old generation —
        // and must not claim to know about the new file.
        std::fs::write(home.join("appeared-while-stopped.txt"), b"z").unwrap();

        let index = index_at(dir.path());
        let status = index.root_status(root).unwrap();
        assert_eq!(status.state, RootState::Current);
        assert_eq!(
            status.generation, first.generation,
            "a warm start rescanned"
        );
        assert_eq!(
            index.search(&Query::new("report")).unwrap().hits.len(),
            expected,
            "a warm start did not serve the committed generation"
        );
        assert!(
            index
                .search(&Query::new("appeared-while-stopped"))
                .unwrap()
                .hits
                .is_empty(),
            "the index claimed knowledge of a file it has never seen"
        );
        assert_eq!(index.entry_count(root).unwrap(), status.entry_count);
    }

    #[test]
    fn an_operation_echo_is_deduplicated_rather_than_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);

        let path = NativePath::from_path(&home.join("report.pdf"));
        let stored = index.entry(root, &path).unwrap().unwrap();
        let before = index.generation().unwrap();

        // The watcher's echo of something the file service already applied.
        let applied = index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Upsert {
                    root,
                    path,
                    meta: stored.meta.clone(),
                }],
            ))
            .unwrap();
        assert_eq!(
            applied.skipped, 1,
            "a redundant mutation was applied anyway"
        );
        assert!(applied.changes.is_empty());
        assert_eq!(
            index.generation().unwrap(),
            before,
            "a no-op batch advanced the generation"
        );
    }

    #[test]
    fn moving_a_directory_carries_its_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);

        let from = NativePath::from_path(&home.join("notes"));
        let to = NativePath::from_path(&home.join("archive"));
        let meta = index.entry(root, &from).unwrap().unwrap().meta;
        let applied = index
            .apply(MutationBatch::new(
                MutationSource::Operation,
                vec![Mutation::Move {
                    root,
                    from: from.clone(),
                    to: to.clone(),
                    meta: meta.clone(),
                }],
            ))
            .unwrap();
        assert_eq!(
            applied.changes.len(),
            1,
            "a folder move is one event, not one per file"
        );
        assert_eq!(applied.changes[0].op, ChangeOp::Moved);
        assert_eq!(applied.changes[0].old_path.as_ref(), Some(&from));
        assert!(applied.moved_descendants >= 3);

        assert!(index.entry(root, &from).unwrap().is_none());
        assert!(
            index
                .entry(
                    root,
                    &NativePath::from_path(&home.join("archive/2026/annual-report.md"))
                )
                .unwrap()
                .is_some(),
            "a descendant kept its old path after the directory moved"
        );

        // The echo of that same move, arriving later from a watcher, changes nothing.
        let echo = index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Move {
                    root,
                    from,
                    to,
                    meta,
                }],
            ))
            .unwrap();
        assert_eq!(echo.skipped, 1, "the echo of a move was replayed");
    }

    #[test]
    fn an_overflow_marks_the_root_stale_and_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);
        let before = index.entry_count(root).unwrap();

        index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Invalidate {
                    root,
                    subtree: NativePath::from_path(&home),
                    reason: StaleReason::WatcherOverflow,
                }],
            ))
            .unwrap();

        let status = index.root_status(root).unwrap();
        assert_eq!(status.state, RootState::Stale);
        assert_eq!(status.reason.as_deref(), Some("watcher-overflow"));
        assert_eq!(
            index.entry_count(root).unwrap(),
            before,
            "an unproven absence became a deletion"
        );
        // The last known rows stay searchable while the root is stale.
        assert!(!index.search(&Query::new("report")).unwrap().hits.is_empty());
    }

    #[test]
    fn an_offline_root_keeps_its_rows_and_cannot_become_current_without_a_scan() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);

        index.mark_offline(root, "unmounted").unwrap();
        assert!(!index.search(&Query::new("report")).unwrap().hits.is_empty());
        assert!(
            matches!(
                index.transition(root, RootState::Current, None),
                Err(IndexError::IllegalTransition { .. })
            ),
            "an offline root was allowed to claim it was current"
        );
        assert_eq!(index.mark_online(root).unwrap().state, RootState::Stale);
    }

    #[test]
    fn the_change_log_is_bounded_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let index = Index::open(
            &dir.path().join("file-index-v1.sqlite3"),
            IndexConfig {
                max_changes: 8,
                ..IndexConfig::default()
            },
        )
        .unwrap();
        let root = index
            .add_root(&NativePath::from_path(&home), RootPolicy::default_home())
            .unwrap()
            .root;

        for index_of in 0..20 {
            index
                .apply(MutationBatch::new(
                    MutationSource::Operation,
                    vec![Mutation::Upsert {
                        root,
                        path: NativePath::from_path(&home.join(format!("f{index_of}"))),
                        meta: EntryMeta::default(),
                    }],
                ))
                .unwrap();
        }
        // 20 changes against a retention budget of 8 must have pruned the log, so a client
        // pinned at seq 0 (never having consumed anything) has genuinely lost history: per the
        // frozen contract (F1-contract.md §7), `changes_since` never invents continuity below
        // the retained floor, with no exception for the sentinel starting sequence.
        let floor = index.diagnostics().unwrap().change_seq_floor;
        assert!(floor > 0, "pruning never advanced the retained floor");
        let batch = index.changes_since(floor, 100).unwrap();
        assert!(
            batch.changes.len() <= 8,
            "the change log grew without bound"
        );
        assert!(
            matches!(
                index.changes_since(0, 10),
                Err(IndexError::SnapshotRequired { .. })
            ),
            "a pruned sequence was answered with invented continuity"
        );
    }

    #[test]
    fn a_subtree_repair_collects_only_inside_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        let (root, _) = scan(&index, &home);

        // Something outside the scope disappears from the filesystem; a subtree repair of
        // notes/ must not notice, and must certainly not delete it.
        std::fs::remove_file(home.join("report.pdf")).unwrap();
        let outcome = index
            .scan_root(
                root,
                ScanOptions {
                    scope: ScanScope::Subtree(NativePath::from_path(&home.join("notes"))),
                    reason: Some(StaleReason::Reconcile),
                    ..ScanOptions::default()
                },
            )
            .unwrap();
        assert!(outcome.complete);
        assert!(
            index
                .entry(root, &NativePath::from_path(&home.join("report.pdf")))
                .unwrap()
                .is_some(),
            "a subtree repair collected a row outside its scope"
        );
    }

    #[test]
    fn a_database_this_build_cannot_read_is_rebuilt_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file-index-v1.sqlite3");
        std::fs::write(&path, b"this is not a database").unwrap();
        let index = Index::open(&path, IndexConfig::default()).unwrap();
        assert!(index.roots().unwrap().is_empty());
        assert_eq!(index.generation().unwrap(), Generation(0));
    }

    #[test]
    fn a_search_can_be_restricted_to_a_root_and_an_empty_query_matches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("one");
        let second = dir.path().join("two");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("report-one.txt"), b"a").unwrap();
        std::fs::write(second.join("report-two.txt"), b"b").unwrap();

        let index = index_at(dir.path());
        let (one, _) = scan(&index, &first);
        let (_, _) = scan(&index, &second);

        assert_eq!(index.search(&Query::new("report")).unwrap().hits.len(), 2);
        let scoped = index
            .search(&Query::new("report").in_roots(vec![one]))
            .unwrap();
        assert_eq!(scoped.hits.len(), 1);
        assert_eq!(
            scoped.roots.len(),
            1,
            "a scoped search reports its scope's freshness"
        );
        assert!(index.search(&Query::new("   ")).unwrap().hits.is_empty());
    }

    #[test]
    fn like_wildcards_in_a_query_stay_literal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        scan(&index, &home);

        let hits = index.search(&Query::new("report_")).unwrap().hits;
        assert_eq!(
            hits.iter()
                .filter(|hit| hit.row.name.display() == "report_final")
                .count(),
            1
        );
        assert!(
            hits.iter()
                .all(|hit| hit.row.name.display().contains("report_")),
            "underscore matched as a wildcard"
        );
        assert!(index.search(&Query::new("%")).unwrap().hits.is_empty());
    }

    #[test]
    fn a_snapshot_frame_refuses_to_be_read_after_a_single_flipped_bit() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        scan(&index, &home);

        let mut frame = Vec::new();
        let manifest = index
            .write_snapshot(&mut frame, &snapshot::SnapshotRequest::default())
            .unwrap();
        assert_eq!(manifest.format, snapshot::FORMAT);
        assert!(manifest.entry_count >= 4);
        let reader = snapshot::SnapshotReader::new(&frame).unwrap();
        assert_eq!(reader.entry_count, manifest.entry_count);
        assert_eq!(reader.roots().unwrap().len(), 1);

        let last = frame.len() - 1;
        frame[last] ^= 0x01;
        assert!(
            snapshot::SnapshotReader::new(&frame).is_err(),
            "a corrupted frame was accepted"
        );
    }

    /// Feeds synthetic entries straight into a generation: this measures the index, not the
    /// filesystem, and it is the only way to reach two million rows without two million inodes.
    fn stage_synthetic(index: &Index, root: RootId, count: u64) -> ScanOutcome {
        let mut session = index.begin_scan(root, ScanOptions::default()).unwrap();
        for id in 0..count {
            let path = NativePath::from_bytes(
                format!("/synthetic/dir{}/file-{id}.txt", id % 1_000).into_bytes(),
            );
            session
                .observe(ScannedEntry {
                    path,
                    parent: None,
                    meta: EntryMeta {
                        size_bytes: id,
                        mtime_ns: id as i64,
                        ..EntryMeta::default()
                    },
                })
                .unwrap();
        }
        session.commit().unwrap()
    }

    /// Scale gate. Run with:
    /// `cargo test -p lege-file-index --release -- --ignored two_million_entries_are_not_truncated`
    #[test]
    #[ignore = "two-million-entry scale fixture: release mode, minutes, ~1 GB of scratch"]
    fn two_million_entries_are_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let index = index_at(dir.path());
        let root = index
            .add_root(
                &NativePath::from_bytes(b"/synthetic".to_vec()),
                RootPolicy::default_home(),
            )
            .unwrap()
            .root;

        let started = std::time::Instant::now();
        let outcome = stage_synthetic(&index, root, 2_000_000);
        let indexed = started.elapsed();
        assert!(
            outcome.complete,
            "two million entries were reported as incomplete"
        );
        assert_eq!(outcome.entries, 2_000_000, "entries were truncated");
        assert_eq!(index.root_status(root).unwrap().state, RootState::Current);

        let mut latencies = Vec::new();
        for needle in ["file-1999999", "dir999", "annual", "file-42.txt"] {
            let started = std::time::Instant::now();
            let result = index
                .search(&Query::new(needle).with_page_size(100))
                .unwrap();
            latencies.push((needle, started.elapsed(), result.hits.len()));
        }
        println!("indexed 2,000,000 entries in {indexed:?}");
        println!("database {} bytes", index.diagnostics().unwrap().db_bytes);
        for (needle, elapsed, hits) in latencies {
            println!("search {needle:>16}: {elapsed:?} ({hits} hits on the first page)");
        }
    }

    /// Accelerator measurement. Run with:
    /// `cargo test -p lege-file-index --release -- --ignored --nocapture fts_trigram_benchmark_100k`
    #[test]
    #[ignore = "benchmark: release mode, prints numbers rather than asserting them"]
    fn fts_trigram_benchmark_100k() {
        let dir = tempfile::tempdir().unwrap();
        let index = index_at(dir.path());
        let root = index
            .add_root(
                &NativePath::from_bytes(b"/synthetic".to_vec()),
                RootPolicy::default_home(),
            )
            .unwrap()
            .root;
        stage_synthetic(&index, root, 100_000);

        println!("planner: {}", index.diagnostics().unwrap().planner);
        for (label, needle) in [
            ("exact", "file-50000.txt"),
            ("prefix", "file-5000"),
            ("substring", "e-50000"),
            ("short-substring", "50"),
            ("path", "dir500"),
        ] {
            let mut best = std::time::Duration::from_secs(60);
            let mut hits = 0;
            for _ in 0..5 {
                let started = std::time::Instant::now();
                let result = index
                    .search(&Query::new(needle).with_page_size(100))
                    .unwrap();
                best = best.min(started.elapsed());
                hits = result.hits.len();
            }
            println!("{label:>16}: {best:?} ({hits} hits on the first page)");
        }
    }
}
