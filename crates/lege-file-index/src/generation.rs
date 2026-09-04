//! Generations: how a rebuild publishes atomically, and why killing one costs nothing.
//!
//! A scan never touches a committed row. It writes into `entries_staging` under a generation
//! allocated when it started, and one transaction at the end merges those rows in, sweeps what the
//! pass proved absent, and moves the root's committed generation forward. Interrupt it anywhere —
//! a crash, a cancel, a budget — and the previous generation is still the one every reader sees.
//!
//! The other half of the design is the refusal to guess. Garbage collection runs **only** when the
//! pass was complete, and **only** inside the scope it covered. A budget trip, a permission wall,
//! an unreadable directory, a directory nothing could be armed on, or a cancellation makes the
//! commit incomplete, which suppresses collection entirely and lands the root in a state that says
//! so. An unread subtree must never become a deletion, and neither must an unwatched one.

use crate::{
    IndexError, Result,
    model::{
        BudgetKind, Coverage, EntryKind, EntryMeta, Generation, MetadataFreshness, NativePath,
        RootId, RootPolicy, RootState, RootStatus, ScanScope, StaleReason, now_ms,
    },
    mutation::{Change, ChangeOp},
    schema,
};
use rusqlite::{Connection, Transaction};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A cooperative stop signal. Cheap to clone and to poll, so a background repair can be abandoned
/// the moment the user does something more important.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a scan is allowed to spend. Every field defaults to `None`, which means **uncapped** —
/// the product cap this crate replaced is not a default hiding somewhere else.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanBudget {
    pub max_entries: Option<u64>,
    pub max_bytes: Option<u64>,
    pub max_duration: Option<Duration>,
    pub max_depth: Option<u32>,
}

impl ScanBudget {
    #[must_use]
    pub fn unlimited() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_max_entries(mut self, entries: u64) -> Self {
        self.max_entries = Some(entries);
        self
    }

    #[must_use]
    pub fn with_max_depth(mut self, depth: u32) -> Self {
        self.max_depth = Some(depth);
        self
    }
}

/// How to run one pass.
#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    pub budget: ScanBudget,
    pub policy: RootPolicy,
    pub cancel: CancelToken,
    pub scope: ScanScope,
    /// Stamped on the root while it is `building`, so a background repair reads as
    /// `building (reconcile)` rather than as an unexplained rebuild.
    pub reason: Option<StaleReason>,
    /// The path coverage rules are evaluated relative to. `None` means the root's own path, which
    /// [`crate::Index::begin_scan`] fills in — so a root registered *inside* a directory the rules
    /// would classify (`~/code/target`, say) is still indexed as the user asked.
    pub base: Option<NativePath>,
}

impl ScanOptions {
    #[must_use]
    pub fn reconcile(policy: RootPolicy) -> Self {
        Self {
            policy,
            reason: Some(StaleReason::Reconcile),
            ..Self::default()
        }
    }
}

/// What a pass actually managed to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOutcome {
    pub generation: Generation,
    pub entries: u64,
    /// Whether the pass covered its whole scope. **The only thing that permits collection, and
    /// the only route to `current`.**
    pub complete: bool,
    pub limited: Option<BudgetKind>,
    pub permission_denied: u64,
    /// Directories the pass could not read at all, permission denials included.
    pub unreadable_dirs: u64,
    /// Directories the pass read but could not arm a watch on. Legible, and unobserved from the
    /// next change onwards — which is why it counts against completeness exactly as a read
    /// failure does.
    pub unwatched_dirs: u64,
    /// Whether this pass re-read every entry's metadata, or took some of it from the index. A
    /// namespace-only repair is `Unverified` by construction and complete all the same.
    pub metadata: MetadataFreshness,
    pub elapsed: Duration,
    pub state: RootState,
}

/// What [`ScanSession::commit`] worked out before it opened the transaction.
#[derive(Clone, Copy, Debug)]
struct Committing {
    complete: bool,
    metadata: MetadataFreshness,
    retention: u64,
    max_changes: u64,
    oldest_cursor: Option<Generation>,
}

/// One entry the walker found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedEntry {
    pub path: NativePath,
    pub parent: Option<NativePath>,
    pub meta: EntryMeta,
}

