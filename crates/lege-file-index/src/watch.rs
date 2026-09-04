//! One watcher owner: arms watches, normalizes kernel events, and reports what it could not see.
//!
//! Linux gives no persistent per-user change journal, so a watcher can only describe the daemon's
//! own lifetime, and only as far as the kernel's queue and watch limits allow. This module is
//! written around that limit rather than against it: every path that loses information — a queue
//! overflow, a refused watch, a rename whose other half never arrived, a mount that vanished —
//! produces an explicit repair request, never a silent gap in a generation that still calls itself
//! current (C5 §4: nothing enters `Current` but a complete pass).
//!
//! Backend choice, measured on the reference laptop (kernel 6.17): `fs.inotify.max_user_watches`
//! is 250,074 and `$HOME` holds 157,283 directories once the index's skip policy is applied, so
//! one home root costs roughly 63% of the user's entire watch budget; `max_queued_events` is
//! 16,384, which an ordinary `cargo build` can overrun. fanotify's answer to both — one mark per
//! mount instead of one per directory — needs `CAP_SYS_ADMIN`, which an unprivileged per-user
//! `lege-filed` does not have and should not acquire, and unprivileged fanotify is still one mark
//! per inode. So: inotify, per directory, with exhaustion and overflow treated as ordinary
//! operating conditions.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    EntryKind, EntryMeta, Mutation, NativePath, RootId, RootPolicy, ScanScope, ScannedEntry,
    StaleReason, WatchStatus,
};

/// The fixture guard.
///
/// A watcher on the wrong directory is not a failing test, it is a developer's home directory
/// being walked by a test run. The trash incident cost real files to exactly this class of
/// mistake, and the rule that came out of it — fixtures only, never a real root — has lived in
/// prose ever since (`docs/generated/CURRENT-STATE.md`). This makes it a panic.
pub mod guard {
    use std::path::Path;
    use std::sync::OnceLock;

    fn active() -> bool {
        static ACTIVE: OnceLock<bool> = OnceLock::new();
        *ACTIVE.get_or_init(|| {
            if cfg!(test) || std::env::var_os("LEGE_INDEX_FIXTURE_GUARD").is_some() {
                return true;
            }
            // Cargo's test binaries live in `target/<profile>/deps/`. An integration test is a
            // separate binary, so `cfg!(test)` is false inside this library when one runs it —
            // without this check the guard would cover unit tests only, which is the half that
            // needs it least.
            std::env::current_exe()
                .is_ok_and(|exe| exe.components().any(|part| part.as_os_str() == "deps"))
        })
    }

    /// Panic unless `path` is inside the temporary directory.
    pub fn assert_fixture_root(path: &Path) {
        if !active() {
            return;
        }
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        assert!(
            real.starts_with(&temp),
            "refusing to watch {} from a test binary: watchers may only be opened on paths under \
             {} (use tempfile::tempdir()). This guard exists because a test that walks or watches \
             a real root walks the developer's own files.",
            real.display(),
            temp.display()
        );
    }
}

/// A watch on one directory.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WatchId(pub u64);

/// What the kernel said, before it means anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawEvent {
    pub watch: WatchId,
    /// The entry's name within the watched directory; absent for events about the directory
    /// itself.
    pub name: Option<Vec<u8>>,
    pub kind: RawKind,
    /// Pairs `MovedFrom` with `MovedTo`. Zero when the kernel supplied none.
    pub cookie: u32,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawKind {
    Created,
    Deleted,
    /// A writer closed the file: the point at which size and mtime are worth reading.
    Written,
    /// Permissions, ownership or times changed.
    Attributes,
    MovedFrom,
    MovedTo,
    /// The watched directory itself went away or moved.
    SelfGone,
    /// The filesystem holding the watch was unmounted.
    Unmounted,
    /// The kernel dropped events. Everything since the last known-good point is unknown.
    Overflow,
    /// The kernel removed this watch (the directory is gone, or the watch was replaced).
    WatchDropped,
}

/// What the kernel will let us have.
#[derive(Clone, Copy, Debug, Default)]
pub struct WatchLimits {
    pub max_user_watches: Option<u64>,
    pub max_queued_events: Option<u64>,
    pub max_user_instances: Option<u64>,
}

impl WatchLimits {
    /// Read `/proc/sys/fs/inotify/*`. Absent files mean "unknown", not zero.
    #[must_use]
    pub fn read() -> Self {
        fn value(name: &str) -> Option<u64> {
            std::fs::read_to_string(format!("/proc/sys/fs/inotify/{name}"))
                .ok()?
                .trim()
                .parse()
                .ok()
        }
        Self {
            max_user_watches: value("max_user_watches"),
            max_queued_events: value("max_queued_events"),
            max_user_instances: value("max_user_instances"),
        }
    }
}

/// How much of the kernel's watch budget this daemon is willing to take.
///
/// The measurement above is the reason this exists: a single home root can ask for two thirds of
/// the per-user budget, and the user's editors, language servers and file managers want watches
/// too. Taking the lot would break other software; taking a share and admitting the shortfall is
/// the honest option.
#[derive(Clone, Copy, Debug)]
pub struct WatchBudget {
    pub max_watches: u64,
}

impl WatchBudget {
    /// Half of the kernel's per-user allowance, or a conservative fixed ceiling if it cannot be
    /// read.
    #[must_use]
    pub fn from_limits(limits: WatchLimits) -> Self {
        Self {
            max_watches: limits.max_user_watches.map_or(65_536, |max| max / 2),
        }
    }
}

