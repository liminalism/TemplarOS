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

pub mod aggregate;
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

pub use aggregate::{AggregateRow, SubtreeTotals};
pub use error::{IndexError, Result};
pub use generation::{
    CancelToken, ScanBudget, ScanOptions, ScanOutcome, ScanSession, ScannedEntry,
};
pub use model::{
    BudgetKind, Coverage, CoverageMatch, CoverageRule, EntryId, EntryKind, EntryMeta, EntryRow,
    Generation, MetadataFreshness, NativePath, ObjectKey, RootId, RootPolicy, RootState,
    RootStatus, ScanScope, StaleReason, VolumeIdentity, WatchStatus,
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
     watch_json, metadata_freshness";

pub(crate) const ENTRY_COLUMNS: &str = "entry_id, root_id, parent_id, name_bytes, name_display, \
     path_bytes, path_display, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden, \
     dev, inode, nlink, mime, written_generation, last_seen_generation, deleted_generation, \
     coverage";

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
        metadata: MetadataFreshness::from_str_or_unverified(&row.get::<_, String>(18)?),
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
        coverage: Coverage::from_i64(row.get(21)?),
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

    /// `entry_count` is maintained by arithmetic on the hot path, so it has to agree with the
    /// `COUNT(*)` it replaced after a batch that exercises every way the count can move.
    ///
    /// The measurement that forced this: the recount ran over every live row of the root on
    /// **every** batch commit — ~8 ms at 15,338 entries and ~51 ms at 153,326, almost entirely
    /// CPU, extrapolating to ~330 ms per coalesced watcher batch on a million-entry home.
    ///
    /// Two things the name should settle. `entry_count` counts rows the index **holds**, at every
    /// coverage tier — an `Opaque` build directory and an expanded `Inventory` subtree are both in
    /// it, because a consumer asking "how many entries does this root have" is not asking what a
    /// search can see. And it stays exact for an **incomplete** generation, because it counts rows
    /// held rather than files on disk: a pass that saw half the tree holds what it wrote.
    #[test]
    // One test per way the count can move would be one fixture per way, and the interesting
    // failures are the interactions between them inside a single transaction.
    #[allow(clippy::too_many_lines)]
    fn entry_count_tracks_every_tier_and_stays_exact_across_an_incomplete_generation() {
        use crate::mutation::{Mutation, MutationBatch, MutationSource};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("target/debug")).unwrap();
        std::fs::write(home.join("target/debug/thing.o"), b"o").unwrap();
        tree(&home);
        let index = index_at(dir.path());
        let native = NativePath::from_path(&home);
        let root = index
            .add_root(&native, RootPolicy::default_home())
            .unwrap()
            .root;

        let recount = |index: &Index| {
            index
                .with_read(|connection| {
                    let count: i64 = connection.query_row(
                        "SELECT COUNT(*) FROM entries
                          WHERE root_id = ?1 AND deleted_generation IS NULL",
                        [root.0],
                        |row| row.get(0),
                    )?;
                    Ok(count as u64)
                })
                .unwrap()
        };
        let stated = |index: &Index| index.root_status(root).unwrap().entry_count;

        // An incomplete pass: it holds what it wrote, and says so exactly.
        let partial = index
            .scan_root(
                root,
                ScanOptions {
                    budget: ScanBudget::unlimited().with_max_entries(4),
                    ..ScanOptions::default()
                },
            )
            .unwrap();
        assert!(!partial.complete);
        assert_eq!(stated(&index), recount(&index));

        index.scan_root(root, ScanOptions::default()).unwrap();
        assert_eq!(stated(&index), recount(&index));
        // The `Opaque` build directory is one of the rows being counted.
        let opaque = index
            .entry(root, &NativePath::from_path(&home.join("target")))
            .unwrap()
            .unwrap();
        assert_eq!(opaque.coverage, Coverage::Opaque);

        let meta = |path: &Path| {
            EntryMeta::from_symlink_metadata(path, &std::fs::symlink_metadata(path).unwrap())
        };
        let path = |path: &Path| NativePath::from_path(path);

        // One batch with every shape at once: a creation, a deletion of a directory that has
        // children, a rename, a duplicate echo of a creation, and a removal of something that was
        // never there.
        std::fs::write(home.join("fresh.txt"), b"new").unwrap();
        let fresh = meta(&home.join("fresh.txt"));
        let renamed = meta(&home.join("notes/report_final"));
        let rewritten = meta(&home.join("report.pdf"));
        let applied = index
            .apply(MutationBatch::new(
                MutationSource::Operation,
                vec![
                    Mutation::Upsert {
                        root,
                        path: path(&home.join("fresh.txt")),
                        meta: fresh.clone(),
                    },
                    Mutation::Remove {
                        root,
                        path: path(&home.join("notes/2026")),
                    },
                    Mutation::Move {
                        root,
                        from: path(&home.join("notes/report_final")),
                        to: path(&home.join("notes/renamed.txt")),
                        meta: renamed,
                    },
                    // The echo: `lege-filed` applied this itself a moment ago.
                    Mutation::Upsert {
                        root,
                        path: path(&home.join("fresh.txt")),
                        meta: fresh,
                    },
                    // A file rewritten in place — the commonest watcher event there is. The row
                    // already exists and its metadata differs, so it is applied and the count must
                    // not move. Assuming a delta from the mutation's kind gets this wrong.
                    Mutation::Upsert {
                        root,
                        path: path(&home.join("report.pdf")),
                        meta: EntryMeta {
                            size_bytes: 4096,
                            mtime_ns: 1,
                            ..rewritten
                        },
                    },
                    Mutation::Remove {
                        root,
                        path: path(&home.join("never-existed")),
                    },
                ],
            ))
            .unwrap();
        assert_eq!(applied.skipped, 2, "the echo and the absent removal");
        assert_eq!(stated(&index), recount(&index), "after the mixed batch");

        // Reviving a tombstoned path is a gain of one, not two: the row is still there, carrying
        // a `deleted_generation` that the upsert clears.
        let buried = home.join("notes/2026/annual-report.md");
        index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Upsert {
                    root,
                    path: path(&buried),
                    meta: meta(&buried),
                }],
            ))
            .unwrap();
        assert_eq!(stated(&index), recount(&index), "after a revival");

        // A batch past `max_batch` commits in chunks, so the arithmetic has to hold per chunk.
        let index = Index::open(
            &dir.path().join("chunked.sqlite3"),
            IndexConfig {
                max_batch: 3,
                ..IndexConfig::default()
            },
        )
        .unwrap();
        let root = index
            .add_root(&native, RootPolicy::default_home())
            .unwrap()
            .root;
        index.scan_root(root, ScanOptions::default()).unwrap();
        let mut many = Vec::new();
        for n in 0..8 {
            let file = home.join(format!("bulk-{n}.txt"));
            std::fs::write(&file, b"x").unwrap();
            many.push(Mutation::Upsert {
                root,
                path: path(&file),
                meta: meta(&file),
            });
        }
        index
            .apply(MutationBatch::new(MutationSource::Watcher, many))
            .unwrap();
        assert_eq!(stated(&index), recount(&index), "across chunk boundaries");

        // And an expanded `Inventory` subtree is counted too.
        let before = stated(&index);
        index
            .materialise_subtree(root, &NativePath::from_path(&home.join("target")))
            .unwrap();
        assert!(stated(&index) > before, "the expansion added rows");
        assert_eq!(stated(&index), recount(&index), "after an expansion");
    }

    /// The whole point of coverage tiers: `target/foo.o` is in the index, and a search for
    /// `foo.o` returns nothing.
    ///
    /// Asserted through **both** query paths, because they are independent leaks. The FTS branch
    /// (needle >= 3 characters) is filtered by what `refresh_fts` chose to insert; the `LIKE`
    /// fallback (1-2 characters) queries `entries` directly and would return the row regardless of
    /// what the accelerator holds. Closing one and calling it done is how the leak survives.
    #[test]
    fn build_output_is_indexed_and_not_searchable_on_either_query_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("target/debug")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::write(home.join("target/debug/foo.o"), b"object code").unwrap();
        std::fs::write(home.join("notes/foo.md"), b"prose").unwrap();

        let index = index_at(dir.path());
        let (root, outcome) = scan(&index, &home);
        assert!(outcome.complete);

        // The directory is known — that is what a drive visualiser needs and what the old
        // all-or-nothing skip list refused to give it.
        let target = index
            .entry(root, &NativePath::from_path(&home.join("target")))
            .unwrap()
            .expect("the build directory itself has a row");
        assert_eq!(target.coverage, Coverage::Opaque);
        assert!(
            index
                .entry(
                    root,
                    &NativePath::from_path(&home.join("target/debug/foo.o"))
                )
                .unwrap()
                .is_none(),
            "an unexpanded Opaque subtree materialises no descendants"
        );

        for needle in ["foo.o", "fo", "o."] {
            let result = index.search(&Query::new(needle)).unwrap();
            assert!(
                result
                    .hits
                    .iter()
                    .all(|hit| !hit.row.path.display().contains("/target/")),
                "`{needle}` reached into the build directory via the {} plan",
                result.strategy
            );
        }
        assert_eq!(
            index.search(&Query::new("foo.o")).unwrap().strategy,
            "fts5-trigram",
            "the >= 3 character needle has to exercise the accelerator, not the fallback"
        );
        assert_eq!(
            index.search(&Query::new("fo")).unwrap().strategy,
            "like-scan",
            "the 2 character needle has to exercise the fallback, not the accelerator"
        );
        assert_eq!(
            index.search(&Query::new("foo")).unwrap().hits.len(),
            1,
            "and the file a person actually wrote is still found"
        );

        // Expanding the subtree materialises it as inventory: accounted for, still not searchable.
        let expanded = index
            .materialise_subtree(root, &NativePath::from_path(&home.join("target")))
            .unwrap();
        assert!(expanded.complete);
        let object = index
            .entry(
                root,
                &NativePath::from_path(&home.join("target/debug/foo.o")),
            )
            .unwrap()
            .expect("the expanded subtree holds its rows");
        assert_eq!(object.coverage, Coverage::Inventory);
        assert_eq!(object.meta.size_bytes, 11);
        for needle in ["foo.o", "fo"] {
            assert!(
                index
                    .search(&Query::new(needle))
                    .unwrap()
                    .hits
                    .iter()
                    .all(|hit| !hit.row.path.display().contains("/target/")),
                "`{needle}` found an inventory row after expansion"
            );
        }
        let inventory = index
            .search(&Query::new("foo.o").including_every_tier())
            .unwrap();
        assert_eq!(
            inventory.hits.len(),
            1,
            "a consumer that asks for every tier does get the object file"
        );
    }

    /// Job 1's whole point: an `Opaque` directory is not just known, it is *sized* — without a
    /// single row for anything inside it — and the trash nested inside it does not count.
    #[test]
    fn an_opaque_directory_reports_a_recursive_aggregate_excluding_nested_ignored_names() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("target/debug")).unwrap();
        std::fs::write(home.join("target/debug/thing.o"), vec![0u8; 1234]).unwrap();
        std::fs::create_dir_all(home.join("target/Trash")).unwrap();
        std::fs::write(home.join("target/Trash/deleted.o"), vec![0u8; 9999]).unwrap();

        let index = index_at(dir.path());
        let (root, outcome) = scan(&index, &home);
        assert!(outcome.complete);

        let target = NativePath::from_path(&home.join("target"));
        let aggregate = index
            .opaque_aggregate(root, &target)
            .unwrap()
            .expect("an Opaque directory has a cached aggregate after an ordinary pass");
        assert_eq!(
            aggregate.recursive_bytes, 1234,
            "the trashed bytes nested inside the Opaque subtree do not count"
        );
        assert_eq!(aggregate.entry_count, 2, "debug/ and debug/thing.o only");
        assert!(aggregate.readable);
        assert!(!aggregate.stale);

        // A Detailed directory has no aggregate: the cache exists only for the tier it was built
        // to answer for.
        assert_eq!(
            index
                .opaque_aggregate(root, &NativePath::from_path(&home))
                .unwrap(),
            None
        );

        // A live mutation directly inside the Opaque directory marks the aggregate stale without
        // recomputing it — nothing here changed the cached numbers.
        std::fs::write(home.join("target/debug/new.o"), vec![0u8; 1]).unwrap();
        index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Upsert {
                    root,
                    path: NativePath::from_path(&home.join("target/debug/new.o")),
                    meta: EntryMeta {
                        kind: EntryKind::Regular,
                        size_bytes: 1,
                        ..EntryMeta::default()
                    },
                }],
            ))
            .unwrap();
        let after_event = index.opaque_aggregate(root, &target).unwrap().unwrap();
        assert!(after_event.stale, "the event marked it stale");
        assert_eq!(
            after_event.recursive_bytes, 1234,
            "staleness never triggers an eager recompute"
        );

        // The next ordinary pass over the directory refreshes it and clears the flag.
        let outcome = index
            .reconcile_root(root, ScanOptions::reconcile(RootPolicy::default_home()))
            .unwrap();
        assert!(outcome.complete);
        let refreshed = index.opaque_aggregate(root, &target).unwrap().unwrap();
        assert!(!refreshed.stale);
        assert_eq!(
            refreshed.recursive_bytes, 1235,
            "the new object file is now counted"
        );
    }

    /// The upgrade gate: a database written before migration 0002 opens under this build with
    /// every row `Detailed` — which is exactly what survived the old skip list — and nothing about
    /// any root changes. A forced rescan of a home directory is not an acceptable upgrade cost.
    #[test]
    fn a_database_from_before_coverage_tiers_opens_without_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let database = dir.path().join("file-index-v1.sqlite3");

        // Build the pre-0002 shape by hand: the 0001 migration, and nothing else.
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
            schema::migrate(&connection).unwrap();
            connection
                .execute_batch(
                    "ALTER TABLE entries DROP COLUMN coverage;
                     ALTER TABLE entries_staging DROP COLUMN coverage;
                     ALTER TABLE roots DROP COLUMN metadata_freshness;
                     DELETE FROM index_migrations WHERE name <> '0001-index-core';",
                )
                .unwrap();
            schema::write_meta(&connection, "schema_version", "1").unwrap();
        }
        let index = Index::open(&database, IndexConfig::default()).unwrap();
        let (root, _) = scan(&index, &home);
        let before = index.root_status(root).unwrap();
        let searchable = index.search(&Query::new("report")).unwrap().hits.len();
        assert!(searchable > 0);
        drop(index);

        // Now open it again the way an upgraded daemon would.
        let index = Index::open(&database, IndexConfig::default()).unwrap();
        let diagnostics = index.diagnostics().unwrap();
        assert_eq!(diagnostics.schema_version, schema::schema_version());
        assert_eq!(index.root_status(root).unwrap(), before, "a root changed");
        assert_eq!(
            before.metadata,
            MetadataFreshness::Verified,
            "a pass that predates the column stat'd everything; saying otherwise would queue a \
             repair of every root on the machine to relabel what is already true"
        );
        assert_eq!(
            index.search(&Query::new("report")).unwrap().hits.len(),
            searchable,
            "the accelerator needed repair after all"
        );
        for row in index.children(root, &NativePath::from_path(&home)).unwrap() {
            assert_eq!(
                row.coverage,
                Coverage::Detailed,
                "{} was backfilled at the wrong tier",
                row.path.display()
            );
        }
    }

    /// The completeness defect this closes, and the reason it is not hypothetical.
    ///
    /// `ScanSession` counted `unreadable_dirs` from the first commit and never consulted it, so a
    /// pass that could not read — or could not watch — a directory still committed `current`, and
    /// `collect_tombstones`, which runs only for a complete pass, turned every row it had not
    /// re-observed into a deletion. On the reference laptop that is the ordinary case rather than
    /// an edge: 189,089 home directories against a 125,037 watch budget refuses watches on every
    /// pass.
    ///
    /// Both halves matter. The pass must not reach `current`, and the rows under the part it could
    /// not see must still be there afterwards.
    #[test]
    fn a_pass_that_could_not_see_a_directory_neither_reaches_current_nor_collects() {
        for (blind, expected_state, expected_reason) in [
            (
                Blindness::Unreadable,
                RootState::Stale,
                "unreadable:directories=1",
            ),
            (
                Blindness::Unwatched,
                RootState::Stale,
                "watcher-exhausted:armed=4,refused=1",
            ),
            (
                Blindness::PermissionDenied,
                RootState::PermissionLimited,
                "permission-denied",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            let (root, first) = scan(&index, &home);
            assert!(first.complete);
            let before = index.entry_count(root).unwrap();
            assert!(before >= 4, "the fixture tree is worth collecting");

            // A pass that observes only the root and then admits it could not see the rest. This
            // is exactly the shape of an arm-refused walk: rows were never re-observed, and the
            // only thing standing between them and a tombstone is the completeness test.
            let mut session = index.begin_scan(root, ScanOptions::default()).unwrap();
            session
                .observe(ScannedEntry {
                    path: NativePath::from_path(&home),
                    parent: None,
                    meta: EntryMeta {
                        kind: EntryKind::Directory,
                        ..EntryMeta::default()
                    },
                })
                .unwrap();
            match blind {
                Blindness::Unreadable => session.note_unreadable(false),
                Blindness::PermissionDenied => session.note_unreadable(true),
                Blindness::Unwatched => session.note_unwatched(4),
            }
            let outcome = session.commit().unwrap();

            assert!(
                !outcome.complete,
                "{blind:?}: a pass that could not see part of its scope claimed completeness"
            );
            let status = index.root_status(root).unwrap();
            assert_eq!(status.state, expected_state, "{blind:?}");
            assert_ne!(
                status.state,
                RootState::Current,
                "{blind:?}: an unseen subtree committed `current`"
            );
            assert_eq!(status.reason.as_deref(), Some(expected_reason), "{blind:?}");
            assert_eq!(
                index.entry_count(root).unwrap(),
                before,
                "{blind:?}: rows the pass never looked at were collected as deletions"
            );
            assert!(
                !index.search(&Query::new("report")).unwrap().hits.is_empty(),
                "{blind:?}: the previous generation stopped answering"
            );
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Blindness {
        Unreadable,
        PermissionDenied,
        Unwatched,
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

    /// Job 2's whole point: a first frame built from directory aggregates instead of leaf rows.
    /// The fixture mirrors a real home — a source tree with many ordinary files, plus a
    /// build-directory-shaped `Opaque` subtree with real files on disk under it — so the size
    /// drop has to come from omitting the leaf rows, not from `Opaque` already being small (it
    /// contributes exactly one row either way).
    #[test]
    fn a_summary_snapshot_is_dramatically_smaller_than_a_materialized_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("src")).unwrap();
        for n in 0..500 {
            std::fs::write(home.join(format!("src/module-{n}.rs")), vec![0u8; 200]).unwrap();
        }
        std::fs::create_dir_all(home.join("target/debug/deps")).unwrap();
        for n in 0..500 {
            std::fs::write(
                home.join(format!("target/debug/deps/obj-{n}.o")),
                vec![0u8; 2000],
            )
            .unwrap();
        }

        let index = index_at(dir.path());
        let (root, outcome) = scan(&index, &home);
        assert!(outcome.complete);

        let mut materialized = Vec::new();
        let full_manifest = index
            .write_snapshot(&mut materialized, &snapshot::SnapshotRequest::default())
            .unwrap();
        assert_eq!(
            full_manifest.format,
            snapshot::FORMAT,
            "unchanged by default"
        );

        let mut summary = Vec::new();
        let summary_manifest = index
            .write_snapshot(
                &mut summary,
                &snapshot::SnapshotRequest {
                    detail: snapshot::SnapshotDetail::Summary,
                    include_coverage: vec![
                        Coverage::Opaque,
                        Coverage::Inventory,
                        Coverage::Detailed,
                    ],
                    ..snapshot::SnapshotRequest::default()
                },
            )
            .unwrap();
        assert_eq!(
            summary_manifest.format,
            snapshot::FORMAT_EXTENDED,
            "a summary payload carries fields an old reader does not know"
        );

        assert!(
            materialized.len() > 500 * 4,
            "the fixture should actually hold hundreds of leaf rows: {}",
            materialized.len()
        );
        let ratio = materialized.len() as f64 / summary.len() as f64;
        eprintln!(
            "materialized={} bytes, summary={} bytes, ratio={ratio:.1}x",
            materialized.len(),
            summary.len()
        );
        assert!(
            summary.len() * 4 < materialized.len(),
            "summary ({} bytes) should be dramatically smaller than materialized ({} bytes)",
            summary.len(),
            materialized.len()
        );

        // The Opaque directory's own row carries its cached aggregate.
        let reader = snapshot::SnapshotReader::new(&summary).unwrap();
        let entries = reader.entries().unwrap();
        assert!(
            entries
                .iter()
                .all(|entry| entry.kind == Some(EntryKind::Directory)),
            "a summary snapshot holds directories only"
        );
        let target = entries
            .iter()
            .find(|entry| {
                entry
                    .path
                    .as_ref()
                    .is_some_and(|path| path.display().ends_with("/target"))
            })
            .expect("the Opaque directory itself is a row");
        assert_eq!(target.coverage, Some(Coverage::Opaque));
        let aggregate = target
            .aggregate
            .expect("the visualiser's first frame needs a number");
        assert_eq!(aggregate.recursive_bytes, 500 * 2000);
        assert!(aggregate.readable);
        assert!(!aggregate.stale);
        let _ = root;
    }

    /// `subtree` narrows the entries section without touching which roots are named in the
    /// manifest — a treemap drilling into one folder gets just that folder's rows.
    #[test]
    fn a_subtree_snapshot_request_rebases_to_just_that_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("a")).unwrap();
        std::fs::create_dir_all(home.join("b")).unwrap();
        std::fs::write(home.join("a/one.txt"), b"1").unwrap();
        std::fs::write(home.join("b/two.txt"), b"2").unwrap();

        let index = index_at(dir.path());
        let (_, outcome) = scan(&index, &home);
        assert!(outcome.complete);

        let mut frame = Vec::new();
        index
            .write_snapshot(
                &mut frame,
                &snapshot::SnapshotRequest {
                    subtree: Some(NativePath::from_path(&home.join("a"))),
                    ..snapshot::SnapshotRequest::default()
                },
            )
            .unwrap();
        let reader = snapshot::SnapshotReader::new(&frame).unwrap();
        let paths = reader.entry_paths().unwrap();
        assert!(
            paths.iter().all(|path| path.display().contains("/home/a")),
            "every entry is inside the requested subtree: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.display().ends_with("/a/one.txt")),
            "the subtree's own contents are present"
        );
        assert!(
            !paths.iter().any(|path| path.display().contains("/b/")),
            "the sibling subtree is excluded"
        );
    }

    /// `SnapshotReader` is the one authoritative parser: an unrecognised format has to fail
    /// loudly rather than let a reader guess at a byte shape it does not know.
    #[test]
    fn a_snapshot_reader_rejects_a_format_it_does_not_understand() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        tree(&home);
        let index = index_at(dir.path());
        scan(&index, &home);

        let mut frame = Vec::new();
        index
            .write_snapshot(&mut frame, &snapshot::SnapshotRequest::default())
            .unwrap();
        frame[8..12].copy_from_slice(&99u32.to_le_bytes());
        let error = snapshot::SnapshotReader::new(&frame).unwrap_err();
        assert!(error.contains("unknown snapshot format"), "{error}");
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

    /// `cargo test -p lege-file-index --release -- --ignored --nocapture progressive_snapshot_benchmark`
    ///
    /// A build-directory-shaped fixture: a source tree of ordinary files (`Detailed`) alongside a
    /// `target/`-shaped `Opaque` subtree with real files on disk under it, at a scale closer to a
    /// real project than the crate's other snapshot tests use. Reports bytes and build time,
    /// summary vs materialized (`docs/measurements/`).
    #[test]
    #[ignore = "benchmark: release mode, prints numbers rather than asserting them"]
    fn progressive_snapshot_benchmark() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("src")).unwrap();
        for n in 0..5_000 {
            std::fs::write(home.join(format!("src/module-{n}.rs")), vec![0u8; 400]).unwrap();
        }
        std::fs::create_dir_all(home.join("target/debug/deps")).unwrap();
        for n in 0..20_000 {
            std::fs::write(
                home.join(format!("target/debug/deps/obj-{n}.o")),
                vec![0u8; 3_000],
            )
            .unwrap();
        }

        let index = index_at(dir.path());
        let (_, outcome) = scan(&index, &home);
        assert!(outcome.complete);

        for (label, request) in [
            ("materialized", snapshot::SnapshotRequest::default()),
            (
                "summary",
                snapshot::SnapshotRequest {
                    detail: snapshot::SnapshotDetail::Summary,
                    include_coverage: vec![
                        Coverage::Opaque,
                        Coverage::Inventory,
                        Coverage::Detailed,
                    ],
                    ..snapshot::SnapshotRequest::default()
                },
            ),
        ] {
            let mut frame = Vec::new();
            let started = std::time::Instant::now();
            let manifest = index.write_snapshot(&mut frame, &request).unwrap();
            let elapsed = started.elapsed();
            println!(
                "{label:>12}: {:>10} entries, {:>10} bytes, format {}, built in {elapsed:?}",
                manifest.entry_count,
                frame.len(),
                manifest.format,
            );
        }
    }
}