/// An open generation. Feed it entries, then commit or abort.
#[derive(Debug)]
pub struct ScanSession {
    index: crate::Index,
    root: RootId,
    generation: Generation,
    options: ScanOptions,
    claim: Option<crate::ScanClaim>,
    buffered: Vec<(ScannedEntry, Coverage)>,
    staged: u64,
    limited: Option<BudgetKind>,
    stopped: bool,
    permission_denied: u64,
    unreadable_dirs: u64,
    unwatched_dirs: u64,
    watches_armed: u64,
    unverified_children: u64,
    started: Instant,
    finished: bool,
}

impl ScanSession {
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub fn root(&self) -> RootId {
        self.root
    }

    #[must_use]
    pub fn options(&self) -> &ScanOptions {
        &self.options
    }

    /// Which budget, if any, has already made this pass incomplete.
    #[must_use]
    pub fn limited(&self) -> Option<BudgetKind> {
        self.limited
    }

    /// Whether walking must stop now. A depth budget makes a pass incomplete without ending it —
    /// the rest of the tree is still worth indexing — while an entry, byte or time budget ends it.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Records a budget. Only the whole-pass budgets stop the walk.
    pub fn set_limited(&mut self, kind: BudgetKind) {
        if self.limited.is_none() {
            self.limited = Some(kind);
        }
        if kind != BudgetKind::Depth {
            self.stopped = true;
        }
    }

    /// Records that a directory could not be read. Never a hard failure: the answer to "I cannot
    /// see part of this tree" is an incomplete pass, not an empty one.
    pub fn note_unreadable(&mut self, denied: bool) {
        self.unreadable_dirs += 1;
        if denied {
            self.permission_denied += 1;
        }
    }

    /// Records a directory the pass read but could not arm a watch on.
    ///
    /// This is the other half of "I cannot see part of this tree", and until this existed it was
    /// counted and then ignored: an unwatched subtree committed `current`, and the next complete
    /// pass tombstoned everything it had not re-observed. `armed` is the watcher's own count at
    /// the moment of the refusal, so the reason a person reads names the watch budget rather than
    /// an unexplained failure.
    pub fn note_unwatched(&mut self, armed: u64) {
        self.unwatched_dirs += 1;
        self.watches_armed = armed;
    }

    /// Records an entry whose metadata came from the index rather than from the filesystem.
    ///
    /// This does **not** make the pass incomplete: the namespace is still exhaustive, which is all
    /// [`RootState::Current`] has ever claimed. It moves the other axis, so a `sort:size` query
    /// can tell that the sizes it is ordering by were not re-read while a filename query pays
    /// nothing for the distinction.
    pub fn note_unverified_metadata(&mut self) {
        self.unverified_children += 1;
    }

    /// The tier a path is stored at under this pass's policy, or `None` when no row is written
    /// for it. The one answer the walker asks before descending and `observe` stamps on the row,
    /// so the two cannot drift.
    #[must_use]
    pub fn coverage_for(&self, path: &NativePath, kind: EntryKind) -> Option<Coverage> {
        match &self.options.base {
            Some(base) => self.options.policy.stored_coverage_within(base, path, kind),
            None => self.options.policy.stored_coverage(path, kind),
        }
    }