impl Default for WatchBudget {
    fn default() -> Self {
        Self::from_limits(WatchLimits::read())
    }
}

/// Interrupts a blocking [`EventSource::poll`] from another thread.
///
/// The whole point of an unbounded `poll(None)` is that an idle watcher costs nothing — no timer,
/// no timeslice, no wakeup. That is only safe if something other than a filesystem event can end
/// the wait, because shutdown and a consumer arriving are both reasons to act that the kernel
/// will never send. This is that something: a cheap, cloneable, `Send + Sync` poke.
#[derive(Clone)]
pub struct SourceWaker(std::sync::Arc<dyn Fn() + Send + Sync>);

impl SourceWaker {
    /// Build a waker from whatever the backend uses to interrupt itself.
    #[must_use]
    pub fn new(wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        Self(wake)
    }

    /// End an in-flight `poll(None)`. Best effort: a source that has already stopped is not an
    /// error, because the loop is about to notice that for itself.
    pub fn wake(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for SourceWaker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SourceWaker")
    }
}

/// Where raw events come from. The kernel in production, a script in tests.
pub trait EventSource: Send {
    fn backend(&self) -> &'static str;
    fn limits(&self) -> WatchLimits;
    /// Arm a watch on one directory.
    fn add_watch(&mut self, dir: &Path) -> std::io::Result<WatchId>;
    fn remove_watch(&mut self, watch: WatchId);
    /// Collect whatever has arrived. `Some(timeout)` bounds the wait; `None` blocks until an
    /// event or a [`SourceWaker`] arrives, which is what lets an idle watcher cost nothing.
    fn poll(&mut self, timeout: Option<Duration>) -> std::io::Result<Vec<RawEvent>>;
    /// A handle another thread can use to end a `poll(None)`.
    ///
    /// `None` is the honest answer for a source that never truly blocks — a scripted one, say —
    /// and the caller must then never pass `None` as a timeout it expects to be interrupted.
    fn waker(&self) -> Option<SourceWaker> {
        None
    }
}

/// A repair the index owes itself, discovered by the watcher.
#[derive(Clone, Debug)]
pub struct RepairRequest {
    pub root: RootId,
    pub scope: ScanScope,
    pub reason: StaleReason,
    /// Whether the whole root's freshness is now in doubt. An overflow means it is; a directory
    /// that arrived by rename means only that subtree is unknown.
    pub mark_root_stale: bool,
}

/// One pump of the event source.
#[derive(Debug, Default)]
pub struct Pumped {
    pub mutations: Vec<Mutation>,
    pub repairs: Vec<RepairRequest>,
    /// Roots whose filesystem went away underneath us.
    pub offline: Vec<RootId>,
    /// The source itself failed. Not a lost event but a lost *stream*: nothing will ever arrive
    /// again, and a loop that keeps polling a dead source spins instead of waiting.
    pub failed: Option<String>,
}

#[derive(Clone, Debug)]
struct WatchedDir {
    root: RootId,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct WatchedRoot {
    id: RootId,
    path: PathBuf,
    policy: RootPolicy,
}

#[derive(Clone, Debug)]
struct PendingMove {
    root: RootId,
    from: PathBuf,
    is_dir: bool,
    at_ms: i64,
}

/// The one owner of every watch in the process.
#[derive(Debug)]
pub struct WatchOwner<S: EventSource> {
    source: S,
    budget: WatchBudget,
    roots: Vec<WatchedRoot>,
    dirs: HashMap<WatchId, WatchedDir>,
    /// Reverse map, so a directory is never watched twice.
    by_path: HashMap<PathBuf, WatchId>,
    pending_moves: HashMap<u32, PendingMove>,
    /// How long an unpaired rename half is held before it is read as a move across the watched
    /// boundary. One coalescing window is enough: the two halves are delivered together.
    move_pairing_ms: i64,
    /// The largest subtree the watcher will enumerate inline when a directory appears. Anything
    /// larger becomes a scheduled repair instead of a stall in the event loop.
    inline_subtree_limit: u64,
    armed: u64,
    refused: u64,
    overflow_count: u64,
    last_event_ms: Option<i64>,
    last_overflow_ms: Option<i64>,
    last_error: Option<String>,
}

impl<S: EventSource> WatchOwner<S> {
    #[must_use]
    pub fn new(source: S, budget: WatchBudget) -> Self {
        Self {
            source,
            budget,
            roots: Vec::new(),
            dirs: HashMap::new(),
            by_path: HashMap::new(),
            pending_moves: HashMap::new(),
            move_pairing_ms: 250,
            inline_subtree_limit: 1000,
            armed: 0,
            refused: 0,
            overflow_count: 0,
            last_event_ms: None,
            last_overflow_ms: None,
            last_error: None,
        }
    }

    /// Register a root. Nothing is armed until [`WatchOwner::arm_tree`] runs.
    pub fn track_root(&mut self, root: RootId, path: &Path, policy: RootPolicy) {
        guard::assert_fixture_root(path);
        self.roots.retain(|existing| existing.id != root);
        self.roots.push(WatchedRoot {
            id: root,
            path: path.to_path_buf(),
            policy,
        });
    }

    pub fn forget_root(&mut self, root: RootId) {
        self.roots.retain(|existing| existing.id != root);
        let ids: Vec<WatchId> = self
            .dirs
            .iter()
            .filter(|(_, dir)| dir.root == root)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(dir) = self.dirs.remove(&id) {
                self.by_path.remove(&dir.path);
            }
            self.source.remove_watch(id);
            self.armed = self.armed.saturating_sub(1);
        }
    }

