//! The shared file index: one derived, per-user picture of the filesystem that every TemplarOS
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
//! The crate is pure: no async runtime, no D-Bus, no toolkit. `templar-filed` hosts it.

#![forbid(unsafe_code)]

#[cfg(feature = "store")]
pub mod aggregate;
#[cfg(feature = "store")]
mod arrays;
#[cfg(feature = "store")]
pub mod btrfs_delta;
#[cfg(feature = "store")]
pub mod coalesce;
#[cfg(feature = "store")]
pub mod context;
pub mod diagnostics;
pub mod error;
#[cfg(feature = "store")]
pub mod generation;
pub mod model;
pub mod mutation;
#[cfg(feature = "store")]
mod overlay;
pub mod query;
#[cfg(feature = "store")]
pub mod reconcile;
#[cfg(feature = "store")]
pub mod scan;
#[cfg(feature = "store")]
pub mod schema;
pub mod snapshot;
#[cfg(feature = "store")]
pub mod volume;
#[cfg(feature = "store")]
pub mod watch;

#[cfg(feature = "store")]
pub use aggregate::{AggregateRow, SubtreeTotals};
pub use error::{IndexError, Result};
#[cfg(feature = "store")]
pub use generation::{
    CancelToken, ScanBudget, ScanOptions, ScanOutcome, ScanSession, ScannedEntry,
};
pub use model::{
    Availability, BudgetKind, Coverage, CoverageMatch, CoverageRule, EntryId, EntryKind, EntryMeta,
    EntryRow, Generation, MetadataFreshness, NativePath, ObjectKey, OfflineReason, RootId,
    RootPolicy, RootState, RootStatus, ScanScope, StaleReason, StorageBytes, VolumeConfidence,
    VolumeIdentity, WatchStatus,
};
pub use mutation::{
    Applied, Change, ChangeOp, ChangeSink, ChangesBatch, Mutation, MutationBatch, MutationSource,
};
#[cfg(feature = "store")]
pub use query::Cursor;
pub use query::{CursorId, FieldMask, Hit, Page, Query, Rank, SearchResult, Sort};

#[cfg(feature = "store")]
pub use context::CallContext;
pub use diagnostics::{
    ArrayColumn, ArrayStats, CursorMetrics, Diagnostics, LaneMetrics, ReaderMetrics,
    RootArrayStats, RuntimeDiagnostics, SnapshotMetrics, WalMetrics,
};
#[cfg(feature = "store")]
pub(crate) use store::{
    ENTRY_COLUMNS, ScanClaim, entry_row_from_row, next_generation, publish_generation,
    read_generation, read_root, read_roots, set_state,
};
#[cfg(feature = "store")]
pub use store::{Index, IndexConfig, MaintenanceExecutor, READ_WAIT};

#[cfg(feature = "store")]
mod store {
    use super::{
        Availability, ChangeSink, Coverage, EntryId, EntryKind, EntryMeta, EntryRow, Generation,
        IndexError, MetadataFreshness, NativePath, ObjectKey, Result, RootId, RootPolicy,
        RootState, RootStatus, ScanBudget, StaleReason, StorageBytes, VolumeIdentity, WatchStatus,
        arrays, generation, model, mutation, query, schema,
    };