    /// Adds one entry to the open generation.
    pub fn observe(&mut self, entry: ScannedEntry) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        if self.options.cancel.is_cancelled() {
            self.set_limited(BudgetKind::Duration);
            return Ok(());
        }
        if let Some(max) = self.options.budget.max_entries
            && self.staged + self.buffered.len() as u64 >= max
        {
            self.set_limited(BudgetKind::Entries);
            return Ok(());
        }
        if let Some(max) = self.options.budget.max_duration
            && self.started.elapsed() >= max
        {
            self.set_limited(BudgetKind::Duration);
            return Ok(());
        }
        // Coverage is stamped here and nowhere else on the scan path, so the walker, the
        // reconciler and `arm_tree`'s visitor cannot disagree about what tier a row is at — and a
        // path no row is written for is dropped before it can spend any of the budget.
        let Some(coverage) = self.coverage_for(&entry.path, entry.meta.kind) else {
            return Ok(());
        };
        self.buffered.push((entry, coverage));
        if self.buffered.len() >= self.index.config().scan_flush_rows {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.buffered);
        let generation = self.generation.0 as i64;
        let root = self.root.0;
        let staged = self.index.with_write(|connection| {
            let transaction = connection.transaction()?;
            {
                let mut statement = transaction.prepare_cached(
                    "INSERT INTO entries_staging
                       (generation, root_id, name_bytes, name_display, name_fold, path_bytes,
                        path_display, parent_bytes, kind, size_bytes, alloc_bytes, mtime_ns,
                        ctime_ns, mode, hidden, dev, inode, nlink, mime, coverage)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                             ?16, ?17, ?18, ?19, ?20)
                     ON CONFLICT (root_id, path_bytes) DO NOTHING",
                )?;
                for (entry, coverage) in &rows {
                    let name = entry.path.file_name().unwrap_or_else(|| entry.path.clone());
                    statement.execute(rusqlite::params![
                        generation,
                        root,
                        name.as_bytes(),
                        name.display(),
                        fold(&name.display()),
                        entry.path.as_bytes(),
                        entry.path.display(),
                        entry.parent.as_ref().map(NativePath::as_bytes),
                        entry.meta.kind.as_i64(),
                        entry.meta.size_bytes as i64,
                        entry.meta.alloc_bytes as i64,
                        entry.meta.mtime_ns,
                        entry.meta.ctime_ns,
                        i64::from(entry.meta.mode),
                        i64::from(entry.meta.hidden),
                        entry.meta.object_key.map(|key| key.dev as i64),
                        entry.meta.object_key.map(|key| key.ino as i64),
                        i64::from(entry.meta.nlink),
                        entry.meta.mime,
                        coverage.as_i64(),
                    ])?;
                }
            }
            transaction.commit()?;
            Ok(rows.len() as u64)
        })?;
        self.staged += staged;

        if let Some(max) = self.options.budget.max_bytes
            && self.database_bytes()? >= max
        {
            self.set_limited(BudgetKind::Bytes);
        }
        Ok(())
    }

    fn database_bytes(&self) -> Result<u64> {
        self.index.with_read(|connection| {
            let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            Ok((pages * size) as u64)
        })
    }

    /// Publishes the generation. One transaction: merge, collect (only if complete, only in
    /// scope), refresh the accelerator, log the change, move the root forward.
    pub fn commit(mut self) -> Result<ScanOutcome> {
        self.flush()?;
        self.finished = true;

        // A cancellation is a duration budget by another name; recording it as one keeps the
        // reason a person reads from depending on whether `observe` happened to be called again
        // after the token was set.
        if self.options.cancel.is_cancelled() {
            self.set_limited(BudgetKind::Duration);
        }
        // Completeness is "this pass saw its whole scope". Every way of not seeing it counts:
        // a budget, a permission wall, a directory that would not open, and a directory nothing
        // is watching. `unreadable_dirs` was counted and reported here from the start and never
        // consulted, so a pass that could not read a subtree committed `current` and the next
        // complete pass turned that subtree into deletions.
        let complete =
            self.limited.is_none() && self.unreadable_dirs == 0 && self.unwatched_dirs == 0;
        // The other axis. A pass vouches for metadata only when it re-read all of it *and* saw its
        // whole scope: rows it never reached keep whatever they had, which is by definition not
        // something this pass verified.
        let metadata = if complete && self.unverified_children == 0 {
            MetadataFreshness::Verified
        } else {
            MetadataFreshness::Unverified
        };
        let generation = self.generation;
        let root = self.root;
        let retention = self.index.config().tombstone_retention_generations;
        let max_changes = self.index.config().max_changes;
        let oldest_cursor = self.index.cursors().lock().oldest_generation();
        let permission_denied = self.permission_denied;
        let limited = self.limited;
        let reason = self.options.reason.clone();

        let (entries, state) = self.index.with_write(|connection| {
            self.commit_transaction(
                connection,
                Committing {
                    complete,
                    metadata,
                    retention,
                    max_changes,
                    oldest_cursor,
                },
            )
        })?;

        let _ = reason;
        let outcome = ScanOutcome {
            generation,
            entries,
            complete,
            limited,
            permission_denied,
            unreadable_dirs: self.unreadable_dirs,
            unwatched_dirs: self.unwatched_dirs,
            metadata,
            elapsed: self.started.elapsed(),
            state,
        };
        let status = self.index.root_status(root)?;
        self.index.notify_root(&status);
        if let Some(sink) = self.index.sink() {
            sink.generation_advanced(generation, &[]);
        }
        self.claim.take();
        Ok(outcome)
    }

    /// What the root becomes, and why.
    ///
    /// The order is the order a person would want to be told in. A budget is the operator's own
    /// doing; a refused watch sends the reader to `fs.inotify.max_user_watches`; a permission wall
    /// sends them to a directory's mode; anything left is an i/o fault. Collapsing all four into
    /// `permission-denied`, which is what this did while `unreadable_dirs` went unconsulted, sends
    /// every reader to the wrong place.
    fn committed_state(&self, complete: bool) -> (RootState, Option<String>) {
        if complete {
            return (RootState::Current, None);
        }
        if let Some(kind) = self.limited {
            let reason = StaleReason::BudgetExhausted { budget: kind };
            return (RootState::Stale, Some(reason.as_string()));
        }
        if self.unwatched_dirs > 0 {
            let reason = StaleReason::WatcherExhausted {
                armed: self.watches_armed,
                refused: self.unwatched_dirs,
            };
            return (RootState::Stale, Some(reason.as_string()));
        }
        if self.unreadable_dirs > self.permission_denied {
            let reason = StaleReason::Unreadable {
                directories: self.unreadable_dirs - self.permission_denied,
            };
            return (RootState::Stale, Some(reason.as_string()));
        }
        // Every gap was a permission wall and nothing else stopped the pass: the one shape of
        // incompleteness with its own state, because re-running the scan will not fix it.
        (
            RootState::PermissionLimited,
            Some(StaleReason::PermissionDenied.as_string()),
        )
    }

    /// The body of [`Self::commit`]'s one transaction, pulled out only so the method above stays
    /// short: merge staging into `entries`, collect (only if `complete`, only in this session's
    /// scope), refresh the accelerator, log the rescan marker, and move the root forward.
    fn commit_transaction(
        &self,
        connection: &mut Connection,
        committing: Committing,
    ) -> Result<(u64, RootState)> {
        let Committing {
            complete,
            metadata,
            retention,
            max_changes,
            oldest_cursor,
        } = committing;
        let root = self.root;
        let generation = self.generation;
        let scope = &self.options.scope;

        let transaction = connection.transaction()?;
        let generation_id = generation.0 as i64;

        merge_staging_into_entries(&transaction, root, generation_id)?;

        // Collection: only for a complete pass, only inside the scope it covered.
        let collected = if complete {
            collect_tombstones(&transaction, root, generation_id, scope)?
        } else {
            0
        };

        refresh_fts(&transaction, root, generation)?;

        // A wholesale re-enumeration is one marker, not a million events: a consumer whose
        // projection spans this root re-reads it rather than replaying it.
        let change = Change {
            seq: 0,
            generation,
            root,
            op: ChangeOp::Rescan,
            entry_id: None,
            path: read_root_path(&transaction, root)?,
            old_path: None,
            kind: EntryKind::Directory,
            size_bytes: 0,
            mtime_ns: 0,
            at_ms: now_ms(),
        };
        crate::mutation::append_change(&transaction, &change)?;
        crate::mutation::prune_changes(&transaction, max_changes)?;

        prune_tombstones(&transaction, generation, retention, oldest_cursor)?;

        let entries: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM entries WHERE root_id = ?1 AND deleted_generation IS NULL",
            [root.0],
            |row| row.get(0),
        )?;

        let (state, state_reason) = self.committed_state(complete);

        transaction.execute(
            "UPDATE roots
                SET committed_generation = ?2, building_generation = NULL, complete = ?3,
                    entry_count = ?4, last_scan_ms = ?5,
                    last_reconcile_finished_ms = ?5, last_error = NULL
              WHERE root_id = ?1",
            rusqlite::params![
                root.0,
                generation_id,
                i64::from(complete),
                entries,
                now_ms()
            ],
        )?;
        // A subtree pass may only *downgrade* the root's metadata axis. It looked at part of the
        // root, so its verifying half says nothing about the rest — and claiming otherwise here
        // would be the same class of lie the namespace axis already refuses to tell.
        if matches!(scope, ScanScope::Root) || metadata == MetadataFreshness::Unverified {
            transaction.execute(
                "UPDATE roots SET metadata_freshness = ?2 WHERE root_id = ?1",
                rusqlite::params![root.0, metadata.as_str()],
            )?;
        }
        crate::set_state(&transaction, root, state, state_reason.as_deref())?;

        let head: u64 = schema::read_meta(&transaction, "committed_generation")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if generation.0 > head {
            schema::write_meta(
                &transaction,
                "committed_generation",
                &generation.0.to_string(),
            )?;
        }

        transaction.execute(
            "DELETE FROM entries_staging WHERE generation = ?1 AND root_id = ?2",
            rusqlite::params![generation_id, root.0],
        )?;
        transaction.commit()?;

        tracing::info!(
            root = root.0,
            %generation,
            entries,
            collected,
            complete,
            "committed an index generation"
        );
        Ok((entries as u64, state))
    }

    /// Throws the generation away. The previous one stays visible; that is the whole point.
    pub fn abort(mut self) -> Result<()> {
        self.finished = true;
        self.abort_inner()
    }

    fn abort_inner(&mut self) -> Result<()> {
        let generation = self.generation.0 as i64;
        let root = self.root.0;
        self.index.with_write(|connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "DELETE FROM entries_staging WHERE generation = ?1 AND root_id = ?2",
                rusqlite::params![generation, root],
            )?;
            let previous: i64 = transaction.query_row(
                "SELECT committed_generation FROM roots WHERE root_id = ?1",
                [root],
                |row| row.get(0),
            )?;
            let state = if previous == 0 {
                RootState::Empty
            } else {
                RootState::Stale
            };
            transaction.execute(
                "UPDATE roots SET building_generation = NULL WHERE root_id = ?1",
                [root],
            )?;
            crate::set_state(
                &transaction,
                RootId(root),
                state,
                if previous == 0 {
                    None
                } else {
                    Some("aborted-scan")
                },
            )?;
            transaction.commit()?;
            Ok(())
        })?;
        self.claim.take();
        Ok(())
    }
}