    #[must_use]
    pub fn watches_armed(&self) -> u64 {
        self.armed
    }

    /// Every root this owner is responsible for, armed or not.
    #[must_use]
    pub fn tracked_roots(&self) -> Vec<RootId> {
        self.roots.iter().map(|root| root.id).collect()
    }

    /// A handle that ends an in-flight blocking [`WatchOwner::pump`].
    #[must_use]
    pub fn waker(&self) -> Option<SourceWaker> {
        self.source.waker()
    }

    /// How long before an unpaired rename half must be resolved, or `None` when none is waiting.
    ///
    /// This is not decoration. `expire_pending_moves` only ever runs at the end of a `pump`, and a
    /// lone `MovedFrom` produces no mutation at all — so nothing else in the loop knows the
    /// watcher owes itself a decision. Block without consulting this and a file moved out of the
    /// watched world stays in the index until some unrelated event happens to arrive.
    #[must_use]
    pub fn next_move_deadline(&self, now_ms: i64) -> Option<Duration> {
        let window = self.move_pairing_ms;
        self.pending_moves
            .values()
            .map(|pending| (pending.at_ms.saturating_add(window)).saturating_sub(now_ms))
            .min()
            .map(|remaining| Duration::from_millis(u64::try_from(remaining).unwrap_or(0)))
    }

    /// Drop every watch while keeping every root tracked.
    ///
    /// The disarm half of the observer lifecycle: the index still knows what it is responsible
    /// for, it simply stops being told about it — which the caller must then say out loud
    /// (`StaleReason::Unwatched`), because an unwatched root is exactly as unproven as one the
    /// daemon was not running for.
    pub fn disarm_all(&mut self) {
        for (id, dir) in std::mem::take(&mut self.dirs) {
            self.by_path.remove(&dir.path);
            self.source.remove_watch(id);
        }
        self.by_path.clear();
        self.pending_moves.clear();
        self.armed = 0;
    }

    /// The watcher's own report, for `Index::set_watch_status`.
    #[must_use]
    pub fn status(&self, pending_events: u64) -> WatchStatus {
        let limits = self.source.limits();
        WatchStatus {
            backend: self.source.backend().to_owned(),
            watches_armed: self.armed,
            watches_refused: self.refused,
            watch_limit: limits.max_user_watches,
            queue_limit: limits.max_queued_events,
            last_event_ms: self.last_event_ms,
            last_overflow_ms: self.last_overflow_ms,
            overflow_count: self.overflow_count,
            pending_events,
            last_error: self.last_error.clone(),
        }
    }