    use crate::{
        CursorMetrics, Diagnostics, ReaderMetrics, RuntimeDiagnostics, WalMetrics,
        context::CallContext, error::OpenFailure, snapshot,
    };
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
        /// Read connections open at once, a hard cap: a reader that would exceed it waits up to
        /// [`READ_WAIT`] for one to come back and is then refused `Busy`. The file service sizes it
        /// to its query workers plus one.
        pub max_read_connections: usize,
        /// Rows kept in the bounded change log before the oldest are dropped.
        pub max_changes: u64,
        /// Live search cursors kept; the least recently used is evicted beyond this.
        pub max_cursors: usize,
        /// Live cursors one caller may hold. Its own least recently used cursor goes first, so one
        /// client cannot evict another's.
        pub max_cursors_per_owner: usize,
        /// Rows a cursor may pin: the best this many matches of a search, in order. Every match is
        /// still counted, so this bounds memory, not correctness.
        pub max_cursor_rows: usize,
        /// How long an idle cursor survives.
        pub cursor_ttl: std::time::Duration,
        /// Generations a tombstone outlives before a commit may sweep it.
        pub tombstone_retention_generations: u64,
        /// Entries staged before a scan flushes them.
        pub scan_flush_rows: usize,
        /// Mutations applied in one transaction before a batch is split.
        pub max_batch: usize,
        /// Explicit daemon repair budgets. Unlimited unless the owner opts into a cap.
        pub scan_budget: ScanBudget,
        /// Answer searches from in-memory arrays of the live rows (`arrays.rs`) once a search has
        /// had them built for the current generation, instead of through SQL. Off by default so a
        /// library user, and every test that does not ask for it, gets the one deterministic SQL
        /// path; the file service turns it on.
        pub search_arrays: bool,
        /// Threads one array match may use. Interactive workers × this is the CPU a burst of typing
        /// can take, so it stays small.
        pub match_threads: usize,
        /// Bytes every search-array shard together may hold; a build that would exceed it is refused
        /// and that root is answered by SQL.
        pub max_array_bytes: usize,
        /// What one snapshot may cost the service.
        pub snapshot_limits: snapshot::SnapshotLimits,
        pub volume_provider: Arc<dyn crate::volume::VolumeProvider>,
    }

    /// How long a reader waits for a connection under [`IndexConfig::max_read_connections`].
    pub const READ_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    impl Default for IndexConfig {
        fn default() -> Self {
            Self {
                max_read_connections: 5,
                max_changes: 100_000,
                max_cursors: 32,
                max_cursors_per_owner: 8,
                max_cursor_rows: 100_000,
                cursor_ttl: std::time::Duration::from_secs(300),
                tombstone_retention_generations: 2,
                scan_flush_rows: 4_096,
                max_batch: 10_000,
                scan_budget: ScanBudget::unlimited(),
                search_arrays: false,
                match_threads: std::thread::available_parallelism()
                    .map_or(1, |threads| (threads.get() / 2).clamp(1, 4)),
                max_array_bytes: 512 * 1024 * 1024,
                snapshot_limits: snapshot::SnapshotLimits::default(),
                volume_provider: Arc::new(crate::volume::NativeVolumes),
            }
        }
    }

    impl Diagnostics {
        /// Inspect an existing store without opening the owner, migrating, recovering staging rows,
        /// or rebuilding anything. A diagnostic reader must never become a second database writer.
        /// Cursor counts are process-local and are therefore zero for this offline inspection.
        pub fn read_only(path: &Path) -> Result<Self> {
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(2))?;
            connection.execute_batch("PRAGMA query_only = ON;")?;
            let view = connection.unchecked_transaction()?;
            let version = |key| -> Result<u32> {
                schema::read_meta(&view, key)?
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| IndexError::Corrupt {
                        detail: format!("missing or invalid {key}"),
                    })
            };
            let schema_version = version("schema_version")?;
            let format_version = version("format_version")?;
            // Root-status diagnostics gained their final required column in schema 3. Later
            // additive entry/index migrations do not make an older readable store a doctor failure.
            if format_version != schema::FORMAT_VERSION
                || !(3..=schema::schema_version()).contains(&schema_version)
            {
                return Err(IndexError::Corrupt {
                    detail: format!(
                        "stored format {format_version}, schema {schema_version}; this build reads \
                     format {}, diagnostic schemas 3..={}. Only templar-filed may migrate the store",
                        schema::FORMAT_VERSION,
                        schema::schema_version(),
                    ),
                });
            }
            let fts_available = generation::fts_present(&view);
            let storage = StorageBytes::of(path);
            Ok(Self {
                path: path.display().to_string(),
                schema_version,
                format_version,
                scanner_version: version("scanner_version")?,
                db_bytes: storage.total(),
                storage,
                availability: Availability::default(),
                generation: read_generation(&view)?,
                planner: if fts_available {
                    "fts5-trigram"
                } else {
                    "like-scan"
                }
                .to_owned(),
                fts_available,
                change_seq_floor: schema::read_meta(&view, "min_retained_change_seq")?
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
                change_seq_head: mutation::read_latest_change_seq(&view)?,
                cursor_count: 0,
                arrays: None,
                runtime: None,
                roots: read_roots(&view)?,
            })
        }
    }

    struct Inner {
        path: PathBuf,
        cache_identity: String,
        writer: Mutex<Connection>,
        readers: Mutex<ReaderPool>,
        readers_returned: parking_lot::Condvar,
        config: IndexConfig,
        fts: bool,
        sink: RwLock<Option<Arc<dyn ChangeSink>>>,
        cursors: Mutex<query::CursorStore>,
        scanning: Mutex<HashSet<RootId>>,
        volume_locations: Mutex<std::collections::HashMap<RootId, (String, NativePath)>>,
        arrays: arrays::ShardSet,
        wal: Mutex<WalState>,
        mutation_batches: std::sync::atomic::AtomicU64,
        /// The exclusive owner lock on `<db>.lock`, held for the index's whole life: a second owner
        /// is refused before it touches a byte of the cache.
        _owner_lock: std::fs::File,
        /// Where background maintenance (array builds) runs. Unset, each job gets its own thread; the
        /// file service installs its bounded maintenance lane.
        maintenance: RwLock<Option<Arc<dyn MaintenanceExecutor>>>,
    }

    #[derive(Default)]
    struct WalState {
        metrics: WalMetrics,
        pending: bool,
    }
    struct CheckpointClaim {
        index: Index,
        finished: bool,
    }
    impl CheckpointClaim {
        fn run(mut self) {
            self.index.0.wal.lock().pending = false;
            if let Err(error) = self.index.checkpoint_wal() {
                tracing::warn!(%error, "index WAL checkpoint failed");
                self.index.0.wal.lock().metrics.last_error = Some(error.to_string());
            }
            let pending = {
                let mut state = self.index.0.wal.lock();
                state.metrics.queued = false;
                std::mem::take(&mut state.pending)
            };
            self.finished = true;
            if pending {
                self.index.request_wal_checkpoint();
            }
        }
    }
    impl Drop for CheckpointClaim {
        fn drop(&mut self) {
            if !self.finished {
                self.index.0.wal.lock().metrics.queued = false;
            }
        }
    }

    // Return a connection and detach cancellation even when a job unwinds. A panic must not leave
    // the pool count full or let a later caller interrupt the connection after its ownership changed.
    struct ReadLease<'a> {
        index: &'a Index,
        connection: Option<Connection>,
        ticket: u64,
    }
    impl Drop for ReadLease<'_> {
        fn drop(&mut self) {
            self.index
                .release_reader(self.connection.take().unwrap(), self.ticket);
        }
    }
    struct ReadRegistration<'a> {
        cx: &'a CallContext,
        connection: &'a Connection,
    }
    impl Drop for ReadRegistration<'_> {
        fn drop(&mut self) {
            self.cx.disarm();
            let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
        }
    }

    /// Runs a background maintenance job. `job` must run eventually or `refused` must be called, so a
    /// claimed build is always released.
    pub trait MaintenanceExecutor: Send + Sync {
        fn submit(
            &self,
            name: &'static str,
            job: Box<dyn FnOnce() + Send>,
            refused: Box<dyn FnOnce() + Send>,
        );
    }

    /// Read connections: idle ones kept for reuse, how many exist, and when each read in progress
    /// began.
    #[derive(Default)]
    struct ReaderPool {
        idle: Vec<Connection>,
        open: usize,
        next_ticket: u64,
        active: std::collections::BTreeMap<u64, std::time::Instant>,
    }

    impl ReaderPool {
        fn start(&mut self) -> u64 {
            self.next_ticket += 1;
            self.active
                .insert(self.next_ticket, std::time::Instant::now());
            self.next_ticket
        }

        fn finish(&mut self, ticket: u64) {
            self.active.remove(&ticket);
        }
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

    /// Quarantined caches kept beside the live one; older quarantines are the only thing `open` ever
    /// deletes.
    const QUARANTINES_KEPT: usize = 2;

    impl Index {
        /// Opens (creating and migrating if needed) the index at `path`.
        ///
        /// Failures are classified before anything is recovered. A file this build can prove it
        /// cannot use — not a database, corrupt, another format or a newer schema — is moved with its
        /// sidecars into `file-index-quarantine/<ms>/` beside it and the cache is rebuilt: the rows
        /// are derived, so the worst a rebuild costs is a rescan. Everything else — another owner, a
        /// read-only file, a full disk, an unwritable directory — is
        /// [`IndexError::Unavailable`], and the files are left exactly as they were. Nothing is ever
        /// deleted in place.
        pub fn open(path: &Path, config: IndexConfig) -> Result<Self> {
            let unavailable = |error: IndexError| IndexError::Unavailable {
                reason: error.open_failure(),
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| unavailable(error.into()))?;
            }
            let owner_lock = Self::lock_owner(path)?;
            let writer = match Self::open_migrated(path) {
                Ok(connection) => connection,
                Err(error) => {
                    let reason = Self::classify_open_failure(path, &error);
                    if !reason.recoverable_by_quarantine() {
                        tracing::warn!(%reason, path = %path.display(), "the file index is unavailable");
                        return Err(IndexError::Unavailable { reason });
                    }
                    tracing::warn!(%reason, path = %path.display(), "quarantining and rebuilding the file index");
                    Self::quarantine(path, &reason)?;
                    Self::open_migrated(path).map_err(unavailable)?
                }
            };
            writer
                .pragma_update(None, "wal_autocheckpoint", 0)
                .map_err(|error| unavailable(error.into()))?;
            let fts = schema::ensure_fts(&writer);
            let cache_identity = writer
                .query_row(
                    "SELECT value FROM index_meta WHERE key = 'cache_identity'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| unavailable(error.into()))?;

            let index = Self(Arc::new(Inner {
                path: path.to_path_buf(),
                cache_identity,
                writer: Mutex::new(writer),
                readers: Mutex::new(ReaderPool::default()),
                readers_returned: parking_lot::Condvar::new(),
                config,
                fts,
                sink: RwLock::new(None),
                cursors: Mutex::new(query::CursorStore::default()),
                scanning: Mutex::new(HashSet::new()),
                volume_locations: Mutex::new(std::collections::HashMap::new()),
                arrays: arrays::ShardSet::default(),
                wal: Mutex::new(WalState::default()),
                mutation_batches: std::sync::atomic::AtomicU64::new(0),
                _owner_lock: owner_lock,
                maintenance: RwLock::new(None),
            }));
            index.sweep_aborted_scans().map_err(unavailable)?;
            Ok(index)
        }

        /// Stable only for this derived cache; replacement must invalidate retained checkpoints.
        #[must_use]
        pub fn cache_identity(&self) -> &str {
            &self.0.cache_identity
        }

        fn sidecar(path: &Path, suffix: &str) -> PathBuf {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        }

        /// Takes `<db>.lock` exclusively without waiting. A held lock means another live owner: the
        /// answer is `Unavailable(Busy)` with nothing touched.
        fn lock_owner(path: &Path) -> Result<std::fs::File> {
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(Self::sidecar(path, ".lock"))
                .map_err(|error| IndexError::Unavailable {
                    reason: IndexError::from(error).open_failure(),
                })?;
            match lock.try_lock() {
                Ok(()) => Ok(lock),
                Err(std::fs::TryLockError::WouldBlock) => Err(IndexError::Unavailable {
                    reason: OpenFailure::Busy,
                }),
                Err(std::fs::TryLockError::Error(error)) => Err(IndexError::Unavailable {
                    reason: IndexError::from(error).open_failure(),
                }),
            }
        }

        /// Opens and migrates the writer connection.
        ///
        /// Closing the last connection to a WAL database normally checkpoints the log into the main
        /// file and deletes it. If this open fails, that close would rewrite or remove exactly the
        /// evidence a quarantine has to keep, so checkpoint-on-close stays off until the migration
        /// has succeeded.
        fn open_migrated(path: &Path) -> Result<Connection> {
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            schema::migrate(&connection)?;
            connection.set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                false,
            )?;
            Ok(connection)
        }

        /// What an open failure means. A bare `SQLITE_ERROR` proves nothing either way, so it is
        /// settled by `PRAGMA quick_check` on a read-only connection before it may be called
        /// corruption. A file that passes the check but whose schema this build's migrations fail on
        /// (a migration's objects present without its record, say) is an incompatible schema: the
        /// rows are intact, and they are quarantined rather than read by code that does not match.
        fn classify_open_failure(path: &Path, error: &IndexError) -> OpenFailure {
            let ambiguous = matches!(
                error,
                IndexError::Sqlite(sqlite) if sqlite.sqlite_error_code() == Some(rusqlite::ErrorCode::Unknown)
            );
            if !ambiguous {
                return error.open_failure();
            }
            let checked = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|connection| {
                    connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                });
            match checked {
                Ok(answer) if answer != "ok" => OpenFailure::Corrupt {
                    detail: format!("quick_check: {answer}"),
                },
                Ok(_) => OpenFailure::Incompatible {
                    found: format!("a schema this build cannot migrate ({error})"),
                    supported: format!("{} migrations", schema::schema_version()),
                },
                Err(check) => IndexError::from(check).open_failure(),
            }
        }

        /// Moves the main file and its sidecars, in that order, into a fresh quarantine directory
        /// beside the cache, with the reason. Called only while holding the owner lock and only for a
        /// failure [`OpenFailure::recoverable_by_quarantine`] allows. If the main file cannot
        /// be moved, nothing is moved and the cache is `Unavailable`.
        fn quarantine(path: &Path, reason: &OpenFailure) -> Result<()> {
            let unavailable = |error: std::io::Error| IndexError::Unavailable {
                reason: IndexError::from(error).open_failure(),
            };
            let base = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("file-index-quarantine");
            let stamp = model::now_ms();
            let mut directory = base.join(stamp.to_string());
            let mut attempt = 0u32;
            while directory.exists() {
                attempt += 1;
                directory = base.join(format!("{stamp}-{attempt}"));
            }
            std::fs::create_dir_all(&directory).map_err(unavailable)?;
            let name = path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("index.sqlite3"));
            if let Err(error) = std::fs::rename(path, directory.join(name)) {
                let _ = std::fs::remove_dir(&directory);
                return Err(unavailable(error));
            }
            for suffix in ["-wal", "-shm"] {
                let sidecar = Self::sidecar(path, suffix);
                if sidecar.exists() {
                    let mut moved = name.to_os_string();
                    moved.push(suffix);
                    std::fs::rename(&sidecar, directory.join(moved)).map_err(unavailable)?;
                }
            }
            std::fs::write(directory.join("reason.txt"), format!("{reason}\n"))
                .map_err(unavailable)?;
            Self::prune_quarantines(&base);
            Ok(())
        }

        /// Keeps the newest [`QUARANTINES_KEPT`] quarantine directories. Their names are millisecond
        /// stamps, so name order is age order.
        fn prune_quarantines(base: &Path) {
            let Ok(entries) = std::fs::read_dir(base) else {
                return;
            };
            let mut directories: Vec<(u128, String, PathBuf)> = entries
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let (stamp, _) = name.split_once('-').unwrap_or((name.as_str(), ""));
                    Some((stamp.parse().ok()?, name.clone(), entry.path()))
                })
                .collect();
            directories.sort();
            let excess = directories.len().saturating_sub(QUARANTINES_KEPT);
            for (_, _, old) in directories.into_iter().take(excess) {
                if let Err(error) = std::fs::remove_dir_all(&old) {
                    tracing::warn!(%error, path = %old.display(), "could not prune an old index quarantine");
                }
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

        pub(crate) fn with_read<T>(
            &self,
            body: impl FnOnce(&Connection) -> Result<T>,
        ) -> Result<T> {
            self.with_read_in(None, body)
        }

        /// A read connection for `body`, never more than [`IndexConfig::max_read_connections`] at
        /// once: a caller at the cap waits up to [`READ_WAIT`] for one to come back, then gets
        /// `Busy`. With a context, the connection's interrupt handle is registered for the duration,
        /// so [`CallContext::cancel`] stops the statement running for it.
        pub(crate) fn with_read_in<T>(
            &self,
            cx: Option<&CallContext>,
            body: impl FnOnce(&Connection) -> Result<T>,
        ) -> Result<T> {
            let (connection, ticket) = self.acquire_reader(cx)?;
            let lease = ReadLease {
                index: self,
                connection: Some(connection),
                ticket,
            };
            let connection = lease.connection.as_ref().unwrap();
            match cx {
                Some(cx) => {
                    cx.arm(connection.get_interrupt_handle());
                    let _registration = ReadRegistration { cx, connection };
                    let progress = cx.clone();
                    connection.progress_handler(4096, Some(move || progress.check().is_err()))?;
                    cx.check().and_then(|()| body(connection))
                }
                None => body(connection),
            }
        }

        fn acquire_reader(&self, cx: Option<&CallContext>) -> Result<(Connection, u64)> {
            let cap = self.0.config.max_read_connections.max(1);
            let deadline = cx
                .and_then(|cx| cx.deadline)
                .map_or(std::time::Instant::now() + READ_WAIT, |deadline| {
                    deadline.min(std::time::Instant::now() + READ_WAIT)
                });
            let mut pool = self.0.readers.lock();
            loop {
                if let Some(cx) = cx {
                    cx.check()?;
                }
                if let Some(connection) = pool.idle.pop() {
                    return Ok((connection, pool.start()));
                }
                if pool.open < cap {
                    pool.open += 1;
                    let ticket = pool.start();
                    drop(pool);
                    let opened = Connection::open_with_flags(
                        &self.0.path,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )
                    .and_then(|connection| {
                        connection.busy_timeout(std::time::Duration::from_secs(5))?;
                        connection.create_scalar_function(
                            "templar_path_depth",
                            2,
                            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                                | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
                            |args| {
                                Ok(snapshot::store::path_depth(
                                    args.get_raw(0).as_blob()?,
                                    args.get_raw(1).as_blob()?,
                                ))
                            },
                        )?;
                        Ok(connection)
                    });
                    return match opened {
                        Ok(connection) => Ok((connection, ticket)),
                        Err(error) => {
                            let mut pool = self.0.readers.lock();
                            pool.open -= 1;
                            pool.finish(ticket);
                            self.0.readers_returned.notify_one();
                            Err(error.into())
                        }
                    };
                }
                if std::time::Instant::now() >= deadline {
                    if let Some(cx) = cx {
                        cx.check()?;
                    }
                    return Err(IndexError::Busy {
                        what: "every index read connection is in use",
                    });
                }
                let next = if cx.is_some() {
                    deadline.min(std::time::Instant::now() + std::time::Duration::from_millis(20))
                } else {
                    deadline
                };
                self.0.readers_returned.wait_until(&mut pool, next);
            }
        }

        fn release_reader(&self, connection: Connection, ticket: u64) {
            let mut pool = self.0.readers.lock();
            pool.finish(ticket);
            pool.idle.push(connection);
            drop(pool);
            self.0.readers_returned.notify_one();
        }

        /// Read connections open, the cap, and the age of the oldest read in progress.
        #[must_use]
        pub fn reader_metrics(&self) -> ReaderMetrics {
            let pool = self.0.readers.lock();
            ReaderMetrics {
                open: pool.open,
                cap: self.0.config.max_read_connections,
                in_use: pool.active.len(),
                oldest_read_ms: pool
                    .active
                    .values()
                    .min()
                    .map(|started| started.elapsed().as_millis() as u64),
            }
        }

        pub(crate) fn cursors(&self) -> &Mutex<query::CursorStore> {
            &self.0.cursors
        }

        pub(crate) fn arrays_slot(&self) -> &arrays::ShardSet {
            &self.0.arrays
        }

        #[must_use]
        pub fn cursor_metrics(&self) -> CursorMetrics {
            let mut cursors = self.0.cursors.lock();
            cursors.expire(self.0.config.cursor_ttl);
            let (per_owner, retained_rows) = cursors.per_owner();
            CursorMetrics {
                oldest_pinned_generation: cursors.oldest_generation(self.0.config.cursor_ttl),
                count: cursors.len(),
                per_owner,
                retained_rows,
                next_expiry_ms: cursors
                    .next_expiry(self.0.config.cursor_ttl)
                    .map(|deadline| {
                        u64::try_from(
                            deadline
                                .saturating_duration_since(std::time::Instant::now())
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX)
                    }),
            }
        }

        /// Routes background maintenance through `executor` from now on.
        pub fn set_maintenance_executor(&self, executor: Arc<dyn MaintenanceExecutor>) {
            *self.0.maintenance.write() = Some(executor);
        }

        /// Submits a maintenance job to the installed executor, or to a thread of its own.
        pub(crate) fn run_maintenance(
            &self,
            name: &'static str,
            job: Box<dyn FnOnce() + Send>,
            refused: impl FnOnce() + Send + 'static,
        ) {
            if let Some(executor) = self.0.maintenance.read().clone() {
                executor.submit(name, job, Box::new(refused));
                return;
            }
            if let Err(error) = std::thread::Builder::new()
                .name(format!("templar-index-{name}"))
                .spawn(job)
            {
                tracing::warn!(%error, name, "could not start an index maintenance thread");
                refused();
            }
        }

        /// Commit-triggered maintenance only. Repeated requests coalesce while queued; a commit
        /// during a checkpoint schedules one later pass. Queries never call this method.
        pub(crate) fn request_wal_checkpoint(&self) {
            {
                let mut state = self.0.wal.lock();
                if state.metrics.queued {
                    state.pending = true;
                    return;
                }
                state.metrics.queued = true;
            }
            let claim = CheckpointClaim {
                index: self.clone(),
                finished: false,
            };
            let executor = self.0.maintenance.read().clone();
            if let Some(executor) = executor {
                executor.submit(
                    "wal-checkpoint",
                    Box::new(move || claim.run()),
                    Box::new(|| {}),
                );
            } else {
                // Standalone store users have no service lane to drain on shutdown. Finish this
                // owned maintenance thread before returning from the commit, so dropping their
                // index releases the owner lock before they reopen or copy the database.
                // Filed installs its bounded lane and always takes the asynchronous branch.
                match std::thread::Builder::new()
                    .name("templar-index-wal-checkpoint".into())
                    .spawn(move || claim.run())
                {
                    Ok(worker) => {
                        if worker.join().is_err() {
                            tracing::warn!("index WAL checkpoint worker panicked");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "could not start WAL maintenance"),
                }
            }
        }

        pub(crate) fn mutation_committed(&self) {
            use std::sync::atomic::Ordering;
            if self
                .0
                .mutation_batches
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1)
                .is_multiple_of(64)
            {
                self.request_wal_checkpoint();
            }
        }

        fn can_truncate_wal(bytes: u64, oldest_read_ms: Option<u64>) -> bool {
            bytes > 64 * 1024 * 1024 && oldest_read_ms.is_none_or(|age| age <= 30_000)
        }

        fn checkpoint_wal(&self) -> Result<()> {
            self.with_write(|connection| {
                // TRUNCATE may meet a newly opened reader. Never wait for it while holding the
                // writer: report busy and retry on a later commit instead.
                connection.busy_timeout(std::time::Duration::ZERO)?;
                let result = (|| -> Result<()> {
                    for mode in ["PASSIVE", "TRUNCATE"] {
                        if mode == "TRUNCATE"
                            && !Self::can_truncate_wal(
                                StorageBytes::of(&self.0.path).wal_bytes,
                                self.reader_metrics().oldest_read_ms,
                            )
                        {
                            break;
                        }
                        let (busy, log, checkpointed) = connection.query_row(
                            &format!("PRAGMA wal_checkpoint({mode})"),
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?;
                        let mut state = self.0.wal.lock();
                        state.metrics.mode = Some(mode.into());
                        state.metrics.busy = Some(busy);
                        state.metrics.log = Some(log);
                        state.metrics.checkpointed = Some(checkpointed);
                        state.metrics.at_ms = Some(model::now_ms());
                        state.metrics.last_error = None;
                        if mode == "PASSIVE" {
                            state.metrics.passive_count += 1;
                        } else {
                            state.metrics.truncate_count += 1;
                        }
                    }
                    Ok(())
                })();
                let restore = connection.busy_timeout(std::time::Duration::from_secs(5));
                result?;
                restore?;
                Ok(())
            })
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
        ///
        /// Roots may nest. Ownership is most specific: a root registered inside another owns its path
        /// and everything under it, and the containing root's scans, watches and repairs treat that
        /// path as a boundary ([`Self::nested_root_paths`]). Rows the container acquired there before
        /// the inner root existed are tombstoned by its next complete pass; nothing is rejected.
        // `policy` is taken by value: this is the frozen C5 contract signature (F1-contract.md §4),
        // which F2/Files/CLI already code against, and only borrows `policy` internally.
        #[allow(clippy::needless_pass_by_value)]
        pub fn add_root(&self, path: &NativePath, policy: RootPolicy) -> Result<RootStatus> {
            let policy_json = serde_json::to_string(&policy)?;
            let identity = self
                .config()
                .volume_provider
                .identify(path)?
                .unwrap_or_default();
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
            let transaction = connection.transaction()?;
            let next: String = transaction.query_row(
                "SELECT value FROM index_meta WHERE key='next_root_id'", [], |row| row.get(0),
            )?;
            let root_id: i64 = next.parse().map_err(|_| IndexError::Corrupt {
                detail: "invalid next root identity".to_owned(),
            })?;
            let following = root_id.checked_add(1).filter(|_| root_id > 0).ok_or_else(|| IndexError::Unsupported {
                detail: "root identity space exhausted".to_owned(),
            })?;
            transaction.execute("UPDATE index_meta SET value=?1 WHERE key='next_root_id'", [following.to_string()])?;
            transaction.execute(
                "INSERT INTO roots (root_id, path_bytes, display, cross_mount, policy_json, policy_hash,
                                    state, committed_generation, complete, entry_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'empty', 0, 0, 0)",
                rusqlite::params![root_id, path.as_bytes(), path.display(), i64::from(policy.cross_mount), policy_json, policy.hash()],
            )?;
            crate::volume::store_identity(&transaction, RootId(root_id), &identity)?;
            transaction.commit()?;
            Ok(RootId(root_id))
        })?;
            self.root_status(root_id)
        }

        /// Forgets a root and every row derived from it, in one transaction that publishes a new
        /// generation: the rows were visible, so their disappearance is a change like any other, and
        /// arrays or cursors stamped with the previous generation must stop answering for them.
        pub fn forget_root(&self, root: RootId) -> Result<()> {
            let generation = self.with_write(|connection| {
                let transaction = connection.transaction()?;
                transaction.execute("DELETE FROM roots WHERE root_id = ?1", [root.0])?;
                transaction.execute("DELETE FROM entries WHERE root_id = ?1", [root.0])?;
                transaction.execute("DELETE FROM entries_staging WHERE root_id = ?1", [root.0])?;
                let generation = publish_generation(&transaction)?;
                transaction.commit()?;
                Ok(generation)
            })?;
            self.0.volume_locations.lock().remove(&root);
            self.0.cursors.lock().invalidate_root(root);
            if let Some(sink) = self.sink() {
                sink.generation_advanced(generation, &[]);
            }
            Ok(())
        }

        pub fn roots(&self) -> Result<Vec<RootStatus>> {
            let mut roots = self.with_read(read_roots)?;
            for root in &mut roots {
                self.attach_volume_location(root);
            }
            Ok(roots)
        }

        pub fn root_status(&self, root: RootId) -> Result<RootStatus> {
            let mut status = self.with_read(|connection| read_root(connection, root))?;
            self.attach_volume_location(&mut status);
            Ok(status)
        }

        fn attach_volume_location(&self, status: &mut RootStatus) {
            if status.state == RootState::Offline {
                status.mounted_elsewhere = self
                    .0
                    .volume_locations
                    .lock()
                    .get(&status.root)
                    .filter(|(identity, _)| *identity == crate::volume::fingerprint(&status.volume))
                    .map(|(_, path)| path.clone());
            }
        }

        /// Called on startup and mount-table events, off the async executor. The cache is only
        /// a location hint: registrations, rows, presence guards and generations stay untouched.
        pub fn observe_mount_locations(&self, mounts: &[crate::volume::MountRecord]) -> Result<()> {
            let roots = self.roots()?;
            let mut locations = std::collections::HashMap::new();
            for status in &roots {
                match crate::volume::mounted_elsewhere(
                    self.config().volume_provider.as_ref(),
                    &status.path,
                    &status.volume,
                    mounts,
                ) {
                    Ok(Some(path)) => {
                        locations.insert(
                            status.root,
                            (crate::volume::fingerprint(&status.volume), path),
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(root = status.root.0, %error, "volume location is unknown");
                    }
                }
            }
            let previous = std::mem::replace(&mut *self.0.volume_locations.lock(), locations);
            for mut status in roots {
                self.attach_volume_location(&mut status);
                if status.state == RootState::Offline
                    && previous.get(&status.root).map(|(_, path)| path)
                        != status.mounted_elsewhere.as_ref()
                {
                    self.notify_root(&status);
                }
            }
            Ok(())
        }

        pub fn clear_mount_locations(&self) {
            let previous = std::mem::take(&mut *self.0.volume_locations.lock());
            if let Ok(roots) = self.roots() {
                for status in roots {
                    if status.state == RootState::Offline && previous.contains_key(&status.root) {
                        self.notify_root(&status);
                    }
                }
            }
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

        /// The registered root paths strictly inside `root`'s path: the boundaries `root` never
        /// acquires, because the most specific root owns them.
        pub fn nested_root_paths(&self, root: RootId) -> Result<Vec<NativePath>> {
            self.with_read(|connection| {
                let all = root_paths(connection)?;
                let Some((_, own)) = all.iter().find(|(id, _)| *id == root) else {
                    return Err(IndexError::UnknownRoot(root));
                };
                let own = own.clone();
                let mut nested: Vec<NativePath> = all
                    .into_iter()
                    .filter(|(id, path)| *id != root && path.starts_with(&own) && *path != own)
                    .map(|(_, path)| path)
                    .collect();
                nested.sort();
                Ok(nested)
            })
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
            self.with_write(|connection| crate::volume::store_identity(connection, root, volume))
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
            // Lost observation does not prove an offline volume returned. Only mark_online or
            // explicit acceptance may clear the presence fence.
            let reason = reason.as_string();
            self.with_write(|connection| {
                let tx = connection.transaction()?;
                if read_root(&tx, root)?.state != RootState::Offline {
                    set_state(&tx, root, RootState::Stale, Some(reason.as_str()))?;
                }
                tx.commit()?;
                Ok(())
            })?;
            let status = self.root_status(root)?;
            self.notify_root(&status);
            Ok(status)
        }

        pub fn mark_offline(&self, root: RootId, reason: &str) -> Result<RootStatus> {
            let reason = if reason.contains("unmount") {
                crate::OfflineReason::Unmounted
            } else {
                crate::OfflineReason::PresenceUnknown {
                    detail: reason.to_owned(),
                }
            };
            self.mark_offline_reason(root, &reason)
        }

        /// A volume that came back. Deliberately lands in `stale`, never `current`: nothing has
        /// re-enumerated it yet, and claiming otherwise is exactly the lie this crate refuses.
        pub fn mark_online(&self, root: RootId) -> Result<RootStatus> {
            crate::volume::ensure_present(self, root)?;
            let verified =
                self.root_status(root)?.volume.confidence == crate::VolumeConfidence::Strong;
            let reason = if verified {
                StaleReason::VolumeReturned
            } else {
                StaleReason::VolumeReturnedUnverified
            }
            .as_string();
            self.transition(root, RootState::Stale, Some(reason.as_str()))
        }

        /// The committed snapshot is good and a queued backlog is being applied.
        pub fn begin_catch_up(&self, root: RootId) -> Result<RootStatus> {
            crate::volume::ensure_present(self, root)?;
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

        /// Live immediate children, in byte-path order, from one read view. Non-root entries link to
        /// their indexed parent; the indexed root itself has no parent in this store.
        pub fn children(&self, root: RootId, directory: &NativePath) -> Result<Vec<EntryRow>> {
            let mut entries = Vec::new();
            self.visit_children(root, directory, |entry| {
                entries.push(entry);
                Ok(true)
            })?;
            Ok(entries)
        }

        /// Stream the same indexed lookup while a repair spends its explicit budget. Returning false
        /// from the visitor stops SQLite row decoding immediately, instead of materializing a whole
        /// million-child directory before the first budget check.
        pub(crate) fn visit_children(
            &self,
            root: RootId,
            directory: &NativePath,
            mut visit: impl FnMut(EntryRow) -> Result<bool>,
        ) -> Result<bool> {
            self.with_read(|connection| {
                let mut statement = connection.prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM entries
                  WHERE root_id = ?1 AND parent_id = (
                      SELECT entry_id FROM entries WHERE root_id = ?1 AND path_bytes = ?2
                        AND deleted_generation IS NULL)
                    AND deleted_generation IS NULL
                  ORDER BY path_bytes"
                ))?;
                for row in statement.query_map(
                    rusqlite::params![root.0, directory.as_bytes()],
                    entry_row_from_row,
                )? {
                    if !visit(row?)? {
                        return Ok(false);
                    }
                }
                Ok(true)
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
            self.with_read(read_generation)
        }

        pub fn diagnostics(&self) -> Result<Diagnostics> {
            let cursors = self.cursor_metrics();
            let cursor_count = cursors.count;
            let readers = self.reader_metrics();
            let wal = self.0.wal.lock().metrics.clone();
            let storage = StorageBytes::of(&self.0.path);
            self.with_read(|connection| {
                let view = connection.unchecked_transaction()?;
                let head = mutation::read_latest_change_seq(&view)?;
                let mut roots = read_roots(&view)?;
                for root in &mut roots {
                    self.attach_volume_location(root);
                }
                let arrays = self
                    .config()
                    .search_arrays
                    .then(|| self.arrays_slot().diagnostic_stats(&roots));
                Ok(Diagnostics {
                    path: self.0.path.display().to_string(),
                    schema_version: schema::schema_version(),
                    format_version: schema::FORMAT_VERSION,
                    scanner_version: schema::SCANNER_VERSION,
                    db_bytes: storage.total(),
                    storage,
                    availability: Availability::default(),
                    generation: read_generation(&view)?,
                    planner: if self.0.fts {
                        "fts5-trigram".to_owned()
                    } else {
                        "like-scan".to_owned()
                    },
                    fts_available: self.0.fts,
                    change_seq_floor: schema::read_meta(&view, "min_retained_change_seq")?
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0),
                    change_seq_head: head,
                    cursor_count,
                    arrays: arrays.clone(),
                    runtime: Some(RuntimeDiagnostics {
                        arrays,
                        cursors,
                        readers,
                        wal,
                        ..RuntimeDiagnostics::default()
                    }),
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
     watch_json, metadata_freshness, volume_fs_type, volume_fs_uuid, volume_fsid, \
     volume_mount_root, volume_mount_id, volume_confidence, offline_reason_json, offline_since_ms";

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
                fs_type: row.get(19)?,
                fs_uuid: row.get(20)?,
                fsid: row.get(21)?,
                mount_root: row
                    .get::<_, Option<Vec<u8>>>(22)?
                    .map(NativePath::from_bytes),
                mount_id: row.get::<_, Option<i64>>(23)?.map(|id| id as u64),
                confidence: match row.get::<_, String>(24)?.as_str() {
                    "strong" => crate::VolumeConfidence::Strong,
                    "weak" => crate::VolumeConfidence::Weak,
                    _ => crate::VolumeConfidence::None,
                },
            },
            policy_hash: row.get(7)?,
            state: RootState::from_str_or_stale(&row.get::<_, String>(8)?),
            reason: row.get(9)?,
            offline_reason: row
                .get::<_, Option<String>>(25)?
                .and_then(|json| serde_json::from_str(&json).ok()),
            offline_since_ms: row.get(26)?,
            mounted_elsewhere: None,
            metadata: MetadataFreshness::from_str_or_unverified(&row.get::<_, String>(18)?),
            generation: Generation(row.get::<_, i64>(10)? as u64),
            complete: row.get::<_, i64>(11)? != 0,
            entry_count: row.get::<_, i64>(12)? as u64,
            last_scan_ms: row.get(13)?,
            last_reconcile_started_ms: row.get(14)?,
            last_reconcile_finished_ms: row.get(15)?,
            last_error: row.get(16)?,
            watch: watch_json.and_then(|json| serde_json::from_str(&json).ok()),
            nested_in: None,
            contains: Vec::new(),
        })
    }

    /// Fills [`RootStatus::nested_in`] and [`RootStatus::contains`] for `statuses` against every
    /// registered root in `all`: the most specific strictly containing root, and every root strictly
    /// inside.
    fn annotate_nesting(statuses: &mut [RootStatus], all: &[(RootId, NativePath)]) {
        for status in statuses {
            status.nested_in = all
                .iter()
                .filter(|(id, path)| {
                    *id != status.root && status.path.starts_with(path) && *path != status.path
                })
                .max_by_key(|(_, path)| path.as_bytes().len())
                .map(|(id, _)| *id);
            status.contains = all
                .iter()
                .filter(|(id, path)| {
                    *id != status.root && path.starts_with(&status.path) && *path != status.path
                })
                .map(|(id, _)| *id)
                .collect();
        }
    }

    fn root_paths(connection: &Connection) -> Result<Vec<(RootId, NativePath)>> {
        let mut statement = connection.prepare_cached("SELECT root_id, path_bytes FROM roots")?;
        let rows = statement.query_map([], |row| {
            Ok((
                RootId(row.get(0)?),
                NativePath::from_bytes(row.get::<_, Vec<u8>>(1)?),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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

    /// Every root, on a caller-supplied connection — so a reader that must see the roots and the rows
    /// they describe in *one* view (`Index::write_snapshot`) reads them inside its own transaction
    /// rather than through a second pooled connection.
    pub(crate) fn read_roots(connection: &Connection) -> Result<Vec<RootStatus>> {
        let mut statement = connection.prepare(&format!(
            "SELECT {ROOT_COLUMNS} FROM roots ORDER BY root_id"
        ))?;
        let rows = statement.query_map([], root_status_from_row)?;
        let mut roots = Vec::new();
        for row in rows {
            roots.push(row?);
        }
        let all: Vec<(RootId, NativePath)> = roots
            .iter()
            .map(|status| (status.root, status.path.clone()))
            .collect();
        annotate_nesting(&mut roots, &all);
        Ok(roots)
    }

    /// The published generation, on a caller-supplied connection. Same reason as [`read_roots`].
    pub(crate) fn read_generation(connection: &Connection) -> Result<Generation> {
        Ok(Generation(
            schema::read_meta(connection, "committed_generation")?
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        ))
    }

    pub(crate) fn read_root(connection: &Connection, root: RootId) -> Result<RootStatus> {
        let status = connection
            .query_row(
                &format!("SELECT {ROOT_COLUMNS} FROM roots WHERE root_id = ?1"),
                [root.0],
                root_status_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => IndexError::UnknownRoot(root),
                other => IndexError::Sqlite(other),
            })?;
        let mut statuses = [status];
        annotate_nesting(&mut statuses, &root_paths(connection)?);
        let [status] = statuses;
        Ok(status)
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
        if state != RootState::Offline {
            connection.execute(
                "UPDATE roots SET offline_reason_json=NULL, offline_since_ms=NULL WHERE root_id=?1",
                [root.0],
            )?;
        }
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

    /// Allocates the next generation and publishes it as the database-wide head, inside the caller's
    /// write transaction. Every commit that changes visible rows goes through here, so the published
    /// head is strictly greater than every earlier one and the rows a commit writes can carry exactly
    /// the head it published.
    pub(crate) fn publish_generation(connection: &Connection) -> Result<Generation> {
        let previous = read_generation(connection)?;
        let next = next_generation(connection)?;
        debug_assert!(
            next > previous,
            "generation {next} does not follow head {previous}"
        );
        schema::write_meta(connection, "committed_generation", &next.0.to_string())?;
        Ok(next)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::mutation::{Mutation, MutationBatch, MutationSource};
        use crate::{
            BudgetKind, ChangeOp, Query, Rank, ScanOptions, ScanOutcome, ScanScope, ScannedEntry,
            SearchResult, Sort, snapshot,
        };
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

        type CheckpointJob = (&'static str, Box<dyn FnOnce() + Send>);
        #[derive(Default)]
        struct CheckpointJobs(Mutex<Vec<CheckpointJob>>);
        impl MaintenanceExecutor for CheckpointJobs {
            fn submit(
                &self,
                name: &'static str,
                job: Box<dyn FnOnce() + Send>,
                _: Box<dyn FnOnce() + Send>,
            ) {
                self.0.lock().push((name, job));
            }
        }
        #[test]
        fn diagnostics_count_wal_and_shm_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let index = index_at(dir.path());
            let jobs = Arc::new(CheckpointJobs::default());
            index.set_maintenance_executor(jobs);
            let diagnostics = index.diagnostics().unwrap();
            assert!(diagnostics.storage.main_bytes > 0);
            assert!(diagnostics.storage.wal_bytes > 0);
            assert!(diagnostics.storage.shm_bytes > 0);
            assert_eq!(diagnostics.db_bytes, diagnostics.storage.total());
            let disk = Diagnostics::read_only(&index.0.path).unwrap();
            assert!(disk.runtime.is_none());
            assert!(disk.arrays.is_none());
        }
        #[test]
        fn a_passive_checkpoint_never_runs_on_the_interactive_lane() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            let jobs = Arc::new(CheckpointJobs::default());
            index.set_maintenance_executor(jobs.clone());
            let (root, _) = scan(&index, &home);
            assert_eq!(jobs.0.lock().len(), 1);
            for _ in 0..5 {
                index
                    .search(&Query::new("report"), &CallContext::internal())
                    .unwrap();
            }
            let diagnostics = index.diagnostics().unwrap();
            assert_eq!(diagnostics.runtime.unwrap().wal.passive_count, 0);
            assert_eq!(jobs.0.lock().len(), 1);
            let (name, job) = jobs.0.lock().pop().unwrap();
            assert_eq!(name, "wal-checkpoint");
            std::thread::Builder::new()
                .name("templar-test-maintenance".into())
                .spawn(job)
                .unwrap()
                .join()
                .unwrap();
            assert_eq!(
                index
                    .diagnostics()
                    .unwrap()
                    .runtime
                    .unwrap()
                    .wal
                    .passive_count,
                1
            );
            assert_eq!(jobs.0.lock().len(), 0);
            let path = NativePath::from_path(&home.join("report.pdf"));
            let mut meta = index.entry(root, &path).unwrap().unwrap().meta;
            for turn in 0..64 {
                meta.size_bytes = 100 + turn;
                let applied = index
                    .apply(MutationBatch::new(
                        MutationSource::Watcher,
                        vec![Mutation::Touch {
                            root,
                            path: path.clone(),
                            meta: meta.clone(),
                        }],
                    ))
                    .unwrap();
                assert!(!applied.changes.is_empty());
                assert_eq!(jobs.0.lock().len(), usize::from(turn == 63));
            }
            assert_eq!(jobs.0.lock().len(), 1);
            jobs.0.lock().clear();
            assert!(!index.diagnostics().unwrap().runtime.unwrap().wal.queued);
            index.request_wal_checkpoint();
            assert_eq!(jobs.0.lock().len(), 1);
            jobs.0.lock().clear();
        }
        #[test]
        fn the_wal_has_a_size_ceiling_from_open() {
            let dir = tempfile::tempdir().unwrap();
            let index = index_at(dir.path());
            // The ceiling lives on the writer, which is the only connection that commits — and
            // therefore the only one SQLite polices. Readers report no limit of their own.
            let limit: i64 = index
                .with_write(|connection| {
                    connection
                        .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
                        .map_err(IndexError::Sqlite)
                })
                .unwrap();
            assert_eq!(limit, 64 * 1024 * 1024);
        }
        #[test]
        fn a_long_scan_requests_checkpoints_as_it_stages_not_only_at_commit() {
            /// Runs maintenance inline and counts it, so mid-scan requests are observable instead
            /// of coalescing invisibly behind one queued job.
            struct InlineJobs(std::sync::atomic::AtomicUsize);
            impl MaintenanceExecutor for InlineJobs {
                fn submit(
                    &self,
                    _: &'static str,
                    job: Box<dyn FnOnce() + Send>,
                    _: Box<dyn FnOnce() + Send>,
                ) {
                    use std::sync::atomic::Ordering;
                    self.0.fetch_add(1, Ordering::SeqCst);
                    job();
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            for n in 0..200 {
                std::fs::write(home.join(format!("file-{n:04}")), b"x").unwrap();
            }
            let index = Index::open(
                &dir.path().join("file-index-v1.sqlite3"),
                IndexConfig {
                    scan_flush_rows: 8,
                    ..IndexConfig::default()
                },
            )
            .unwrap();
            let jobs = Arc::new(InlineJobs(std::sync::atomic::AtomicUsize::new(0)));
            index.set_maintenance_executor(jobs.clone());
            let (root, outcome) = scan(&index, &home);
            assert!(outcome.complete);
            assert!(outcome.entries >= 200, "the scan staged every file");
            // 25 staging flushes: one checkpoint request mid-scan plus the commit's own.
            assert_eq!(jobs.0.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert_eq!(
                index
                    .diagnostics()
                    .unwrap()
                    .runtime
                    .unwrap()
                    .wal
                    .passive_count,
                2
            );
            let _ = root;
        }
        #[test]
        fn a_directory_past_the_sort_cap_still_indexes_every_child() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            for n in 0..5_000 {
                std::fs::write(home.join(format!("file-{n:05}")), b"x").unwrap();
            }
            let index = index_at(dir.path());
            let (root, outcome) = scan(&index, &home);
            assert!(outcome.complete);
            assert!(
                outcome.entries >= 5_000,
                "the spill must visit every child, not just the sorted prefix"
            );
            for n in [0, 1234, 4096, 4999] {
                let path = home.join(format!("file-{n:05}"));
                assert!(
                    index
                        .entry(root, &NativePath::from_path(&path))
                        .unwrap()
                        .is_some(),
                    "spilled child missing from the index: {}",
                    path.display()
                );
            }
        }
        #[test]
        fn wal_truncate_respects_old_readers_and_runs_after_release() {
            let dir = tempfile::tempdir().unwrap();
            let index = index_at(dir.path());
            index.with_write(|writer| {
                writer.execute_batch("CREATE TABLE checkpoint_payload(bytes BLOB); INSERT INTO checkpoint_payload VALUES(zeroblob(68157440));")?;
                Ok(())
            }).unwrap();
            assert!(StorageBytes::of(&index.0.path).wal_bytes > 64 * 1024 * 1024);
            index
                .with_read(|reader| {
                    let view = reader.unchecked_transaction()?;
                    view.query_row("SELECT COUNT(*) FROM checkpoint_payload", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                    for started in index.0.readers.lock().active.values_mut() {
                        *started = std::time::Instant::now()
                            .checked_sub(std::time::Duration::from_secs(31))
                            .unwrap();
                    }
                    index.checkpoint_wal()?;
                    assert_eq!(index.0.wal.lock().metrics.truncate_count, 0);
                    Ok(())
                })
                .unwrap();
            index.checkpoint_wal().unwrap();
            assert_eq!(index.0.wal.lock().metrics.truncate_count, 1);
            assert_eq!(StorageBytes::of(&index.0.path).wal_bytes, 0);
        }

        #[test]
        fn readers_never_exceed_the_configured_cap() {
            let dir = tempfile::tempdir().unwrap();
            let index = Index::open(
                &dir.path().join("index.sqlite"),
                IndexConfig {
                    max_read_connections: 2,
                    ..Default::default()
                },
            )
            .unwrap();
            let gate = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
            let (ready, received) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..16)
                    .map(|_| {
                        let (index, gate, ready) = (index.clone(), gate.clone(), ready.clone());
                        scope.spawn(move || {
                            index.with_read(|_| {
                                ready.send(()).unwrap();
                                let mut released = gate.0.lock();
                                while !*released {
                                    gate.1.wait(&mut released);
                                }
                                Ok(())
                            })
                        })
                    })
                    .collect();
                received.recv_timeout(READ_WAIT).unwrap();
                received.recv_timeout(READ_WAIT).unwrap();
                assert_eq!(index.reader_metrics().in_use, 2);
                assert_eq!(index.reader_metrics().open, 2);
                *gate.0.lock() = true;
                gate.1.notify_all();
                for worker in workers {
                    worker.join().unwrap().unwrap();
                }
            });
            assert_eq!(index.reader_metrics().open, 2);
            assert_eq!(index.reader_metrics().in_use, 0);
        }

        #[test]
        fn a_cancelled_search_stops_its_sql_statement_and_leaves_no_cursor() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir(&home).unwrap();
            let index = index_at(dir.path());
            let (root, _) = scan(&index, &home);
            for batch in 0..20 {
                let entries = (0..10_000)
                    .map(|number| Mutation::Upsert {
                        root,
                        path: NativePath::from_path(
                            &home.join(format!("row-{}", batch * 10_000 + number)),
                        ),
                        meta: EntryMeta {
                            kind: EntryKind::Regular,
                            ..Default::default()
                        },
                    })
                    .collect();
                index
                    .apply(MutationBatch::new(MutationSource::Reconcile, entries))
                    .unwrap();
            }
            let cx = Arc::new(CallContext::internal());
            let thread = {
                let (index, cx) = (index.clone(), cx.clone());
                std::thread::spawn(move || index.search(&Query::new("/never-present/needle"), &cx))
            };
            let until = std::time::Instant::now() + READ_WAIT;
            while index.reader_metrics().in_use == 0 && !thread.is_finished() {
                assert!(std::time::Instant::now() < until);
                std::thread::yield_now();
            }
            let start = std::time::Instant::now();
            cx.cancel();
            assert!(matches!(thread.join().unwrap(), Err(IndexError::Cancelled)));
            assert!(start.elapsed() < std::time::Duration::from_millis(100));
            assert_eq!(index.diagnostics().unwrap().cursor_count, 0);
            assert_eq!(index.reader_metrics().in_use, 0);
            // A reused connection is no longer attached to the old cancelled context.
            assert!(
                index
                    .search(&Query::new("row-123"), &CallContext::internal())
                    .is_ok()
            );
        }

        #[test]
        fn a_streamed_frame_is_byte_identical_to_the_previous_writer() {
            for fixture in 0..3 {
                let dir = tempfile::tempdir().unwrap();
                let home = dir.path().join("home");
                tree(&home);
                if fixture > 0 {
                    std::fs::write(
                        home.join(std::ffi::OsString::from_vec(b"native-\xff".to_vec())),
                        b"native",
                    )
                    .unwrap();
                }
                if fixture > 1 {
                    std::fs::create_dir_all(home.join("target/deep")).unwrap();
                    std::fs::write(home.join("target/deep/object.o"), [7; 29]).unwrap();
                }
                let index = index_at(dir.path());
                scan(&index, &home);
                for fields in [
                    query::FieldMask(0),
                    query::FieldMask::PATH,
                    query::FieldMask::ALL,
                    query::FieldMask::ALL
                        .with(query::FieldMask::COVERAGE)
                        .with(query::FieldMask::AGGREGATE),
                ] {
                    for detail in [
                        snapshot::SnapshotDetail::Materialized,
                        snapshot::SnapshotDetail::Summary,
                    ] {
                        let request = snapshot::SnapshotRequest {
                            fields,
                            detail,
                            ..Default::default()
                        };
                        let mut previous = Vec::new();
                        let expected = index
                            .write_snapshot_oracle(&mut previous, &request)
                            .unwrap();
                        let mut streamed = std::io::Cursor::new(Vec::new());
                        let actual = index
                            .write_snapshot(
                                &mut streamed,
                                &request,
                                &snapshot::SnapshotLimits::default(),
                                &CallContext::internal(),
                            )
                            .unwrap();
                        assert_eq!(
                            streamed.into_inner(),
                            previous,
                            "fixture {fixture}, mask {}, {detail:?}",
                            fields.0
                        );
                        assert_eq!(actual.snapshot_seq, expected.snapshot_seq);
                        assert_eq!(actual.payload_crc32c, expected.payload_crc32c);
                    }
                }
            }
        }

        #[test]
        fn the_writer_aborts_with_too_large_before_sealing() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            scan(&index, &home);
            for limits in [
                snapshot::SnapshotLimits {
                    max_bytes: 100,
                    ..Default::default()
                },
                snapshot::SnapshotLimits {
                    max_entries: 1,
                    ..Default::default()
                },
            ] {
                let mut frame = std::io::Cursor::new(Vec::new());
                let error = index
                    .write_snapshot(
                        &mut frame,
                        &snapshot::SnapshotRequest::default(),
                        &limits,
                        &CallContext::internal(),
                    )
                    .unwrap_err();
                assert!(matches!(error, IndexError::TooLarge { .. }));
                assert!(snapshot::SnapshotReader::new(frame.get_ref()).is_err());
                assert_eq!(index.reader_metrics().in_use, 0);
            }
            let request = snapshot::SnapshotRequest {
                max_entries: Some(0),
                ..Default::default()
            };
            assert!(matches!(
                index.write_snapshot(
                    &mut std::io::Cursor::new(Vec::new()),
                    &request,
                    &snapshot::SnapshotLimits::default(),
                    &CallContext::internal()
                ),
                Err(IndexError::TooLarge { .. })
            ));
        }

        #[test]
        fn summary_depth_three_returns_only_directories_within_three_levels() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let native = home.join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
            std::fs::create_dir_all(native.join("two/three/four")).unwrap();
            std::fs::write(native.join("leaf"), b"leaf").unwrap();
            let index = index_at(dir.path());
            scan(&index, &home);
            let request = snapshot::SnapshotRequest {
                detail: snapshot::SnapshotDetail::Summary,
                fields: query::FieldMask::PATH.with(query::FieldMask::KIND),
                max_depth: Some(3),
                ..Default::default()
            };
            let mut frame = std::io::Cursor::new(Vec::new());
            index
                .write_snapshot(
                    &mut frame,
                    &request,
                    &snapshot::SnapshotLimits::default(),
                    &CallContext::internal(),
                )
                .unwrap();
            let reader = snapshot::SnapshotReader::new(frame.get_ref()).unwrap();
            let entries: Vec<_> = reader.entries_iter().unwrap().map(Result::unwrap).collect();
            assert_eq!(entries.len(), 4);
            assert!(
                entries
                    .iter()
                    .all(|entry| entry.kind == Some(EntryKind::Directory))
            );
            assert!(
                entries.iter().any(|entry| entry.path
                    == Some(native.join("two/three").as_os_str().as_encoded_bytes()))
            );
            assert!(!entries.iter().any(|entry| entry.path
                == Some(native.join("two/three/four").as_os_str().as_encoded_bytes())));
            index
                .with_read(|connection| {
                    let (sql, params) = snapshot::store::entries_query_for_test(
                        1,
                        home.as_os_str().as_encoded_bytes(),
                        &request,
                    );
                    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let plan: Vec<String> = statement
                        .query_map(rusqlite::params_from_iter(params), |row| row.get(3))?
                        .collect::<std::result::Result<_, _>>()?;
                    println!("snapshot summary plan: {}", plan.join("; "));
                    assert!(plan.iter().any(|line| line.contains("INDEX")), "{plan:?}");
                    Ok(())
                })
                .unwrap();
        }

        #[test]
        fn a_cancelled_snapshot_releases_its_read_transaction() {
            use std::io::{Seek, SeekFrom, Write};
            struct CancelSink<'a> {
                bytes: std::io::Cursor<Vec<u8>>,
                cx: &'a CallContext,
            }
            impl Write for CancelSink<'_> {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    let written = self.bytes.write(bytes)?;
                    if self.bytes.position() > u64::from(snapshot::HEADER_LEN) {
                        self.cx.cancel();
                    }
                    Ok(written)
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            impl Seek for CancelSink<'_> {
                fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                    self.bytes.seek(pos)
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            scan(&index, &home);
            let cx = CallContext::internal();
            let mut sink = CancelSink {
                bytes: std::io::Cursor::new(Vec::new()),
                cx: &cx,
            };
            assert!(matches!(
                index.write_snapshot(
                    &mut sink,
                    &snapshot::SnapshotRequest::default(),
                    &snapshot::SnapshotLimits::default(),
                    &cx
                ),
                Err(IndexError::Cancelled)
            ));
            assert_eq!(index.reader_metrics().in_use, 0);
            assert!(snapshot::SnapshotReader::new(sink.bytes.get_ref()).is_err());
            index
                .with_write(|connection| {
                    let busy: i64 =
                        connection
                            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
                    assert_eq!(busy, 0);
                    Ok(())
                })
                .unwrap();
            let mut frame = std::io::Cursor::new(Vec::new());
            index
                .write_snapshot(
                    &mut frame,
                    &snapshot::SnapshotRequest::default(),
                    &snapshot::SnapshotLimits::default(),
                    &CallContext::internal(),
                )
                .unwrap();
        }

        #[test]
        fn diagnostics_never_create_migrate_or_recover_the_derived_database() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("missing.sqlite3");
            assert!(Diagnostics::read_only(&missing).is_err());
            assert!(!missing.exists());
            let home = dir.path().join("home");
            tree(&home);
            let path = dir.path().join("index.sqlite3");
            let index = Index::open(&path, IndexConfig::default()).unwrap();
            let (root, _) = scan(&index, &home);
            let generation = index.generation().unwrap();
            let diagnostic = Diagnostics::read_only(&path).unwrap();
            assert_eq!(diagnostic.generation, generation);
            assert_eq!(diagnostic.roots[0].root, root);
            index
                .with_write(|connection| {
                    connection.execute(
                        "UPDATE index_meta SET value='5' WHERE key='schema_version'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(Diagnostics::read_only(&path).unwrap().schema_version, 5);
            index
                .with_write(|connection| {
                    connection.execute(
                        "UPDATE index_meta SET value='999' WHERE key='schema_version'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
            drop(index);
            let before = std::fs::read(&path).unwrap();
            assert!(Diagnostics::read_only(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(!path.with_extension("corrupt").exists());
        }

        #[test]
        fn cache_lineage_survives_reopen_but_not_disposable_cache_replacement() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("index.sqlite3");
            let first = Index::open(&path, IndexConfig::default()).unwrap();
            let identity = first.cache_identity().to_owned();
            let a = first
                .add_root(
                    &NativePath::from_path(&dir.path().join("a")),
                    RootPolicy::default_home(),
                )
                .unwrap();
            first.forget_root(a.root).unwrap();
            let b = first
                .add_root(
                    &NativePath::from_path(&dir.path().join("b")),
                    RootPolicy::default_home(),
                )
                .unwrap();
            assert!(b.root.0 > a.root.0, "removal recycled an old root identity");
            drop(first);
            let reopened = Index::open(&path, IndexConfig::default()).unwrap();
            assert_eq!(reopened.cache_identity(), identity);
            reopened.forget_root(b.root).unwrap();
            let c = reopened
                .add_root(
                    &NativePath::from_path(&dir.path().join("c")),
                    RootPolicy::default_home(),
                )
                .unwrap();
            assert!(
                c.root.0 > b.root.0,
                "reopening lost the root identity sequence"
            );
            let replacement = Index::open(
                &dir.path().join("replacement.sqlite3"),
                IndexConfig::default(),
            )
            .unwrap();
            assert_ne!(replacement.cache_identity(), identity);
        }

        #[test]
        fn atomic_acquisition_never_commits_a_prefix_above_the_watcher_chunk_limit() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = Index::open(
                &dir.path().join("index.sqlite3"),
                IndexConfig {
                    max_batch: 1,
                    ..IndexConfig::default()
                },
            )
            .unwrap();
            let (root, _) = scan(&index, &home);
            let generation = index.generation().unwrap();
            let path = NativePath::from_path(&home.join("candidate.txt"));
            let meta = EntryMeta::default();
            let batch = MutationBatch::new(
                MutationSource::Reconcile,
                vec![
                    Mutation::Upsert {
                        root,
                        path: path.clone(),
                        meta: meta.clone(),
                    },
                    Mutation::Upsert {
                        root: RootId(i64::MAX),
                        path: NativePath::from_path(&home.join("invalid")),
                        meta,
                    },
                ],
            );
            assert!(index.apply_atomic(&batch).is_err());
            assert_eq!(index.generation().unwrap(), generation);
            assert!(index.entry(root, &path).unwrap().is_none());
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
                !index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty(),
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
                index
                    .search(&Query::new("buried"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty(),
                "the depth budget did not bound the branch"
            );
            assert!(
                !index
                    .search(&Query::new("shallow"), &crate::CallContext::internal())
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
                        // The echo: `templar-filed` applied this itself a moment ago.
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

            // A move onto an occupied path is a loss of one: the destination's row becomes the moved
            // entry and the source's row stays behind as a tombstone. A move onto a path held only by
            // a tombstone costs nothing, because that row was not live to begin with.
            std::fs::rename(home.join("fresh.txt"), home.join("report.pdf")).unwrap();
            index
                .apply(MutationBatch::new(
                    MutationSource::Watcher,
                    vec![Mutation::Move {
                        root,
                        from: path(&home.join("fresh.txt")),
                        to: path(&home.join("report.pdf")),
                        meta: meta(&home.join("report.pdf")),
                    }],
                ))
                .unwrap();
            assert_eq!(stated(&index), recount(&index), "after an overwriting move");
            std::fs::rename(home.join("report.pdf"), home.join("fresh.txt")).unwrap();
            index
                .apply(MutationBatch::new(
                    MutationSource::Watcher,
                    vec![Mutation::Move {
                        root,
                        from: path(&home.join("report.pdf")),
                        to: path(&home.join("fresh.txt")),
                        meta: meta(&home.join("fresh.txt")),
                    }],
                ))
                .unwrap();
            assert_eq!(
                stated(&index),
                recount(&index),
                "after a move onto a tombstone"
            );

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
                let result = index
                    .search(&Query::new(needle), &crate::CallContext::internal())
                    .unwrap();
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
                index
                    .search(&Query::new("foo.o"), &crate::CallContext::internal())
                    .unwrap()
                    .strategy,
                "fts5-trigram",
                "the >= 3 character needle has to exercise the accelerator, not the fallback"
            );
            assert_eq!(
                index
                    .search(&Query::new("fo"), &crate::CallContext::internal())
                    .unwrap()
                    .strategy,
                "like-scan",
                "the 2 character needle has to exercise the fallback, not the accelerator"
            );
            assert_eq!(
                index
                    .search(&Query::new("foo"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .len(),
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
                        .search(&Query::new(needle), &crate::CallContext::internal())
                        .unwrap()
                        .hits
                        .iter()
                        .all(|hit| !hit.row.path.display().contains("/target/")),
                    "`{needle}` found an inventory row after expansion"
                );
            }
            let inventory = index
                .search(
                    &Query::new("foo.o").including_every_tier(),
                    &crate::CallContext::internal(),
                )
                .unwrap();
            assert_eq!(
                inventory.hits.len(),
                1,
                "a consumer that asks for every tier does get the object file"
            );
        }

        /// A result cut at `max_cursor_rows` is the best rows in order, on both query paths, and its
        /// `total` still counts every match. The old candidate `LIMIT` ran before ranking, so a cut
        /// kept whatever the scan visited first and the exact match could be the row that was lost.
        #[test]
        fn a_cut_off_result_keeps_the_best_matches_and_counts_all_of_them() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            for number in 0..40 {
                std::fs::write(home.join(format!("aaa-report-{number:02}")), b"x").unwrap();
            }
            std::fs::write(home.join("report"), b"the one").unwrap();
            let index = Index::open(
                &dir.path().join("file-index-v1.sqlite3"),
                IndexConfig {
                    max_cursor_rows: 5,
                    ..IndexConfig::default()
                },
            )
            .unwrap();
            scan(&index, &home);

            for (needle, best) in [("report", Rank::Exact), ("re", Rank::Prefix)] {
                let result = index
                    .search(&Query::new(needle), &crate::CallContext::internal())
                    .unwrap();
                assert_eq!(result.total, Some(41), "`{needle}` must count every match");
                assert!(result.truncated, "`{needle}`: 41 matches, 5 kept");
                assert_eq!(result.hits.len(), 5);
                assert_eq!(
                    result.hits[0].row.path,
                    NativePath::from_path(&home.join("report")),
                    "`{needle}` via {} lost the best match",
                    result.strategy
                );
                assert_eq!(result.hits[0].rank, best);
            }
        }

        /// The arrays are a cache of the SQL answer, so they must give the same one: the same rows in
        /// the same order with the same ranks and the same total, for every sort, direction, filter
        /// and scope — including the cut at `max_cursor_rows`.
        #[test]
        fn the_search_arrays_answer_exactly_what_sql_answers() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let files: &[(&str, usize, u64)] = &[
                ("Report.PDF", 30, 100),
                ("report", 1, 300),
                ("notes/2026/annual-report.md", 400, 200),
                ("notes/report_final", 400, 200),
                // Sort between `reports` and `reports/…` in byte order (`-` and `.` precede `/`),
                // so a directory's subtree is not the rows right after it.
                ("reports-2025.csv", 3, 250),
                ("reports.txt", 3, 250),
                ("reports-old/report.md", 3, 250),
                ("notes/.hidden-report", 5, 50),
                ("reports/q1/sales.csv", 7, 400),
                ("reports/q1/report.txt", 7, 400),
                ("archive/reports/old/a.txt", 9, 10),
                ("archive/misc/rep.txt", 9, 10),
                ("x", 2, 500),
                ("big.bin", 5000, 5),
            ];
            for (path, size, age) in files {
                let path = home.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, vec![b'x'; *size]).unwrap();
                let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(*age);
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(when)
                    .unwrap();
            }
            let index = Index::open(
                &dir.path().join("file-index-v1.sqlite3"),
                IndexConfig {
                    search_arrays: true,
                    max_cursor_rows: 4,
                    ..IndexConfig::default()
                },
            )
            .unwrap();
            let (root, _) = scan(&index, &home);
            index.prepare_search_arrays().unwrap();

            let mut queries = Vec::new();
            for needle in ["rep", "re", "r", "report", "REPORT", "q1", "x", "nothing"] {
                for sort in [
                    Sort::Relevance,
                    Sort::Name,
                    Sort::Path,
                    Sort::Size,
                    Sort::Mtime,
                ] {
                    for descending in [false, true] {
                        let mut query = Query::new(needle);
                        query.sort = sort;
                        query.descending = descending;
                        queries.push(query);
                    }
                }
            }
            let mut filtered = Query::new("rep");
            filtered.include_hidden = false;
            filtered.kinds = Some(vec![EntryKind::Regular]);
            filtered.min_size = Some(8);
            queries.push(filtered);
            let mut scoped = Query::new("re").in_roots(vec![root]);
            scoped.within = vec![NativePath::from_path(&home.join("reports"))];
            queries.push(scoped);
            let mut whole_root = Query::new("rep");
            whole_root.within = vec![NativePath::from_path(&home)];
            queries.push(whole_root);
            let mut every_tier = Query::new("rep").including_every_tier();
            every_tier.sort = Sort::Size;
            queries.push(every_tier);

            for query in &queries {
                let arrays = index
                    .search(query, &crate::CallContext::internal())
                    .unwrap();
                let sql = index
                    .search_with(
                        &crate::query::Fts5TrigramPlanner,
                        query,
                        &crate::CallContext::internal(),
                    )
                    .unwrap();
                assert_eq!(arrays.strategy, "arrays", "{query:?}");
                let shape = |result: &SearchResult| {
                    (
                        result.total,
                        result.truncated,
                        result
                            .hits
                            .iter()
                            .map(|hit| (hit.row.path.display(), hit.rank))
                            .collect::<Vec<_>>(),
                    )
                };
                assert_eq!(shape(&arrays), shape(&sql), "{query:?}");
            }
        }

        /// Arrays built for one generation are not used for the next: the search goes back to SQL,
        /// which sees the new row, rather than answering from a stale cache.
        #[test]
        fn stale_search_arrays_are_not_used() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = Index::open(
                &dir.path().join("file-index-v1.sqlite3"),
                IndexConfig {
                    search_arrays: true,
                    ..IndexConfig::default()
                },
            )
            .unwrap();
            let (root, _) = scan(&index, &home);
            index.prepare_search_arrays().unwrap();
            assert_eq!(
                index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .strategy,
                "arrays"
            );

            std::fs::write(home.join("fresh-report.txt"), b"new").unwrap();
            index.scan_root(root, ScanOptions::default()).unwrap();
            let result = index
                .search(&Query::new("fresh-report"), &crate::CallContext::internal())
                .unwrap();
            assert_ne!(result.strategy, "arrays");
            assert_eq!(result.total, Some(1));
        }

        /// A search scoped to one folder sees every match inside it, however far down the whole
        /// index's ranking those matches sit.
        #[test]
        fn a_folder_scoped_search_returns_every_match_inside_the_folder() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            for number in 0..30 {
                let elsewhere = home.join(format!("elsewhere/d{number:02}"));
                std::fs::create_dir_all(&elsewhere).unwrap();
                std::fs::write(elsewhere.join("plan"), b"x").unwrap();
            }
            std::fs::create_dir_all(home.join("inside/deep")).unwrap();
            std::fs::write(home.join("inside/deep/old-plan-notes"), b"x").unwrap();
            let index = index_at(dir.path());
            scan(&index, &home);

            let mut query = Query::new("plan").with_page_size(10);
            query.within = vec![NativePath::from_path(&home.join("inside"))];
            let result = index
                .search(&query, &crate::CallContext::internal())
                .unwrap();
            assert_eq!(result.total, Some(1));
            assert_eq!(
                result.hits[0].row.path,
                NativePath::from_path(&home.join("inside/deep/old-plan-notes"))
            );
            assert_eq!(
                index
                    .search(&Query::new("plan"), &crate::CallContext::internal())
                    .unwrap()
                    .total,
                Some(31),
                "unscoped, the thirty exact matches rank ahead of it"
            );
        }

        /// Path matching without path trigrams: a row matches by path when a directory above it
        /// matches by name. Nested matching directories are counted once, and a needle with a
        /// separator matches whole paths.
        #[test]
        fn path_matches_come_from_matching_directories_and_are_counted_once() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join("projects/alpha/src")).unwrap();
            std::fs::create_dir_all(home.join("projects/alpha/projects")).unwrap();
            std::fs::write(home.join("projects/alpha/src/main.rs"), b"x").unwrap();
            std::fs::write(home.join("projects/alpha/projects-notes.md"), b"x").unwrap();
            let index = index_at(dir.path());
            scan(&index, &home);

            for needle in ["projects", "pr"] {
                let result = index
                    .search(&Query::new(needle), &crate::CallContext::internal())
                    .unwrap();
                // projects, alpha, src, main.rs, projects (nested), projects-notes.md
                assert_eq!(result.total, Some(6), "`{needle}` via {}", result.strategy);
                let mut ids: Vec<_> = result.hits.iter().map(|hit| hit.row.entry_id).collect();
                ids.sort();
                ids.dedup();
                assert_eq!(
                    ids.len(),
                    6,
                    "a row under two matching directories appeared twice"
                );
                let main = result
                    .hits
                    .iter()
                    .find(|hit| {
                        hit.row.path
                            == NativePath::from_path(&home.join("projects/alpha/src/main.rs"))
                    })
                    .expect("a file under a matching directory is a path match");
                assert_eq!(main.rank, Rank::PathOnly);
            }

            let separator = index
                .search(&Query::new("alpha/src"), &crate::CallContext::internal())
                .unwrap();
            assert_eq!(separator.strategy, "path-scan");
            assert_eq!(separator.total, Some(2), "src and main.rs");
        }

        /// A database written by a build with the old accelerator (trigrams of names and paths, with
        /// content) is converted on open and refilled, so name search keeps working without a rescan.
        #[test]
        fn an_old_path_trigram_accelerator_is_replaced_and_refilled_on_open() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let db = dir.path().join("file-index-v1.sqlite3");
            {
                let index = index_at(dir.path());
                scan(&index, &home);
            }
            {
                let connection = Connection::open(&db).unwrap();
                connection
                    .execute_batch(
                        r#"DROP TABLE entries_fts;
                       CREATE VIRTUAL TABLE entries_fts USING fts5(
                           name_display, path_display, tokenize = "trigram");"#,
                    )
                    .unwrap();
            }
            let index = index_at(dir.path());
            let result = index
                .search(&Query::new("annual"), &crate::CallContext::internal())
                .unwrap();
            assert_eq!(result.strategy, "fts5-trigram");
            assert_eq!(result.total, Some(1), "the refill restored the name");
            let shape: String = Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'entries_fts'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!shape.contains("path_display"), "{shape}");
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
            let searchable = index
                .search(&Query::new("report"), &crate::CallContext::internal())
                .unwrap()
                .hits
                .len();
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
                index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .len(),
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
                    !index
                        .search(&Query::new("report"), &crate::CallContext::internal())
                        .unwrap()
                        .hits
                        .is_empty(),
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
            let before = index
                .search(&Query::new("report"), &crate::CallContext::internal())
                .unwrap();
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
            // The leaked session still holds the owner lock, as a process that has not died yet
            // would. What a SIGKILL leaves is the files on disk, so the reopen reads a copy of them.
            let crashed = dir.path().join("crashed");
            std::fs::create_dir_all(&crashed).unwrap();
            for suffix in ["", "-wal", "-shm"] {
                let source = dir.path().join(format!("file-index-v1.sqlite3{suffix}"));
                if source.exists() {
                    std::fs::copy(
                        &source,
                        crashed.join(format!("file-index-v1.sqlite3{suffix}")),
                    )
                    .unwrap();
                }
            }
            drop(index);

            let index = index_at(&crashed);
            let after = index
                .search(&Query::new("report"), &crate::CallContext::internal())
                .unwrap();
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
                    .search(&Query::new("half-written"), &crate::CallContext::internal())
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
                .write_snapshot(
                    &mut std::io::Cursor::new(&mut materialized),
                    &snapshot::SnapshotRequest::default(),
                    &crate::snapshot::SnapshotLimits::default(),
                    &crate::CallContext::internal(),
                )
                .unwrap();
            assert_eq!(
                full_manifest.format,
                snapshot::FORMAT,
                "unchanged by default"
            );

            let mut summary = Vec::new();
            let summary_manifest = index
                .write_snapshot(
                    &mut std::io::Cursor::new(&mut summary),
                    &snapshot::SnapshotRequest {
                        detail: snapshot::SnapshotDetail::Summary,
                        include_coverage: vec![
                            Coverage::Opaque,
                            Coverage::Inventory,
                            Coverage::Detailed,
                        ],
                        ..snapshot::SnapshotRequest::default()
                    },
                    &crate::snapshot::SnapshotLimits::default(),
                    &crate::CallContext::internal(),
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
            let entries = reader
                .entries_iter()
                .unwrap()
                .map(|entry| entry.unwrap().to_entry())
                .collect::<Vec<_>>();
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
                    &mut std::io::Cursor::new(&mut frame),
                    &snapshot::SnapshotRequest {
                        subtree: Some(NativePath::from_path(&home.join("a"))),
                        ..snapshot::SnapshotRequest::default()
                    },
                    &crate::snapshot::SnapshotLimits::default(),
                    &crate::CallContext::internal(),
                )
                .unwrap();
            let reader = snapshot::SnapshotReader::new(&frame).unwrap();
            let paths = reader
                .entry_paths_iter()
                .unwrap()
                .map(|path| crate::NativePath::from_bytes(path.unwrap().to_vec()))
                .collect::<Vec<_>>();
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
                .write_snapshot(
                    &mut std::io::Cursor::new(&mut frame),
                    &snapshot::SnapshotRequest::default(),
                    &crate::snapshot::SnapshotLimits::default(),
                    &crate::CallContext::internal(),
                )
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

            let hits = index
                .search(&Query::new("report"), &crate::CallContext::internal())
                .unwrap()
                .hits;
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
                .write_snapshot(
                    &mut std::io::Cursor::new(&mut frame),
                    &snapshot::SnapshotRequest::default(),
                    &crate::snapshot::SnapshotLimits::default(),
                    &crate::CallContext::internal(),
                )
                .unwrap();
            let reader = snapshot::SnapshotReader::new(&frame).unwrap();
            assert!(
                reader
                    .entry_paths_iter()
                    .unwrap()
                    .map(|path| crate::NativePath::from_bytes(path.unwrap().to_vec()))
                    .collect::<Vec<_>>()
                    .contains(&expected),
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
                .search(
                    &Query::new("report").with_page_size(1),
                    &crate::CallContext::internal(),
                )
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

            let page = index
                .fetch_page(&result.cursor, 0, 10, &crate::CallContext::internal())
                .unwrap();
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
            let again = index
                .search(&Query::new("report"), &crate::CallContext::internal())
                .unwrap();
            assert_eq!(again.generation, applied.generation);

            index.close_cursor(&result.cursor, &crate::CallContext::internal());
            assert!(
                matches!(
                    index.fetch_page(&result.cursor, 0, 10, &crate::CallContext::internal()),
                    Err(IndexError::SnapshotRequired { .. })
                ),
                "a closed cursor must ask for a new snapshot, not answer from newer data"
            );
        }

        #[test]
        fn a_cursor_refuses_newer_size_or_renamed_path_facts_and_refresh_restores_the_right_parent()
        {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            let (root, _) = scan(&index, &home);
            let original = home.join("report.pdf");
            let pinned = index
                .search(&Query::new("report.pdf"), &crate::CallContext::internal())
                .unwrap();
            std::fs::write(&original, b"new size").unwrap();
            index
                .apply(MutationBatch::new(
                    MutationSource::Watcher,
                    vec![Mutation::Upsert {
                        root,
                        path: NativePath::from_path(&original),
                        meta: EntryMeta::from_symlink_metadata(
                            &original,
                            &original.symlink_metadata().unwrap(),
                        ),
                    }],
                ))
                .unwrap();
            assert!(matches!(
                index.fetch_page(&pinned.cursor, 0, 100, &crate::CallContext::internal()),
                Err(IndexError::SnapshotRequired { .. })
            ));
            let refreshed = index
                .search(&Query::new("report.pdf"), &crate::CallContext::internal())
                .unwrap();
            assert_eq!(refreshed.hits[0].row.meta.size_bytes, 8);
            assert!(refreshed.generation > pinned.generation);
            let renamed = home.join("notes/renamed-document.pdf");
            std::fs::rename(&original, &renamed).unwrap();
            index
                .apply(MutationBatch::new(
                    MutationSource::Watcher,
                    vec![Mutation::Move {
                        root,
                        from: NativePath::from_path(&original),
                        to: NativePath::from_path(&renamed),
                        meta: EntryMeta::from_symlink_metadata(
                            &renamed,
                            &renamed.symlink_metadata().unwrap(),
                        ),
                    }],
                ))
                .unwrap();
            assert!(matches!(
                index.fetch_page(&refreshed.cursor, 0, 100, &crate::CallContext::internal()),
                Err(IndexError::SnapshotRequired { .. })
            ));
            assert!(
                index
                    .search(&Query::new("report.pdf"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty()
            );
            let current = index
                .search(
                    &Query::new("renamed-document.pdf"),
                    &crate::CallContext::internal(),
                )
                .unwrap();
            assert_eq!(current.hits[0].row.path.as_path(), renamed);
            let parent = index
                .entry(root, &NativePath::from_path(&home.join("notes")))
                .unwrap()
                .unwrap();
            assert_eq!(current.hits[0].row.parent, Some(parent.entry_id));
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
            let expected = index
                .search(&Query::new("report"), &crate::CallContext::internal())
                .unwrap()
                .hits
                .len();
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
                index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .len(),
                expected,
                "a warm start did not serve the committed generation"
            );
            assert!(
                index
                    .search(
                        &Query::new("appeared-while-stopped"),
                        &crate::CallContext::internal()
                    )
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
            assert!(
                !index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty()
            );
        }

        #[test]
        fn an_offline_root_keeps_its_rows_and_cannot_become_current_without_a_scan() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            let (root, _) = scan(&index, &home);

            index.mark_offline(root, "unmounted").unwrap();
            assert!(
                !index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty()
            );
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
            let batch = index
                .changes_since(floor, 100, &crate::CallContext::internal())
                .unwrap();
            assert_eq!(
                batch.changes.len(),
                8,
                "the change log must retain exactly its budget, no more and no fewer"
            );
            assert!(
                matches!(
                    index.changes_since(0, 10, &crate::CallContext::internal()),
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

        /// Contract change (WP4): an unreadable cache is rebuilt, and the bytes that could not be read
        /// are kept in the quarantine beside it rather than renamed-or-deleted in place.
        #[test]
        fn a_database_this_build_cannot_read_is_rebuilt_rather_than_refused() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file-index-v1.sqlite3");
            std::fs::write(&path, b"this is not a database").unwrap();
            let index = Index::open(&path, IndexConfig::default()).unwrap();
            assert!(index.roots().unwrap().is_empty());
            assert_eq!(index.generation().unwrap(), Generation(0));
            let quarantine = std::fs::read_dir(dir.path().join("file-index-quarantine"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(
                std::fs::read(quarantine.join("file-index-v1.sqlite3")).unwrap(),
                b"this is not a database"
            );
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

            assert_eq!(
                index
                    .search(&Query::new("report"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .len(),
                2
            );
            let scoped = index
                .search(
                    &Query::new("report").in_roots(vec![one]),
                    &crate::CallContext::internal(),
                )
                .unwrap();
            assert_eq!(scoped.hits.len(), 1);
            assert_eq!(
                scoped.roots.len(),
                1,
                "a scoped search reports its scope's freshness"
            );
            assert!(
                index
                    .search(&Query::new("   "), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty()
            );
        }

        #[test]
        fn like_wildcards_in_a_query_stay_literal() {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            tree(&home);
            let index = index_at(dir.path());
            scan(&index, &home);

            let hits = index
                .search(&Query::new("report_"), &crate::CallContext::internal())
                .unwrap()
                .hits;
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
            assert!(
                index
                    .search(&Query::new("%"), &crate::CallContext::internal())
                    .unwrap()
                    .hits
                    .is_empty()
            );
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
                .write_snapshot(
                    &mut std::io::Cursor::new(&mut frame),
                    &snapshot::SnapshotRequest::default(),
                    &crate::snapshot::SnapshotLimits::default(),
                    &crate::CallContext::internal(),
                )
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
        /// `cargo test -p templar-file-index --release -- --ignored two_million_entries_are_not_truncated`
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
                    .search(
                        &Query::new(needle).with_page_size(100),
                        &crate::CallContext::internal(),
                    )
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
        /// `cargo test -p templar-file-index --release -- --ignored --nocapture fts_trigram_benchmark_100k`
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
                        .search(
                            &Query::new(needle).with_page_size(100),
                            &crate::CallContext::internal(),
                        )
                        .unwrap();
                    best = best.min(started.elapsed());
                    hits = result.hits.len();
                }
                println!("{label:>16}: {best:?} ({hits} hits on the first page)");
            }
        }

        /// `cargo test -p templar-file-index --release -- --ignored --nocapture progressive_snapshot_benchmark`
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
                let manifest = index
                    .write_snapshot(
                        &mut std::io::Cursor::new(&mut frame),
                        &request,
                        &crate::snapshot::SnapshotLimits::default(),
                        &crate::CallContext::internal(),
                    )
                    .unwrap();
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
}