impl Drop for ScanSession {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.abort_inner()
        {
            tracing::warn!(%error, "could not clean up an abandoned scan generation");
        }
    }
}

impl crate::Index {
    /// Opens a generation over `root`. The root moves to `building`; its committed generation
    /// keeps serving every reader until [`ScanSession::commit`].
    pub fn begin_scan(&self, root: RootId, mut options: ScanOptions) -> Result<ScanSession> {
        let claim = self.claim_scan(root)?;
        if options.base.is_none() {
            options.base = Some(self.root_status(root)?.path);
        }
        let generation = self.with_write(|connection| {
            let transaction = connection.transaction()?;
            let generation = crate::next_generation(&transaction)?;
            crate::set_state(
                &transaction,
                root,
                RootState::Building,
                options
                    .reason
                    .as_ref()
                    .map(StaleReason::as_string)
                    .as_deref(),
            )?;
            transaction.execute(
                "UPDATE roots SET building_generation = ?2, last_reconcile_started_ms = ?3
                  WHERE root_id = ?1",
                rusqlite::params![root.0, generation.0 as i64, now_ms()],
            )?;
            transaction.commit()?;
            Ok(generation)
        })?;
        let status = self.root_status(root)?;
        self.notify_root(&status);
        Ok(ScanSession {
            index: self.clone(),
            root,
            generation,
            options,
            claim: Some(claim),
            buffered: Vec::new(),
            staged: 0,
            limited: None,
            stopped: false,
            permission_denied: 0,
            unreadable_dirs: 0,
            unwatched_dirs: 0,
            watches_armed: 0,
            unverified_children: 0,
            started: Instant::now(),
            finished: false,
        })
    }