    fn root_of(&self, path: &Path) -> Option<&WatchedRoot> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            // The longest matching root wins, so a root nested inside another is not stolen.
            .max_by_key(|root| root.path.as_os_str().len())
    }

    fn arm_dir(&mut self, root: RootId, dir: &Path) -> ArmResult {
        if self.by_path.contains_key(dir) {
            return ArmResult::AlreadyArmed;
        }
        if self.armed >= self.budget.max_watches {
            self.refused = self.refused.saturating_add(1);
            return ArmResult::Refused;
        }
        guard::assert_fixture_root(dir);
        match self.source.add_watch(dir) {
            Ok(id) => {
                self.dirs.insert(
                    id,
                    WatchedDir {
                        root,
                        path: dir.to_path_buf(),
                    },
                );
                self.by_path.insert(dir.to_path_buf(), id);
                self.armed = self.armed.saturating_add(1);
                ArmResult::Armed
            }
            Err(error) => {
                self.refused = self.refused.saturating_add(1);
                self.last_error = Some(format!("{}: {error}", dir.display()));
                ArmResult::Refused
            }
        }
    }

    /// The event source, for wiring and for tests that script events.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Arm one directory. `false` means the budget or the kernel refused, which the caller must
    /// carry into its own completeness.
    pub fn arm_directory(&mut self, root: RootId, dir: &Path) -> bool {
        !matches!(self.arm_dir(root, dir), ArmResult::Refused)
    }

    /// Arm `dir` and everything under it, handing each indexable entry to `visit` **after** its
    /// directory is watched.
    ///
    /// The order is the whole point (plan of record §3.3.2): arm, then read. Reading first leaves
    /// a window in which a creation is neither in the scan nor in the event stream, and a scan
    /// that commits from that window would call itself complete while missing a file.
    pub fn arm_tree(
        &mut self,
        root: RootId,
        dir: &Path,
        visit: &mut dyn FnMut(ScannedEntry) -> Result<(), crate::IndexError>,
    ) -> Result<ArmOutcome, crate::IndexError> {
        let policy = self
            .roots
            .iter()
            .find(|tracked| tracked.id == root)
            .map(|tracked| tracked.policy.clone())
            .unwrap_or_default();
        let mut outcome = ArmOutcome::default();
        self.arm_recursive(root, dir, &policy, visit, &mut outcome)?;
        Ok(outcome)
    }

    fn arm_recursive(
        &mut self,
        root: RootId,
        dir: &Path,
        policy: &RootPolicy,
        visit: &mut dyn FnMut(ScannedEntry) -> Result<(), crate::IndexError>,
        outcome: &mut ArmOutcome,
    ) -> Result<(), crate::IndexError> {
        match self.arm_dir(root, dir) {
            ArmResult::Armed | ArmResult::AlreadyArmed => outcome.dirs_armed += 1,
            ArmResult::Refused => {
                // The budget or the kernel said no. Everything below this directory is
                // unwatched, and the index must say so rather than pretend.
                outcome.dirs_refused += 1;
                outcome.complete = false;
                return Ok(());
            }
        }

        let reader = match std::fs::read_dir(dir) {
            Ok(reader) => reader,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                outcome.permission_denied += 1;
                outcome.complete = false;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                outcome.unreadable_dirs += 1;
                outcome.complete = false;
                self.last_error = Some(format!("{}: {error}", dir.display()));
                return Ok(());
            }
        };

        let mut subdirectories = Vec::new();
        for entry in reader {
            let Ok(entry) = entry else {
                outcome.unreadable_dirs += 1;
                outcome.complete = false;
                continue;
            };
            let path = entry.path();
            let Ok(metadata) = entry.path().symlink_metadata() else {
                // Vanished between readdir and stat. The event stream will describe it.
                continue;
            };
            let meta = EntryMeta::from_symlink_metadata(&path, &metadata);
            let native = NativePath::from_path(&path);
            if !policy.should_index(&native, meta.kind) {
                continue;
            }
            visit(ScannedEntry {
                path: native,
                parent: Some(NativePath::from_path(dir)),
                meta: meta.clone(),
            })?;
            outcome.entries += 1;
            if meta.kind == EntryKind::Directory {
                subdirectories.push(path);
            }
        }

        for child in subdirectories {
            self.arm_recursive(root, &child, policy, visit, outcome)?;
        }
        Ok(())
    }

    /// Collect events and turn them into mutations and repairs.
    ///
    /// `timeout` of `None` blocks until something arrives or [`WatchOwner::waker`] is poked. The
    /// caller may only pass `None` when it has nothing of its own to do at a deadline — an
    /// unpaired rename waiting to expire is exactly such a deadline (see
    /// [`WatchOwner::next_move_deadline`]).
    pub fn pump(
        &mut self,
        timeout: Option<Duration>,
        clock: &dyn crate::coalesce::Clock,
    ) -> Pumped {
        let mut pumped = Pumped::default();
        let events = match self.source.poll(timeout) {
            Ok(events) => events,
            Err(error) => {
                let detail = error.to_string();
                self.last_error = Some(detail.clone());
                pumped.failed = Some(detail);
                return pumped;
            }
        };
        let now = clock.now_ms();
        if !events.is_empty() {
            self.last_event_ms = Some(now);
        }

        for event in events {
            self.normalize(event, now, &mut pumped);
        }
        self.expire_pending_moves(now, &mut pumped);
        pumped
    }

    #[allow(clippy::too_many_lines)] // one dispatch over every raw kernel event kind, kept together
    fn normalize(&mut self, event: RawEvent, now: i64, pumped: &mut Pumped) {
        if event.kind == RawKind::Overflow {
            // The kernel dropped an unknown number of events. Nothing after this point can be
            // trusted to describe a complete history, so every watched root is marked stale and
            // scheduled for repair. This is the single most important branch in the module: the
            // alternative — carrying on and letting the next batch advance the generation — is
            // exactly the silent lie the acceptance gates forbid.
            self.overflow_count = self.overflow_count.saturating_add(1);
            self.last_overflow_ms = Some(now);
            self.pending_moves.clear();
            for root in &self.roots {
                pumped.repairs.push(RepairRequest {
                    root: root.id,
                    scope: ScanScope::Root,
                    reason: StaleReason::WatcherOverflow,
                    mark_root_stale: true,
                });
                pumped.mutations.push(Mutation::Invalidate {
                    root: root.id,
                    subtree: NativePath::from_path(&root.path),
                    reason: StaleReason::WatcherOverflow,
                });
            }
            return;
        }

        let Some(watched) = self.dirs.get(&event.watch).cloned() else {
            return;
        };

        match event.kind {
            RawKind::Unmounted => {
                pumped.offline.push(watched.root);
                self.forget_root(watched.root);
                return;
            }
            RawKind::SelfGone => {
                // The watched directory itself is gone. If it was a root, the root is offline;
                // otherwise its parent's `Deleted` event carries the removal.
                if self.roots.iter().any(|root| root.path == watched.path) {
                    pumped.offline.push(watched.root);
                }
                self.drop_watch(event.watch);
                return;
            }
            RawKind::WatchDropped => {
                self.drop_watch(event.watch);
                return;
            }
            _ => {}
        }

        let Some(name) = event.name else { return };
        let path = watched.path.join(bytes_to_path(&name));
        let native = NativePath::from_path(&path);
        let Some(policy) = self
            .roots
            .iter()
            .find(|root| root.id == watched.root)
            .map(|root| root.policy.clone())
        else {
            return;
        };

        match event.kind {
            RawKind::Created | RawKind::Written | RawKind::Attributes => {
                let Some(meta) = stat(&path) else {
                    // Gone again already; the delete event is on its way.
                    return;
                };
                if !policy.should_index(&native, meta.kind) {
                    return;
                }
                if event.kind == RawKind::Created && meta.kind == EntryKind::Directory {
                    self.absorb_new_directory(watched.root, &path, &policy, pumped);
                }
                let mutation = if event.kind == RawKind::Attributes {
                    Mutation::Touch {
                        root: watched.root,
                        path: native,
                        meta,
                    }
                } else {
                    Mutation::Upsert {
                        root: watched.root,
                        path: native,
                        meta,
                    }
                };
                pumped.mutations.push(mutation);
            }
            RawKind::Deleted => {
                if let Some(id) = self.by_path.get(&path).copied() {
                    self.drop_watch(id);
                }
                pumped.mutations.push(Mutation::Remove {
                    root: watched.root,
                    path: native,
                });
            }
            RawKind::MovedFrom => {
                self.pending_moves.insert(
                    event.cookie,
                    PendingMove {
                        root: watched.root,
                        from: path,
                        is_dir: event.is_dir,
                        at_ms: now,
                    },
                );
            }
            RawKind::MovedTo => {
                if let Some(pending) = self.pending_moves.remove(&event.cookie) {
                    self.complete_move(&pending, &path, pumped);
                } else {
                    // The other half never arrived, or arrived from outside every watched root:
                    // whatever is here now is new to the index.
                    let Some(meta) = stat(&path) else { return };
                    if !policy.should_index(&native, meta.kind) {
                        return;
                    }
                    if meta.kind == EntryKind::Directory {
                        // A directory arrived whole. Its contents were never watched and never
                        // scanned, so this is the one genuinely ambiguous case: repair it rather
                        // than guess at its contents.
                        self.absorb_new_directory(watched.root, &path, &policy, pumped);
                    }
                    pumped.mutations.push(Mutation::Upsert {
                        root: watched.root,
                        path: native,
                        meta,
                    });
                }
            }
            RawKind::Overflow | RawKind::Unmounted | RawKind::SelfGone | RawKind::WatchDropped => {
                unreachable!("handled above")
            }
        }
    }

    /// A directory appeared. Arm it and enumerate it if it is small; otherwise ask for a repair.
    fn absorb_new_directory(
        &mut self,
        root: RootId,
        path: &Path,
        policy: &RootPolicy,
        pumped: &mut Pumped,
    ) {
        let mut entries = Vec::new();
        let mut outcome = ArmOutcome::default();
        let mut over_budget = false;
        let inline_subtree_limit = self.inline_subtree_limit;
        let mut visit = |entry: ScannedEntry| -> Result<(), crate::IndexError> {
            if entries.len() as u64 >= inline_subtree_limit {
                over_budget = true;
                return Err(crate::IndexError::Cancelled);
            }
            entries.push(entry);
            Ok(())
        };
        let armed = self.arm_recursive(root, path, policy, &mut visit, &mut outcome);

        if over_budget || armed.is_err() || !outcome.complete {
            // Too big to absorb between two events, or partly unreadable. The subtree is real but
            // unknown: schedule it, and do not pretend the mutations we did collect are all of it.
            pumped.repairs.push(RepairRequest {
                root,
                scope: ScanScope::Subtree(NativePath::from_path(path)),
                reason: StaleReason::RenameAmbiguous,
                mark_root_stale: false,
            });
            return;
        }
        for entry in entries {
            pumped.mutations.push(Mutation::Upsert {
                root,
                path: entry.path,
                meta: entry.meta,
            });
        }
    }

    fn complete_move(&mut self, pending: &PendingMove, to: &Path, pumped: &mut Pumped) {
        let Some(meta) = stat(to) else { return };
        let same_root = self.root_of(to).is_some_and(|root| root.id == pending.root);
        if !same_root {
            // Out of this root's world: a removal here, and whoever owns the destination will see
            // its own create.
            pumped.mutations.push(Mutation::Remove {
                root: pending.root,
                path: NativePath::from_path(&pending.from),
            });
            return;
        }
        pumped.mutations.push(Mutation::Move {
            root: pending.root,
            from: NativePath::from_path(&pending.from),
            to: NativePath::from_path(to),
            meta,
        });
        if pending.is_dir {
            // The index re-paths descendant rows itself (`Applied::moved_descendants`), so a
            // folder drag costs one mutation rather than a subtree re-enumeration. What it cannot
            // fix is our own watch table: the kernel's watches follow the inodes and stay valid,
            // but they are filed under names that no longer exist.
            self.retarget_subtree(&pending.from, to);
        }
    }

    /// Rewrite the watch table after a directory rename. The kernel keeps the watches — they
    /// follow the inode — but our idea of where they point is now wrong.
    fn retarget_subtree(&mut self, from: &Path, to: &Path) {
        let moved: Vec<(WatchId, PathBuf)> = self
            .dirs
            .iter()
            .filter(|(_, dir)| dir.path.starts_with(from))
            .map(|(id, dir)| (*id, dir.path.clone()))
            .collect();
        for (id, old) in moved {
            let Ok(relative) = old.strip_prefix(from) else {
                continue;
            };
            let new = to.join(relative);
            self.by_path.remove(&old);
            self.by_path.insert(new.clone(), id);
            if let Some(dir) = self.dirs.get_mut(&id) {
                dir.path = new;
            }
        }
    }

    fn drop_watch(&mut self, id: WatchId) {
        if let Some(dir) = self.dirs.remove(&id) {
            self.by_path.remove(&dir.path);
            self.armed = self.armed.saturating_sub(1);
        }
        self.source.remove_watch(id);
    }

    /// A rename half whose partner never came. After the pairing window it can only mean the
    /// entry left the watched world.
    fn expire_pending_moves(&mut self, now: i64, pumped: &mut Pumped) {
        let window = self.move_pairing_ms;
        let expired: Vec<u32> = self
            .pending_moves
            .iter()
            .filter(|(_, pending)| now.saturating_sub(pending.at_ms) >= window)
            .map(|(cookie, _)| *cookie)
            .collect();
        for cookie in expired {
            let Some(pending) = self.pending_moves.remove(&cookie) else {
                continue;
            };
            if let Some(id) = self.by_path.get(&pending.from).copied() {
                self.drop_watch(id);
            }
            pumped.mutations.push(Mutation::Remove {
                root: pending.root,
                path: NativePath::from_path(&pending.from),
            });
        }
    }
}