    /// Walks a root with this crate's own walker and commits. The no-watcher path: this crate's
    /// tests, a cold CLI run, a first index before anything is armed.
    pub fn scan_root(&self, root: RootId, options: ScanOptions) -> Result<ScanOutcome> {
        let status = self.root_status(root)?;
        let mut session = self.begin_scan(root, options)?;
        crate::scan::walk_into(&mut session, &status.path)?;
        session.commit()
    }

    /// A repair pass: the same walk, stamped as a reconciliation so the root reads as
    /// `building (reconcile)` while it runs.
    pub fn reconcile_root(&self, root: RootId, mut options: ScanOptions) -> Result<ScanOutcome> {
        if options.reason.is_none() {
            options.reason = Some(StaleReason::Reconcile);
        }
        self.scan_root(root, options)
    }

    /// Walks one subtree instead of the whole root, and collects only inside it.
    ///
    /// The walk starts at `start` rather than at the root's path, which `scan_root` does not do
    /// even when handed a `ScanScope::Subtree` — there the scope bounds collection only.
    pub fn scan_subtree(
        &self,
        root: RootId,
        start: &NativePath,
        mut options: ScanOptions,
    ) -> Result<ScanOutcome> {
        let status = self.root_status(root)?;
        if !start.starts_with(&status.path) {
            return Err(IndexError::Unsupported {
                detail: format!(
                    "{} is not inside root {}",
                    start.display(),
                    status.path.display()
                ),
            });
        }
        options.scope = ScanScope::Subtree(start.clone());
        let mut session = self.begin_scan(root, options)?;
        crate::scan::walk_into(&mut session, start)?;
        session.commit()
    }

    /// Materialise an `Opaque` subtree on demand.
    ///
    /// The rows under a build directory are not written by an ordinary pass — that is what
    /// `Opaque` means. A consumer that wants them (a drive visualiser drilling into `target/`)
    /// asks for this: the same `reconcile`/`ScanScope::Subtree` machinery, run once with one extra
    /// rule in front of the policy, storing the descendants at [`Coverage::Inventory`] — present
    /// and sized, still invisible to a filename search. The root's stored policy is untouched, so
    /// nothing about this expansion outlives the rows it wrote, and the next full pass drops back
    /// to `Opaque` unless the caller asks again.
    pub fn materialise_subtree(&self, root: RootId, subtree: &NativePath) -> Result<ScanOutcome> {
        let policy = self
            .root_policy(root)?
            .materialising(subtree, Coverage::Inventory);
        self.scan_subtree(
            root,
            subtree,
            ScanOptions {
                policy,
                reason: Some(StaleReason::Reconcile),
                ..ScanOptions::default()
            },
        )
    }

    /// The status of a root without a scan — what a warm start serves.
    pub fn warm_status(&self, root: RootId) -> Result<RootStatus> {
        self.root_status(root)
    }
}

/// Rewrites the accelerator's rows for everything this generation wrote, and drops the rows of
/// everything it collected.
///
/// The `coverage` predicate on the insert is the first of the two places a non-`Detailed` row can
/// leak into a search, and it is the cheap one to get right: this function is called from
/// `ScanSession::commit_transaction` **and** from `mutation::apply_transaction`, so one condition
/// covers the scanner, the reconciler and every live watcher mutation. The delete above it is
/// unconditional on purpose — a row demoted out of `Detailed` by a policy change has to leave the
/// accelerator, and it leaves by not being re-inserted.
pub(crate) fn refresh_fts(
    transaction: &Transaction<'_>,
    root: RootId,
    generation: Generation,
) -> Result<()> {
    if !fts_present(transaction) {
        return Ok(());
    }
    let generation = generation.0 as i64;
    // `UNION ALL` of two single-column tests rather than one `OR`: an `OR` across two columns
    // cannot use either index, so this was a full scan of the root on every commit.
    transaction.execute(
        "DELETE FROM entries_fts
          WHERE rowid IN (
                SELECT entry_id FROM entries WHERE root_id = ?1 AND written_generation = ?2
                UNION ALL
                SELECT entry_id FROM entries WHERE root_id = ?1 AND deleted_generation = ?2)",
        rusqlite::params![root.0, generation],
    )?;
    transaction.execute(
        &format!(
            "INSERT INTO entries_fts (rowid, name_display, path_display)
             SELECT entry_id, name_display, path_display FROM entries
              WHERE root_id = ?1 AND written_generation = ?2 AND deleted_generation IS NULL
                AND coverage = {searchable}",
            searchable = Coverage::Detailed.as_i64()
        ),
        rusqlite::params![root.0, generation],
    )?;
    Ok(())
}

pub(crate) fn fts_present(connection: &rusqlite::Connection) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE name = 'entries_fts'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
}