enum ArmResult {
    Armed,
    AlreadyArmed,
    Refused,
}

/// What one arming walk saw.
#[derive(Clone, Copy, Debug)]
pub struct ArmOutcome {
    pub dirs_armed: u64,
    pub dirs_refused: u64,
    pub entries: u64,
    pub permission_denied: u64,
    pub unreadable_dirs: u64,
    /// False as soon as anything was refused or unreadable. A scan commit reads this: an
    /// incomplete walk must never garbage-collect (C5 §3).
    pub complete: bool,
}

impl Default for ArmOutcome {
    fn default() -> Self {
        Self {
            dirs_armed: 0,
            dirs_refused: 0,
            entries: 0,
            permission_denied: 0,
            unreadable_dirs: 0,
            complete: true,
        }
    }
}

fn stat(path: &Path) -> Option<EntryMeta> {
    path.symlink_metadata()
        .ok()
        .map(|metadata| EntryMeta::from_symlink_metadata(path, &metadata))
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

// ---------------------------------------------------------------------------------------------
// The inotify backend.
// ---------------------------------------------------------------------------------------------

/// The production event source.
///
/// The kernel fd is drained by a dedicated thread doing a blocking read; the owner thread takes
/// messages from a channel, which is what gives `poll` a timeout — and an interruptible unbounded
/// wait — without a `poll(2)` call, and keeps the crate free of `unsafe`.
///
/// Accepted debt: that reader thread is detached and is never reaped. Ending it means closing the
/// inotify fd from another thread while it sits inside `read(2)`, and a descriptor closed under a
/// blocking reader can be reused by an unrelated `open` before the reader notices — the classic
/// fd-reuse race, and a far worse bug than a thread that outlives its owner. The thread exits on
/// its own the moment the kernel returns an error, and the process is short of the whole point of
/// this daemon by then anyway.
#[derive(Debug)]
pub struct InotifySource {
    watches: inotify::Watches,
    events: std::sync::mpsc::Receiver<SourceMessage>,
    /// A retained sender, so that [`SourceWaker`] can end a blocking `recv`. It also means the
    /// channel never disconnects on its own, which is why the reader announces its own exit
    /// rather than leaving `recv` to infer it.
    wake: std::sync::mpsc::Sender<SourceMessage>,
    descriptors: HashMap<WatchId, inotify::WatchDescriptor>,
    /// The kernel names watches by descriptor; everything above this module names them by
    /// `WatchId`, so that the fake source can exist at all.
    ids: HashMap<inotify::WatchDescriptor, WatchId>,
    next_id: u64,
    limits: WatchLimits,
    /// Latched: a source that has lost its reader can never produce another event, and must not
    /// be polled into a spin.
    reader_stopped: bool,
}

/// What travels the reader thread's channel.
#[derive(Debug)]
enum SourceMessage {
    Event(Box<KernelEvent>),
    /// Somebody wants an unbounded `poll` to return. Carries no meaning of its own.
    Wake,
    /// The reader thread is gone. Sent from a drop guard, so a panic announces itself too.
    ReaderStopped,
}

/// One event as it left the reader thread, still named by the kernel's descriptor.
#[derive(Debug)]
struct KernelEvent {
    watch: Option<inotify::WatchDescriptor>,
    name: Option<Vec<u8>>,
    kind: RawKind,
    cookie: u32,
    is_dir: bool,
}

/// Announces the reader thread's exit however it happens, including by unwind.
struct ReaderExit(std::sync::mpsc::Sender<SourceMessage>);

impl Drop for ReaderExit {
    fn drop(&mut self) {
        let _ = self.0.send(SourceMessage::ReaderStopped);
    }
}

impl InotifySource {
    /// Open one inotify instance. One per process: `fs.inotify.max_user_instances` is 128 on the
    /// reference host and every other application wants some too.
    pub fn open() -> std::io::Result<Self> {
        let inotify = inotify::Inotify::init()?;
        let watches = inotify.watches();
        let (sender, events) = std::sync::mpsc::channel();
        let wake = sender.clone();
        std::thread::Builder::new()
            .name("lege-index-inotify".to_owned())
            .spawn(move || {
                let _exit = ReaderExit(sender.clone());
                let mut inotify = inotify;
                let mut buffer = [0u8; 16 * 1024];
                loop {
                    let Ok(batch) = inotify.read_events_blocking(&mut buffer) else {
                        break;
                    };
                    for event in batch {
                        if sender
                            .send(SourceMessage::Event(Box::new(convert(&event))))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            watches,
            events,
            wake,
            descriptors: HashMap::new(),
            ids: HashMap::new(),
            next_id: 1,
            limits: WatchLimits::read(),
            reader_stopped: false,
        })
    }

    /// Fold one channel message into this poll's results. Returns false once the stream is over.
    fn absorb(message: SourceMessage, kernel: &mut Vec<KernelEvent>) -> bool {
        match message {
            SourceMessage::Event(event) => {
                kernel.push(*event);
                true
            }
            SourceMessage::Wake => true,
            SourceMessage::ReaderStopped => false,
        }
    }
}

/// `CLOSE_WRITE` rather than `MODIFY`: one event when a writer is done instead of one per write.
/// A file held open forever therefore shows its old size until it is closed — the same trade
/// every practical indexer makes, and the reconciler is what corrects it.
fn watch_mask() -> inotify::WatchMask {
    use inotify::WatchMask as M;
    M::CREATE
        | M::DELETE
        | M::DELETE_SELF
        | M::MOVED_FROM
        | M::MOVED_TO
        | M::MOVE_SELF
        | M::CLOSE_WRITE
        | M::ATTRIB
        | M::EXCL_UNLINK
        | M::ONLYDIR
}

fn convert(event: &inotify::Event<&std::ffi::OsStr>) -> KernelEvent {
    use inotify::EventMask as E;
    use std::os::unix::ffi::OsStrExt;
    let mask = event.mask;
    let kind = if mask.contains(E::Q_OVERFLOW) {
        RawKind::Overflow
    } else if mask.contains(E::UNMOUNT) {
        RawKind::Unmounted
    } else if mask.contains(E::IGNORED) {
        RawKind::WatchDropped
    } else if mask.contains(E::DELETE_SELF) || mask.contains(E::MOVE_SELF) {
        RawKind::SelfGone
    } else if mask.contains(E::CREATE) {
        RawKind::Created
    } else if mask.contains(E::DELETE) {
        RawKind::Deleted
    } else if mask.contains(E::MOVED_FROM) {
        RawKind::MovedFrom
    } else if mask.contains(E::MOVED_TO) {
        RawKind::MovedTo
    } else if mask.contains(E::CLOSE_WRITE) {
        RawKind::Written
    } else {
        RawKind::Attributes
    };
    KernelEvent {
        // An overflow carries no valid descriptor, so it has no watch at all.
        watch: (kind != RawKind::Overflow).then(|| event.wd.clone()),
        name: event.name.map(|name| name.as_bytes().to_vec()),
        kind,
        cookie: event.cookie,
        is_dir: mask.contains(E::ISDIR),
    }
}

impl EventSource for InotifySource {
    fn backend(&self) -> &'static str {
        "inotify"
    }

    fn limits(&self) -> WatchLimits {
        self.limits
    }

    fn add_watch(&mut self, dir: &Path) -> std::io::Result<WatchId> {
        let descriptor = self.watches.add(dir, watch_mask())?;
        // Re-adding an existing watch returns the same descriptor rather than a second watch,
        // which is also how the kernel accounts for it.
        if let Some(id) = self.ids.get(&descriptor) {
            return Ok(*id);
        }
        let id = WatchId(self.next_id);
        self.next_id += 1;
        self.ids.insert(descriptor.clone(), id);
        self.descriptors.insert(id, descriptor);
        Ok(id)
    }

    fn remove_watch(&mut self, watch: WatchId) {
        if let Some(descriptor) = self.descriptors.remove(&watch) {
            self.ids.remove(&descriptor);
            // A watch the kernel has already dropped (the directory went away) fails here, and
            // that is the expected path, not an error worth reporting.
            let _ = self.watches.remove(descriptor);
        }
    }

    fn waker(&self) -> Option<SourceWaker> {
        let wake = self.wake.clone();
        Some(SourceWaker::new(std::sync::Arc::new(move || {
            let _ = wake.send(SourceMessage::Wake);
        })))
    }

    fn poll(&mut self, timeout: Option<Duration>) -> std::io::Result<Vec<RawEvent>> {
        if self.reader_stopped {
            return Err(std::io::Error::other("the inotify reader thread stopped"));
        }
        let mut kernel = Vec::new();
        let mut alive = match timeout {
            Some(timeout) => match self.events.recv_timeout(timeout) {
                Ok(message) => Self::absorb(message, &mut kernel),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
            },
            // Unbounded: the thread parks until the kernel, or a `SourceWaker`, has something to
            // say. This is the zero-idle-cost path.
            None => match self.events.recv() {
                Ok(message) => Self::absorb(message, &mut kernel),
                Err(std::sync::mpsc::RecvError) => false,
            },
        };
        while let Ok(message) = self.events.try_recv() {
            alive &= Self::absorb(message, &mut kernel);
        }
        if !alive {
            self.reader_stopped = true;
            return Err(std::io::Error::other("the inotify reader thread stopped"));
        }
        Ok(kernel
            .into_iter()
            .map(|event| RawEvent {
                watch: event
                    .watch
                    .as_ref()
                    .and_then(|descriptor| self.ids.get(descriptor).copied())
                    .unwrap_or(WatchId(0)),
                name: event.name,
                kind: event.kind,
                cookie: event.cookie,
                is_dir: event.is_dir,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted event source.
// ---------------------------------------------------------------------------------------------

/// An event source a test writes by hand, including the events a kernel only produces under load.
///
/// Public, not `#[cfg(test)]`: an overflow that can only be reproduced by filling a real kernel
/// queue is an overflow nobody tests.
#[derive(Debug, Default)]
pub struct FakeSource {
    queued: std::collections::VecDeque<RawEvent>,
    watches: HashMap<WatchId, PathBuf>,
    next_id: u64,
    /// Fail every `add_watch` past this many, the way the kernel does at `max_user_watches`.
    pub watch_ceiling: Option<u64>,
    pub limits: WatchLimits,
    /// Make the next `poll` fail, the way a source whose reader thread died does. The one failure
    /// a test cannot stage by any other means, and the one that used to turn the loop into a spin.
    pub fail_with: Option<String>,
}

impl FakeSource {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    /// The watch id armed for `dir`, so a test can address events at it.
    #[must_use]
    pub fn watch_for(&self, dir: &Path) -> Option<WatchId> {
        self.watches
            .iter()
            .find(|(_, path)| path.as_path() == dir)
            .map(|(id, _)| *id)
    }

    pub fn push(&mut self, event: RawEvent) {
        self.queued.push_back(event);
    }

    /// The event the kernel sends when it has dropped an unknown number of others.
    pub fn push_overflow(&mut self) {
        self.queued.push_back(RawEvent {
            watch: WatchId(0),
            name: None,
            kind: RawKind::Overflow,
            cookie: 0,
            is_dir: false,
        });
    }

    pub fn push_named(&mut self, watch: WatchId, name: &str, kind: RawKind, is_dir: bool) {
        self.queued.push_back(RawEvent {
            watch,
            name: Some(name.as_bytes().to_vec()),
            kind,
            cookie: 0,
            is_dir,
        });
    }
}

impl EventSource for FakeSource {
    fn backend(&self) -> &'static str {
        "fake"
    }

    fn limits(&self) -> WatchLimits {
        self.limits
    }

    fn add_watch(&mut self, dir: &Path) -> std::io::Result<WatchId> {
        if self
            .watch_ceiling
            .is_some_and(|ceiling| self.watches.len() as u64 >= ceiling)
        {
            return Err(std::io::Error::from_raw_os_error(28)); // ENOSPC, as the kernel returns.
        }
        let id = WatchId(self.next_id);
        self.next_id += 1;
        self.watches.insert(id, dir.to_path_buf());
        Ok(id)
    }

    fn remove_watch(&mut self, watch: WatchId) {
        self.watches.remove(&watch);
    }

    /// Never blocks, whatever the timeout says — including `None`. A scripted source has nothing
    /// to wait for, and a test that could hang on one would be worse than useless. It therefore
    /// offers no [`EventSource::waker`] either: there is never a wait to interrupt.
    fn poll(&mut self, _timeout: Option<Duration>) -> std::io::Result<Vec<RawEvent>> {
        if let Some(error) = self.fail_with.take() {
            return Err(std::io::Error::other(error));
        }
        Ok(self.queued.drain(..).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unbounded wait is only safe because something other than the kernel can end it.
    ///
    /// This is the one test that has to use the real backend: `FakeSource` never blocks, so it
    /// cannot demonstrate that a blocking `poll(None)` is interruptible. Opening an inotify
    /// instance arms nothing and touches no path, so the fixture guard has nothing to object to.
    /// The wait is observed from another thread with a bounded `recv_timeout`, so a waker that
    /// does not work fails the test rather than hanging the suite.
    #[test]
    fn a_waker_ends_an_unbounded_poll() {
        let mut source = InotifySource::open().expect("an inotify instance");
        let waker = source
            .waker()
            .expect("the production source must be interruptible");

        let (done, finished) = std::sync::mpsc::channel();
        let polling = std::thread::spawn(move || {
            let result = source.poll(None);
            let _ = done.send(result.map(|events| events.len()));
        });

        assert!(
            matches!(
                finished.recv_timeout(Duration::from_millis(200)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "poll(None) must actually block when nothing has happened"
        );

        waker.wake();
        let woken = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("a wake must end the wait");
        assert_eq!(
            woken.expect("a wake is not a failure"),
            0,
            "a wake carries no event of its own"
        );
        polling.join().expect("the polling thread");
    }

    /// A source whose reader is gone reports it once and keeps reporting it, rather than
    /// returning nothing instantly forever — which, under an unbounded wait, is a spin.
    #[test]
    fn a_scripted_failure_is_latched_into_the_pump() {
        let mut source = FakeSource::new();
        source.fail_with = Some("the reader thread stopped".to_owned());
        let mut owner = WatchOwner::new(source, WatchBudget { max_watches: 8 });
        let clock = crate::coalesce::FakeClock::new(0);

        let pumped = owner.pump(None, &clock);
        assert_eq!(
            pumped.failed.as_deref(),
            Some("the reader thread stopped"),
            "a dead source is a lost stream, not a quiet one"
        );
    }

    /// An unpaired rename half is a deadline nothing else in the loop knows about: it produces no
    /// mutation, so the coalescer stays empty and would happily let the caller block forever.
    #[test]
    fn a_half_seen_rename_is_a_deadline() {
        let temp = tempfile::tempdir().expect("a temporary directory");
        let tree = temp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();

        let root = RootId(1);
        let mut owner = WatchOwner::new(FakeSource::new(), WatchBudget { max_watches: 8 });
        owner.track_root(root, &tree, RootPolicy::default_home());
        assert!(owner.arm_directory(root, &tree));
        let watch = owner.source_mut().watch_for(&tree).expect("an armed watch");

        let clock = crate::coalesce::FakeClock::new(0);
        assert_eq!(owner.next_move_deadline(0), None, "nothing is pending");

        owner
            .source_mut()
            .push_named(watch, "gone.txt", RawKind::MovedFrom, false);
        let pumped = owner.pump(None, &clock);
        assert!(
            pumped.mutations.is_empty(),
            "a lone MOVED_FROM says nothing yet — which is exactly why it needs a deadline"
        );
        assert_eq!(
            owner.next_move_deadline(0),
            Some(Duration::from_millis(250)),
            "the pairing window is the deadline"
        );

        // Past the window, the pump resolves it into the removal it turned out to be.
        clock.advance(250);
        let pumped = owner.pump(None, &clock);
        assert_eq!(pumped.mutations.len(), 1);
        assert_eq!(owner.next_move_deadline(250), None);
    }
}