/// The body of [`ScanSession::commit_transaction`]'s merge step, pulled out only so that function
/// stays short: staged rows for this generation are upserted into `entries`, then parent links
/// are resolved now that the rows they point at exist.
fn merge_staging_into_entries(
    transaction: &Transaction<'_>,
    root: RootId,
    generation_id: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO entries
           (root_id, parent_id, name_bytes, name_display, name_fold, path_bytes,
            path_display, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden,
            dev, inode, nlink, mime, coverage, written_generation, last_seen_generation,
            deleted_generation)
         SELECT root_id, NULL, name_bytes, name_display, name_fold, path_bytes,
                path_display, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode,
                hidden, dev, inode, nlink, mime, coverage, ?1, ?1, NULL
           FROM entries_staging WHERE generation = ?1 AND root_id = ?2
         ON CONFLICT (root_id, path_bytes) DO UPDATE SET
            name_bytes = excluded.name_bytes,
            name_display = excluded.name_display,
            name_fold = excluded.name_fold,
            kind = excluded.kind,
            size_bytes = excluded.size_bytes,
            alloc_bytes = excluded.alloc_bytes,
            mtime_ns = excluded.mtime_ns,
            ctime_ns = excluded.ctime_ns,
            mode = excluded.mode,
            hidden = excluded.hidden,
            dev = excluded.dev,
            inode = excluded.inode,
            nlink = excluded.nlink,
            mime = excluded.mime,
            coverage = excluded.coverage,
            written_generation = ?1,
            last_seen_generation = ?1,
            deleted_generation = NULL",
        rusqlite::params![generation_id, root.0],
    )?;

    // Parent links, resolved once the rows they point at exist.
    transaction.execute(
        "UPDATE entries AS e
            SET parent_id = (SELECT p.entry_id FROM entries p
                              WHERE p.root_id = e.root_id
                                AND p.path_bytes = (SELECT s.parent_bytes
                                                      FROM entries_staging s
                                                     WHERE s.generation = ?1
                                                       AND s.root_id = e.root_id
                                                       AND s.path_bytes = e.path_bytes))
          WHERE e.root_id = ?2 AND e.written_generation = ?1",
        rusqlite::params![generation_id, root.0],
    )?;
    Ok(())
}

/// The body of [`ScanSession::commit_transaction`]'s collection step: tombstones every row **not**
/// seen this generation, scoped to whatever the session actually covered — a subtree repair must
/// never garbage-collect the rest of the root.
fn collect_tombstones(
    transaction: &Transaction<'_>,
    root: RootId,
    generation_id: i64,
    scope: &ScanScope,
) -> Result<usize> {
    Ok(match scope {
        ScanScope::Root => transaction.execute(
            "UPDATE entries SET deleted_generation = ?1
              WHERE root_id = ?2 AND deleted_generation IS NULL
                AND last_seen_generation < ?1",
            rusqlite::params![generation_id, root.0],
        )?,
        ScanScope::Subtree(prefix) => {
            let (low, high) = prefix.subtree_bounds();
            transaction.execute(
                "UPDATE entries SET deleted_generation = ?1
                  WHERE root_id = ?2 AND deleted_generation IS NULL
                    AND last_seen_generation < ?1
                    AND path_bytes >= ?3 AND path_bytes < ?4",
                rusqlite::params![generation_id, root.0, low, high],
            )?
        }
    })
}

/// Drops tombstones nobody can still be looking at: older than the retention window, and older
/// than the oldest generation any live cursor is pinned to.
fn prune_tombstones(
    transaction: &Transaction<'_>,
    generation: Generation,
    retention: u64,
    oldest_cursor: Option<Generation>,
) -> Result<()> {
    let mut floor = generation.0.saturating_sub(retention);
    if let Some(cursor) = oldest_cursor {
        floor = floor.min(cursor.0.saturating_sub(1));
    }
    if floor == 0 {
        return Ok(());
    }
    transaction.execute(
        "DELETE FROM entries WHERE deleted_generation IS NOT NULL AND deleted_generation <= ?1",
        [floor as i64],
    )?;
    Ok(())
}

fn read_root_path(transaction: &Transaction<'_>, root: RootId) -> Result<NativePath> {
    let bytes: Vec<u8> = transaction.query_row(
        "SELECT path_bytes FROM roots WHERE root_id = ?1",
        [root.0],
        |row| row.get(0),
    )?;
    Ok(NativePath::from_bytes(bytes))
}

/// Case folding for exact and prefix matching. Lowercasing covers the names people search for
/// without dragging in a collation table.
#[must_use]
pub fn fold(value: &str) -> String {
    value.to_lowercase()
}

/// Turns an i/o error into the honest question: was this permission, or absence?
#[must_use]
pub fn is_permission_denied(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

impl From<IndexError> for std::io::Error {
    fn from(error: IndexError) -> Self {
        Self::other(error.to_string())
    }
}
