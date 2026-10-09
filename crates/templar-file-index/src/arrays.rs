//! In-memory search arrays: the index's live rows as flat columns in path order, scanned in
//! parallel per query.
//!
//! SQLite answers a search by visiting every matching row through a b-tree, about a microsecond
//! each, so a broad needle over two million entries costs seconds whatever the planner does. These
//! arrays answer the same question from contiguous memory instead:
//!
//! - **Matching is a scan.** Folded names sit in one byte blob; every name is tested in parallel
//!   chunks, so the cost is a pass over a few tens of megabytes, not a b-tree descent per hit.
//! - **Path matches need no paths.** Rows are stored in byte order of their paths, in which every
//!   path under a directory `d` is contiguous (the range `[d/, d0)`), though not necessarily
//!   right after `d` itself: `d-x` and `d.rs` sort between `d` and `d/`. Each directory keeps
//!   that range, and a directory whose name matches makes it a path-only match.
//! - **Ordering needs no sort.** The permutations for every [`Sort`] are built once with the
//!   arrays; a query walks the one it needs and keeps the first rows that match. Ties break on the
//!   position, which is the path, exactly as the SQL order does.
//!
//! They cache committed root generations, never an authority. A query uses them only when every
//! participating root matches in its SQLite view; otherwise the search goes
//! through SQL and asks for a rebuild in the background. A build is demand-driven — only a search
//! starts one — so an index nobody searches costs nothing.

use crate::{
    ArrayColumn, ArrayStats, EntryId, IndexError, Result, RootArrayStats,
    context::CallContext,
    model::{Coverage, EntryKind, Generation, NativePath, RootId},
    query::{Query, Rank, Sort},
};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

/// No match at a position.
const NONE: u8 = u8::MAX;
/// Path-only: an ancestor directory's name matches.
const PATH_ONLY: u8 = 3;
/// Below this many rows a query is matched on the calling thread.
const PARALLEL_FROM: usize = 32_768;
/// A directory whose first descendant has not been seen yet.
const UNSET: u32 = u32::MAX;

/// One root's immutable live columns in native path order. A generation check in the query's
/// SQLite view makes these a cache, never an authority.
#[derive(Debug)]
pub(crate) struct BaseImage {
    pub root: RootId,
    pub root_generation: Generation,
    pub built_change_seq: u64,
    path: Vec<u8>,
    root_row: bool,
    topology: Vec<(RootId, Vec<u8>)>,
    /// Lower bounds for other root rows and their descendant intervals in this shard. A root
    /// row and its descendants can straddle `root-old`; two cuts preserve native path order
    /// without retaining every path.
    cuts: std::collections::HashMap<RootId, (u32, u32)>,
    /// Old container rows can overlap a newly registered root until its complete repair.
    /// Such a pair uses SQL until ownership is disjoint; a cut alone cannot order it.
    overlaps: std::collections::HashSet<RootId>,
    ids: Ids,
    names: Box<[u8]>,
    name_end: Box<[u32]>,
    subtree_start: Box<[u32]>,
    subtree_end: Box<[u32]>,
    kind: Box<[u8]>,
    flags: Box<[u8]>,
    size: Box<[u64]>,
    mtime: Box<[i64]>,
    dirs_by_id: Directories,
    by_mtime_desc: Box<[u32]>,
    by_name: Box<[u32]>,
    name_rank: Box<[u32]>,
    by_size: Box<[u32]>,
    reservation: Reservation,
    last_query: Mutex<Option<Instant>>,
    pub(crate) built_at: Instant,
    pub(crate) build_peak_bytes: u64,
}

/// A published root view shares its immutable base with older readers. Every replacement
/// overlay is also immutable and carries the exact committed root generation it describes.
#[derive(Debug)]
pub(crate) struct RootShard {
    pub(crate) base: Arc<BaseImage>,
    pub(crate) overlay: Arc<crate::overlay::Overlay>,
}
impl std::ops::Deref for RootShard {
    type Target = BaseImage;
    fn deref(&self) -> &BaseImage {
        &self.base
    }
}
impl RootShard {
    fn build(
        connection: &Connection,
        root: RootId,
        cap: usize,
        retained: Arc<std::sync::atomic::AtomicUsize>,
        cx: &CallContext,
    ) -> Result<Self> {
        let base = Arc::new(BaseImage::build(connection, root, cap, retained, cx)?);
        let overlay = Arc::new(crate::overlay::Overlay::empty(
            base.root_generation,
            base.built_change_seq,
        ));
        Ok(Self { base, overlay })
    }
    pub(crate) fn generation(&self) -> Generation {
        self.overlay.generation
    }
    pub(crate) fn bound(&self) -> usize {
        10_000usize.max(self.len() / 20)
    }
    pub(crate) fn base_ids(&self) -> std::collections::HashSet<EntryId> {
        (0..self.len()).map(|p| self.ids.at(p)).collect()
    }
    pub(crate) fn base_directory(&self, id: EntryId) -> bool {
        self.dirs_by_id.find(id).is_some()
    }
}

#[derive(Debug)]
enum Ids {
    Narrow { base: i64, offsets: Box<[u32]> },
    Wide(Box<[EntryId]>),
}
impl Ids {
    fn len(&self) -> usize {
        match self {
            Self::Narrow { offsets, .. } => offsets.len(),
            Self::Wide(ids) => ids.len(),
        }
    }
    fn at(&self, position: usize) -> EntryId {
        match self {
            Self::Narrow { base, offsets } => EntryId(*base + i64::from(offsets[position])),
            Self::Wide(ids) => ids[position],
        }
    }
    fn bytes(&self) -> usize {
        self.len()
            * match self {
                Self::Narrow { .. } => 4,
                Self::Wide(_) => 8,
            }
    }
}
type DirectorySlices<'a> = (&'a [(u32, u32)], &'a [(EntryId, u32)]);

#[derive(Debug)]
enum Directories {
    Narrow {
        base: i64,
        values: Box<[(u32, u32)]>,
    },
    Wide(Box<[(EntryId, u32)]>),
}
impl Directories {
    fn find(&self, id: EntryId) -> Option<u32> {
        match self {
            Self::Narrow { base, values } => {
                let offset = u32::try_from(id.0.checked_sub(*base)?).ok()?;
                values
                    .binary_search_by_key(&offset, |(id, _)| *id)
                    .ok()
                    .map(|i| values[i].1)
            }
            Self::Wide(values) => values
                .binary_search_by_key(&id, |(id, _)| *id)
                .ok()
                .map(|i| values[i].1),
        }
    }
    fn positions(&self) -> impl Iterator<Item = u32> + '_ {
        let (narrow, wide): DirectorySlices<'_> = match self {
            Self::Narrow { values, .. } => (values, &[]),
            Self::Wide(values) => (&[], values),
        };
        narrow
            .iter()
            .map(|(_, p)| *p)
            .chain(wide.iter().map(|(_, p)| *p))
    }
    fn bytes(&self) -> usize {
        match self {
            Self::Narrow { values, .. } => values.len() * 8,
            Self::Wide(values) => values.len() * std::mem::size_of::<(EntryId, u32)>(),
        }
    }
}

/// Includes retired shards while a query still holds their Arc. Reservations happen before
/// allocating columns, so simultaneous synchronous and maintenance builds share one cap.
#[derive(Debug)]
pub(crate) struct Reservation {
    bytes: usize,
    retained: Arc<std::sync::atomic::AtomicUsize>,
}
impl Reservation {
    pub(crate) fn resize(&mut self, bytes: usize, cap: usize) -> Result<()> {
        self.retained
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_sub(self.bytes)
                    .and_then(|rest| rest.checked_add(bytes))
                    .filter(|sum| *sum <= cap)
            })
            .map_err(|held| IndexError::TooLarge {
                entries: 0,
                bytes: held.saturating_sub(self.bytes).saturating_add(bytes) as u64,
            })?;
        self.bytes = bytes;
        Ok(())
    }
    pub(crate) fn reserve(
        bytes: usize,
        retained: Arc<std::sync::atomic::AtomicUsize>,
        cap: usize,
    ) -> Result<Self> {
        retained
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|sum| *sum <= cap)
            })
            .map_err(|held| IndexError::TooLarge {
                entries: 0,
                bytes: held.saturating_add(bytes) as u64,
            })?;
        Ok(Self { bytes, retained })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.retained.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
/// What a query against the arrays produced: the best `keep` positions' ids and ranks in order,
/// how many rows matched in all, and how many of those are directories.
pub(crate) struct ArraysAnswer {
    pub ids: Vec<EntryId>,
    pub ranks: Vec<Rank>,
    pub total: u64,
    pub folders: u64,
}

struct ShardAnswer {
    ids: Vec<EntryId>,
    positions: Vec<u32>,
    ranks: Vec<Rank>,
    total: u64,
    folders: u64,
}

thread_local! {
    /// One match's per-row ranks and path-only sweep, reused by every query on this thread:
    /// cleared, never freed, so a burst of typing does not allocate a row-sized buffer per key.
    static SCRATCH: std::cell::RefCell<(Vec<u8>, Vec<i32>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
}

/// Rows matched between cancellation checks.
const CANCEL_STRIDE: usize = 65_536;

/// Root-local demand-built accelerators. Unrelated commits do not invalidate a shard.
#[derive(Debug, Default)]
pub(crate) struct ShardSet {
    shards: RwLock<std::collections::HashMap<RootId, Arc<RootShard>>>,
    building: Mutex<std::collections::HashSet<RootId>>,
    last_build: Mutex<std::collections::HashMap<RootId, (Instant, Duration)>>,
    refused: Mutex<std::collections::HashMap<RootId, String>>,
    retained: Arc<std::sync::atomic::AtomicUsize>,
    pressure_until: Mutex<Option<Instant>>,
    updating: Mutex<std::collections::HashMap<RootId, bool>>,
    dirty: Mutex<std::collections::HashSet<RootId>>,
    counters: Mutex<QueryCounters>,
    over_budget: Mutex<std::collections::HashSet<RootId>>,
}
#[derive(Debug, Default)]
struct QueryCounters {
    hits: u64,
    misses: std::collections::BTreeMap<String, u64>,
    roots: std::collections::BTreeMap<RootId, (u64, std::collections::BTreeMap<String, u64>)>,
}
const MIN_REBUILD_INTERVAL: Duration = Duration::from_secs(5);

impl ShardSet {
    pub(crate) fn cached_roots(&self) -> Vec<RootId> {
        self.shards.read().keys().copied().collect()
    }
    pub(crate) fn needs_base(&self, root: RootId) -> bool {
        let shards = self.shards.read();
        let Some(shard) = shards.get(&root) else {
            return true;
        };
        self.dirty.lock().contains(&root)
            || (shard.overlay.row_count() > shard.bound() / 4
                && shard.built_at.elapsed() >= MIN_REBUILD_INTERVAL)
    }
    pub(crate) fn begin_update(&self, root: RootId) -> Option<Arc<RootShard>> {
        if self.dirty.lock().contains(&root)
            || self
                .pressure_until
                .lock()
                .is_some_and(|until| Instant::now() < until)
        {
            return None;
        }
        let shard = self.shards.read().get(&root).cloned()?;
        let mut updating = self.updating.lock();
        if let Some(pending) = updating.get_mut(&root) {
            *pending = true;
            return None;
        }
        updating.insert(root, false);
        Some(shard)
    }
    pub(crate) fn abandon_update(&self, root: RootId) {
        self.updating.lock().remove(&root);
    }
    pub(crate) fn refresh_overlay(
        &self,
        connection: &Connection,
        shard: &RootShard,
        cap: usize,
        cx: &CallContext,
    ) -> Result<crate::overlay::Overlay> {
        crate::overlay::Overlay::refresh(connection, shard, Arc::clone(&self.retained), cap, cx)
    }
    pub(crate) fn finish_update(
        &self,
        root: RootId,
        old: &RootShard,
        result: Result<crate::overlay::Overlay>,
    ) -> bool {
        let mut shards = self.shards.write();
        if let Some(current) = shards.get(&root)
            && Arc::ptr_eq(&current.base, &old.base)
        {
            match result {
                Ok(overlay) if overlay.generation >= current.generation() => {
                    shards.insert(
                        root,
                        Arc::new(RootShard {
                            base: Arc::clone(&old.base),
                            overlay: Arc::new(overlay),
                        }),
                    );
                    self.refused.lock().remove(&root);
                    self.over_budget.lock().remove(&root);
                }
                Err(error) => {
                    if matches!(error, IndexError::TooLarge { .. }) {
                        self.over_budget.lock().insert(root);
                    }
                    self.refused.lock().insert(root, error.to_string());
                    if matches!(
                        error,
                        IndexError::SnapshotRequired { .. } | IndexError::TooLarge { .. }
                    ) {
                        self.dirty.lock().insert(root);
                    }
                }
                _ => {}
            }
        }
        drop(shards);
        self.updating.lock().remove(&root).unwrap_or(false)
    }
    pub(crate) fn stats(&self) -> Option<ArrayStats> {
        let shards = self.shards.read();
        if shards.is_empty()
            && self.refused.lock().is_empty()
            && self.counters.lock().roots.is_empty()
        {
            return None;
        }
        let last_build = self.last_build.lock().clone();
        let refused = self.refused.lock().clone();
        let mut columns: std::collections::BTreeMap<String, ArrayColumn> =
            std::collections::BTreeMap::new();
        let mut generation = Generation(0);
        let mut rows = 0;
        let mut roots = Vec::new();
        for shard in shards.values() {
            let stats = shard.stats();
            generation = generation.max(shard.generation());
            rows += stats.rows;
            roots.push(RootArrayStats {
                root: shard.root,
                generation: shard.generation(),
                built_change_seq: shard.built_change_seq,
                rows: stats.rows,
                bytes: stats.bytes,
                last_build_ms: last_build
                    .get(&shard.root)
                    .map(|(_, time)| time.as_millis() as u64),
                refusal: refused.get(&shard.root).cloned(),
                overlay_rows: shard.overlay.row_count() as u64,
                overlay_bytes: shard.overlay.bytes() as u64,
                overlay_change_seq: shard.overlay.change_seq,
                build_peak_bytes: shard.build_peak_bytes,
                ..RootArrayStats::default()
            });
            for column in stats.columns {
                let target = columns.entry(column.name.clone()).or_insert(ArrayColumn {
                    name: column.name,
                    bytes: 0,
                    capacity_bytes: 0,
                });
                target.bytes += column.bytes;
                target.capacity_bytes += column.capacity_bytes;
            }
        }
        for (root, reason) in &refused {
            if !shards.contains_key(root) {
                roots.push(RootArrayStats {
                    root: *root,
                    generation: Generation(0),
                    built_change_seq: 0,
                    rows: 0,
                    bytes: 0,
                    last_build_ms: self
                        .last_build
                        .lock()
                        .get(root)
                        .map(|(_, t)| t.as_millis() as u64),
                    refusal: Some(reason.clone()),
                    overlay_rows: 0,
                    overlay_bytes: 0,
                    overlay_change_seq: 0,
                    ..RootArrayStats::default()
                });
            }
        }
        let counters = self.counters.lock();
        for (id, (hits, misses)) in &counters.roots {
            if !roots.iter().any(|r| r.root == *id) {
                roots.push(RootArrayStats {
                    root: *id,
                    ..RootArrayStats::default()
                });
            }
            let root = roots.iter_mut().find(|r| r.root == *id).unwrap();
            root.hits = *hits;
            root.misses = misses.clone();
        }
        for root in &mut roots {
            root.building = self.building.lock().contains(&root.root);
            root.dirty = self.dirty.lock().contains(&root.root);
        }
        roots.sort_by_key(|root| root.root);
        let columns: Vec<_> = columns.into_values().collect();
        Some(ArrayStats {
            generation,
            rows,
            bytes: columns.iter().map(|c| c.bytes).sum(),
            capacity_bytes: columns.iter().map(|c| c.capacity_bytes).sum(),
            columns,
            roots,
            retained_bytes: self.retained.load(Ordering::Acquire) as u64,
            hits: counters.hits,
            misses: counters.misses.clone(),
        })
    }

    pub(crate) fn note_query(&self, roots: &[RootId], reason: Option<&str>) {
        let mut counters = self.counters.lock();
        if let Some(reason) = reason {
            *counters.misses.entry(reason.into()).or_default() += 1;
        } else {
            counters.hits += 1;
        }
        for id in roots {
            let (hits, misses) = counters.roots.entry(*id).or_default();
            if let Some(reason) = reason {
                *misses.entry(reason.into()).or_default() += 1;
            } else {
                *hits += 1;
            }
        }
    }
    pub(crate) fn note_invalid_query(
        &self,
        roots: &[RootId],
        invalid: &[RootId],
        global_reason: &str,
    ) {
        let reasons: Vec<_> = roots
            .iter()
            .map(|root| {
                (
                    *root,
                    if invalid.contains(root) {
                        self.miss_reason(*root)
                    } else {
                        "mixed"
                    },
                )
            })
            .collect();
        let mut counters = self.counters.lock();
        *counters.misses.entry(global_reason.into()).or_default() += 1;
        for (root, reason) in reasons {
            *counters
                .roots
                .entry(root)
                .or_default()
                .1
                .entry(reason.into())
                .or_default() += 1;
        }
    }
    pub(crate) fn miss_reason(&self, root: RootId) -> &'static str {
        if self.over_budget.lock().contains(&root) {
            "over-budget"
        } else if self.dirty.lock().contains(&root) {
            "dirty"
        } else if self.building.lock().contains(&root) {
            "building"
        } else {
            "stale"
        }
    }
    pub(crate) fn diagnostic_stats(&self, roots: &[crate::RootStatus]) -> ArrayStats {
        let mut stats = self.stats().unwrap_or_default();
        let shards = self.shards.read();
        let topology: Vec<_> = roots
            .iter()
            .map(|r| (r.root, r.path.as_bytes().to_vec()))
            .collect();
        stats
            .roots
            .retain(|r| roots.iter().any(|root| root.root == r.root));
        for root in roots {
            if !stats.roots.iter().any(|s| s.root == root.root) {
                stats.roots.push(RootArrayStats {
                    root: root.root,
                    ..RootArrayStats::default()
                });
            }
            let item = stats
                .roots
                .iter_mut()
                .find(|s| s.root == root.root)
                .unwrap();
            item.valid = item.generation == root.generation
                && shards.get(&root.root).is_some_and(|s| {
                    s.generation() == item.generation && s.topology == topology && !item.dirty
                });
        }
        stats.roots.sort_by_key(|r| r.root);
        stats
    }

    pub(crate) fn current_for(
        &self,
        wanted: &[(RootId, Generation)],
        topology: &[(RootId, Vec<u8>)],
    ) -> (Vec<Arc<RootShard>>, Vec<RootId>) {
        let mut guard = self.shards.write();
        guard.retain(|root, _| topology.iter().any(|(id, _)| id == root));
        self.counters
            .lock()
            .roots
            .retain(|root, _| topology.iter().any(|(id, _)| id == root));
        let mut valid = Vec::new();
        let mut invalid = Vec::new();
        for (root, generation) in wanted {
            match guard.get(root) {
                Some(shard)
                    if shard.generation() == *generation
                        && shard.topology == topology
                        && !self.dirty.lock().contains(root) =>
                {
                    *shard.last_query.lock() = Some(Instant::now());
                    valid.push(Arc::clone(shard));
                }
                _ => invalid.push(*root),
            }
        }
        (valid, invalid)
    }

    /// Evict least recently queried unused shards first. Readers keep their exact view via Arc.
    pub(crate) fn evict_unused(&self) -> (usize, usize) {
        *self.pressure_until.lock() = Some(Instant::now() + Duration::from_secs(10));
        let mut shards = self.shards.write();
        let mut candidates: Vec<_> = shards
            .iter()
            .filter(|(_, shard)| Arc::strong_count(shard) == 1)
            .map(|(root, shard)| (*root, *shard.last_query.lock(), shard.reservation.bytes))
            .collect();
        candidates.sort_by_key(|(_, queried, _)| *queried);
        let mut bytes = 0;
        for (root, _, held) in &candidates {
            shards.remove(root);
            bytes += held;
            self.refused.lock().insert(
                *root,
                "evicted for memory pressure; rebuild paused for ten seconds".into(),
            );
        }
        (candidates.len(), bytes)
    }

    pub(crate) fn begin_build(&self, root: RootId) -> bool {
        if self
            .pressure_until
            .lock()
            .is_some_and(|until| Instant::now() < until)
        {
            return false;
        }
        let mut building = self.building.lock();
        if building.contains(&root) {
            return false;
        }
        let mut last = self.last_build.lock();
        if let Some((started, took)) = last.get(&root)
            && started.elapsed() < MIN_REBUILD_INTERVAL.max(*took * 2)
        {
            return false;
        }
        building.insert(root);
        last.insert(root, (Instant::now(), Duration::ZERO));
        true
    }
    pub(crate) fn finish_build(&self, root: RootId, result: Result<RootShard>) {
        match result {
            Ok(shard) => {
                self.shards.write().insert(root, Arc::new(shard));
                self.refused.lock().remove(&root);
                self.over_budget.lock().remove(&root);
                self.dirty.lock().remove(&root);
            }
            Err(error) => {
                if matches!(error, IndexError::TooLarge { .. }) {
                    self.over_budget.lock().insert(root);
                }
                self.refused.lock().insert(root, error.to_string());
            }
        }
        if let Some((started, took)) = self.last_build.lock().get_mut(&root) {
            *took = started.elapsed();
        }
        self.building.lock().remove(&root);
    }
    pub(crate) fn abandoned(&self, root: RootId) {
        self.refused
            .lock()
            .insert(root, "maintenance job refused or cancelled".into());
        self.building.lock().remove(&root);
    }
    pub(crate) fn build(
        &self,
        connection: &Connection,
        root: RootId,
        cap: usize,
        cx: &CallContext,
    ) -> Result<RootShard> {
        if self
            .pressure_until
            .lock()
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(IndexError::TooLarge {
                entries: 0,
                bytes: self.retained.load(Ordering::Acquire) as u64,
            });
        }
        let result = RootShard::build(connection, root, cap, Arc::clone(&self.retained), cx);
        if let Err(error) = &result {
            if matches!(error, IndexError::TooLarge { .. }) {
                self.over_budget.lock().insert(root);
            }
            self.refused.lock().insert(root, error.to_string());
        }
        result
    }
    pub(crate) fn install(&self, shard: RootShard) {
        let root = shard.root;
        self.shards.write().insert(root, Arc::new(shard));
        self.refused.lock().remove(&root);
        self.over_budget.lock().remove(&root);
        self.dirty.lock().remove(&root);
    }
}

impl BaseImage {
    #[allow(
        clippy::too_many_lines,
        reason = "The bounded column build keeps one SQLite view and one memory reservation through publication."
    )]
    fn build(
        connection: &Connection,
        root: RootId,
        cap: usize,
        retained: Arc<std::sync::atomic::AtomicUsize>,
        cx: &CallContext,
    ) -> Result<Self> {
        let view = connection.unchecked_transaction()?;
        let (path, root_generation): (Vec<u8>, i64) = view.query_row(
            "SELECT path_bytes, committed_generation FROM roots WHERE root_id=?1",
            [root.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let built_change_seq = crate::mutation::read_latest_change_seq(&view)?;
        let mut statement =
            view.prepare("SELECT root_id, path_bytes FROM roots ORDER BY root_id")?;
        let topology = statement
            .query_map([], |row| {
                Ok((RootId(row.get(0)?), row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        // SQLite length(TEXT) counts characters: cast to BLOB to reserve folded UTF-8 bytes.
        let (count, name_bytes, raw_bytes, dirs, minimum, maximum): (
            usize,
            usize,
            usize,
            usize,
            i64,
            i64,
        ) = view.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(CAST(name_fold AS BLOB))),0),
                    COALESCE(SUM(length(name_bytes)),0), COALESCE(SUM(kind=1),0),
                    COALESCE(MIN(entry_id),0), COALESCE(MAX(entry_id),0)
             FROM entries WHERE root_id=?1 AND deleted_generation IS NULL",
            [root.0],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, i64>(3)? as usize,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let narrow = maximum
            .checked_sub(minimum)
            .is_some_and(|range| range <= i64::from(u32::MAX));
        if count >= u32::MAX as usize
            || name_bytes > u32::MAX as usize
            || raw_bytes > u32::MAX as usize
        {
            return Err(IndexError::TooLarge {
                entries: count as u64,
                bytes: name_bytes as u64,
            });
        }
        let bytes = count
            .checked_mul(if narrow { 50 } else { 54 })
            .and_then(|bytes| bytes.checked_add(name_bytes))
            .and_then(|bytes| {
                bytes.checked_add(
                    dirs * if narrow {
                        8
                    } else {
                        std::mem::size_of::<(EntryId, u32)>()
                    },
                )
            })
            .ok_or(IndexError::TooLarge {
                entries: count as u64,
                bytes: u64::MAX,
            })?;
        retained
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|sum| *sum <= cap)
            })
            .map_err(|held| IndexError::TooLarge {
                entries: count as u64,
                bytes: held.saturating_add(bytes) as u64,
            })?;
        let reservation = Reservation { bytes, retained };
        let mut ids32 = Vec::with_capacity(if narrow { count } else { 0 });
        let mut ids64 = Vec::with_capacity(if narrow { 0 } else { count });
        let mut names = Vec::with_capacity(name_bytes);
        let mut name_end = Vec::with_capacity(count);
        let mut raw_name_bytes = Vec::with_capacity(raw_bytes);
        let mut raw_name_end = Vec::with_capacity(count);
        let mut subtree_start = Vec::with_capacity(count);
        let mut subtree_end = Vec::with_capacity(count);
        let mut kind = Vec::with_capacity(count);
        let mut flags = Vec::with_capacity(count);
        let mut size = Vec::with_capacity(count);
        let mut mtime = Vec::with_capacity(count);
        let mut dirs32 = Vec::with_capacity(if narrow { dirs } else { 0 });
        let mut dirs64 = Vec::with_capacity(if narrow { 0 } else { dirs });
        let boundaries: Vec<_> = topology
            .iter()
            .filter(|(id, _)| *id != root)
            .map(|(id, path)| {
                let mut low = path.clone();
                if low != b"/" {
                    low.push(b'/');
                }
                (*id, path.as_slice(), low)
            })
            .collect();
        let mut cuts = topology
            .iter()
            .filter(|(id, _)| *id != root)
            .map(|(id, _)| (*id, (0u32, 0u32)))
            .collect::<std::collections::HashMap<_, _>>();
        let mut overlaps = std::collections::HashSet::new();
        let mut root_row = false;
        let mut open: Vec<(u32, Vec<u8>, Vec<u8>)> = Vec::new();
        let mut statement = view.prepare(
            "SELECT entry_id, name_bytes, name_fold, path_bytes, kind, size_bytes, mtime_ns, hidden, coverage
             FROM entries WHERE root_id=?1 AND deleted_generation IS NULL ORDER BY path_bytes")?;
        let mut rows = statement.query([root.0])?;
        while let Some(row) = rows.next()? {
            let position = kind.len() as u32;
            if (position as usize).is_multiple_of(CANCEL_STRIDE) {
                cx.check()?;
            }
            let row_path = row_blob(row, 3)?;
            root_row |= row_path == path;
            while let Some((directory, _, past)) = open.last() {
                if row_path < past.as_slice() {
                    break;
                }
                close_subtree(&mut subtree_start, &mut subtree_end, *directory, position);
                open.pop();
            }
            if let Some((directory, directory_path, _)) = open.last()
                && subtree_start[*directory as usize] == UNSET
                && is_descendant(row_path, directory_path)
            {
                subtree_start[*directory as usize] = position;
            }
            for (id, other, low) in &boundaries {
                if row_path < *other {
                    cuts.get_mut(id).expect("known root").0 += 1;
                }
                if row_path < low.as_slice() {
                    cuts.get_mut(id).expect("known root").1 += 1;
                }
                if is_descendant(other, &path)
                    && (row_path == *other || is_descendant(row_path, other))
                {
                    overlaps.insert(*id);
                }
            }
            let id: i64 = row.get(0)?;
            if narrow {
                ids32.push((id - minimum) as u32);
            } else {
                ids64.push(EntryId(id));
            }
            raw_name_bytes.extend_from_slice(row_blob(row, 1)?);
            raw_name_end.push(raw_name_bytes.len() as u32);
            names.extend_from_slice(row_text(row, 2)?.as_bytes());
            name_end.push(names.len() as u32);
            subtree_start.push(position + 1);
            subtree_end.push(position + 1);
            let row_kind: u8 = row.get(4)?;
            kind.push(row_kind);
            flags.push(u8::from(row.get::<_, i64>(7)? != 0) | ((row.get::<_, u8>(8)? & 3) << 1));
            size.push(row.get::<_, i64>(5)? as u64);
            mtime.push(row.get::<_, i64>(6)?);
            if row_kind == EntryKind::Directory.as_i64() as u8 {
                if narrow {
                    dirs32.push(((id - minimum) as u32, position));
                } else {
                    dirs64.push((EntryId(id), position));
                }
                subtree_start[position as usize] = UNSET;
                open.push((position, row_path.to_vec(), subtree_past(row_path)));
            }
        }
        let end = kind.len() as u32;
        for (directory, _, _) in open {
            close_subtree(&mut subtree_start, &mut subtree_end, directory, end);
        }
        drop(rows);
        drop(statement);
        drop(view);
        dirs32.sort_unstable();
        dirs64.sort_unstable();
        let mut by_mtime_desc: Vec<u32> = (0..end).collect();
        by_mtime_desc
            .sort_unstable_by(|a, b| mtime[*b as usize].cmp(&mtime[*a as usize]).then(a.cmp(b)));
        let mut by_size: Vec<u32> = (0..end).collect();
        by_size.sort_unstable_by(|a, b| size[*a as usize].cmp(&size[*b as usize]).then(a.cmp(b)));
        let raw_name = |position: u32| -> &[u8] {
            let p = position as usize;
            let start = if p == 0 {
                0
            } else {
                raw_name_end[p - 1] as usize
            };
            &raw_name_bytes[start..raw_name_end[p] as usize]
        };
        let mut by_name: Vec<u32> = (0..end).collect();
        by_name.sort_unstable_by(|a, b| raw_name(*a).cmp(raw_name(*b)).then(a.cmp(b)));
        let mut name_rank = vec![0u32; count];
        let mut rank = 0u32;
        for (i, position) in by_name.iter().enumerate() {
            if i > 0 && raw_name(by_name[i - 1]) != raw_name(*position) {
                rank += 1;
            }
            name_rank[*position as usize] = rank;
        }
        let shard = Self {
            root,
            root_generation: Generation(root_generation as u64),
            built_change_seq,
            path,
            root_row,
            topology,
            cuts,
            overlaps,
            ids: if narrow {
                Ids::Narrow {
                    base: minimum,
                    offsets: ids32.into_boxed_slice(),
                }
            } else {
                Ids::Wide(ids64.into_boxed_slice())
            },
            names: names.into_boxed_slice(),
            name_end: name_end.into_boxed_slice(),
            subtree_start: subtree_start.into_boxed_slice(),
            subtree_end: subtree_end.into_boxed_slice(),
            kind: kind.into_boxed_slice(),
            flags: flags.into_boxed_slice(),
            size: size.into_boxed_slice(),
            mtime: mtime.into_boxed_slice(),
            dirs_by_id: if narrow {
                Directories::Narrow {
                    base: minimum,
                    values: dirs32.into_boxed_slice(),
                }
            } else {
                Directories::Wide(dirs64.into_boxed_slice())
            },
            by_mtime_desc: by_mtime_desc.into_boxed_slice(),
            by_name: by_name.into_boxed_slice(),
            name_rank: name_rank.into_boxed_slice(),
            by_size: by_size.into_boxed_slice(),
            reservation,
            last_query: Mutex::new(None),
            built_at: Instant::now(),
            build_peak_bytes: bytes
                .saturating_add(raw_bytes)
                .saturating_add(count.saturating_mul(4)) as u64,
        };
        debug_assert_eq!(shard.stats().bytes as usize, bytes);
        cx.check()?;
        Ok(shard)
    }
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }
    fn stats(&self) -> ArrayStats {
        fn column<T>(name: &str, values: &[T]) -> ArrayColumn {
            let bytes = std::mem::size_of_val(values) as u64;
            ArrayColumn {
                name: name.into(),
                bytes,
                capacity_bytes: bytes,
            }
        }
        let columns = vec![
            ArrayColumn {
                name: "ids".into(),
                bytes: self.ids.bytes() as u64,
                capacity_bytes: self.ids.bytes() as u64,
            },
            column("names", &self.names),
            column("name_end", &self.name_end),
            column("subtree_start", &self.subtree_start),
            column("subtree_end", &self.subtree_end),
            column("kind", &self.kind),
            column("flags", &self.flags),
            column("size", &self.size),
            column("mtime", &self.mtime),
            ArrayColumn {
                name: "dirs_by_id".into(),
                bytes: self.dirs_by_id.bytes() as u64,
                capacity_bytes: self.dirs_by_id.bytes() as u64,
            },
            column("by_mtime_desc", &self.by_mtime_desc),
            column("by_name", &self.by_name),
            column("name_rank", &self.name_rank),
            column("by_size", &self.by_size),
        ];
        ArrayStats {
            generation: self.root_generation,
            rows: self.len() as u64,
            bytes: columns.iter().map(|c| c.bytes).sum(),
            capacity_bytes: columns.iter().map(|c| c.capacity_bytes).sum(),
            columns,
            roots: Vec::new(),
            retained_bytes: self.reservation.bytes as u64,
            ..ArrayStats::default()
        }
    }

    fn name(&self, position: usize) -> &[u8] {
        let start = if position == 0 {
            0
        } else {
            self.name_end[position - 1] as usize
        };
        &self.names[start..self.name_end[position] as usize]
    }

    /// Answers `query` for the folded `needle`, or `Ok(None)` when it cannot: a needle with a
    /// separator (a whole-path match these arrays do not hold) or a `within` directory they do
    /// not know. The caller then asks SQL, which can. Matching uses at most `threads` threads and
    /// stops with `Cancelled` when `cx` is cancelled.
    #[allow(
        clippy::too_many_arguments,
        reason = "The shard search carries one validated query view, caller limits and overlay exclusions together."
    )]
    fn search(
        &self,
        query: &Query,
        needle: &str,
        within_ids: &[Option<EntryId>],
        keep: usize,
        threads: usize,
        cx: &CallContext,
        ancestor_match: bool,
        keep_positions: bool,
        removed: &std::collections::HashSet<EntryId>,
    ) -> Result<Option<ShardAnswer>> {
        if needle.contains('/') {
            return Ok(None);
        }
        let Some(scope) = self.scope(query, within_ids) else {
            return Ok(None);
        };
        SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            let (ranks, delta) = &mut *scratch;
            self.match_ranks(needle.as_bytes(), threads, cx, ranks, delta)?;
            if ancestor_match {
                for rank in ranks.iter_mut() {
                    if *rank == NONE {
                        *rank = PATH_ONLY;
                    }
                }
            }
            cx.check()?;
            Ok(Some(self.collect(
                query,
                &scope,
                ranks,
                keep,
                keep_positions,
                removed,
            )))
        })
    }

    /// Walks the order `query` asks for over the ranked rows and keeps the best `keep`.
    fn collect(
        &self,
        query: &Query,
        scope: &[(u32, u32)],
        ranks: &[u8],
        keep: usize,
        keep_positions: bool,
        removed: &std::collections::HashSet<EntryId>,
    ) -> ShardAnswer {
        let filter = Filter::of(query, self);
        let directory = EntryKind::Directory.as_i64() as u8;
        let folders = std::cell::Cell::new(0u64);
        let accept = |position: u32| -> Option<u8> {
            let rank = ranks[position as usize];
            let accepted = rank != NONE
                && in_scope(scope, position)
                && filter.passes(self, position as usize)
                && (removed.is_empty() || !removed.contains(&self.ids.at(position as usize)));
            if accepted && self.kind[position as usize] == directory {
                folders.set(folders.get() + 1);
            }
            accepted.then_some(rank)
        };

        let total = std::cell::Cell::new(0u64);
        let mut positions = Vec::new();
        let mut ids = Vec::new();
        let mut out_ranks = Vec::new();
        let mut push = |position: u32, rank: u8| {
            total.set(total.get() + 1);
            if out_ranks.len() < keep {
                if keep_positions {
                    positions.push(position);
                } else {
                    ids.push(self.ids.at(position as usize));
                }
                out_ranks.push(rank_of(rank));
            }
        };

        match (query.sort, query.descending) {
            (Sort::Relevance, _) => {
                // Rank first, then the permutation's own order: mtime descending, then path.
                let mut buckets: [Vec<u32>; 4] = Default::default();
                let mut counts = [0u64; 4];
                for position in &self.by_mtime_desc {
                    if let Some(rank) = accept(*position) {
                        let bucket = rank as usize;
                        counts[bucket] += 1;
                        if buckets[bucket].len() < keep {
                            buckets[bucket].push(*position);
                        }
                    }
                }
                for (rank, bucket) in buckets.iter().enumerate() {
                    for position in bucket {
                        push(*position, rank as u8);
                    }
                    // Rows counted but not kept still count towards the total.
                    total.set(total.get() + counts[rank] - bucket.len() as u64);
                }
            }
            (Sort::Path, false) => {
                for position in 0..self.len() as u32 {
                    if let Some(rank) = accept(position) {
                        push(position, rank);
                    }
                }
            }
            (Sort::Path, true) => {
                for position in (0..self.len() as u32).rev() {
                    if let Some(rank) = accept(position) {
                        push(position, rank);
                    }
                }
            }
            (Sort::Name, false) => walk(&self.by_name, &accept, &mut push),
            (Sort::Name, true) => walk_descending(
                &self.by_name,
                |position| self.name_rank[position as usize],
                &accept,
                &mut push,
            ),
            (Sort::Size, false) => walk(&self.by_size, &accept, &mut push),
            (Sort::Size, true) => walk_descending(
                &self.by_size,
                |position| self.size[position as usize],
                &accept,
                &mut push,
            ),
            (Sort::Mtime, true) => walk(&self.by_mtime_desc, &accept, &mut push),
            (Sort::Mtime, false) => walk_descending(
                &self.by_mtime_desc,
                |position| self.mtime[position as usize],
                &accept,
                &mut push,
            ),
        }
        ShardAnswer {
            ids,
            positions,
            ranks: out_ranks,
            total: total.get(),
            folders: folders.get(),
        }
    }

    /// The position ranges a query may return from: everything, or the union of its `within`
    /// directories' subtrees, as a sorted list of disjoint ranges. Root restriction is a per-row
    /// test in [`Filter`], because a root nested inside another interleaves with it in path
    /// order. `within_ids` is aligned with `query.within` and holds each directory's entry id
    /// where it has one. `None` when a `within` directory is neither a root nor a directory these
    /// arrays hold.
    fn scope(&self, query: &Query, within_ids: &[Option<EntryId>]) -> Option<Vec<(u32, u32)>> {
        if query.within.is_empty() {
            return Some(vec![(0, self.len() as u32)]);
        }
        let mut within = Vec::with_capacity(query.within.len());
        for (directory, id) in query.within.iter().zip(within_ids) {
            within.push(self.subtree_of(directory, *id)?);
        }
        Some(normalize(within))
    }

    /// The descendants of `directory` (not the directory itself) as a position range.
    fn subtree_of(&self, directory: &NativePath, id: Option<EntryId>) -> Option<(u32, u32)> {
        let directory = directory.as_bytes();
        if directory == self.path || is_descendant(&self.path, directory) {
            // Exclude the root row itself only when the scope is exactly that root.
            let start = u32::from(directory == self.path && self.root_row);
            return Some((start, self.len() as u32));
        }
        if !is_descendant(directory, &self.path) {
            return Some((0, 0));
        }
        let position = self.dirs_by_id.find(id?)?;
        Some((
            self.subtree_start[position as usize],
            self.subtree_end[position as usize],
        ))
    }

    /// Ranks every position into `ranks`: `0..=2` for a name match, [`PATH_ONLY`] for a row under
    /// a matching directory, [`NONE`] otherwise. Uses at most `threads` scoped threads; `ranks` and
    /// `delta` are the caller's reusable scratch, resized and cleared here, never shrunk.
    fn match_ranks(
        &self,
        needle: &[u8],
        threads: usize,
        cx: &CallContext,
        ranks: &mut Vec<u8>,
        delta: &mut Vec<i32>,
    ) -> Result<()> {
        let count = self.len();
        ranks.clear();
        ranks.resize(count, NONE);
        let threads = threads.max(1);
        if count < PARALLEL_FROM || threads == 1 {
            self.match_names(needle, 0, ranks, cx)?;
        } else {
            let chunk = count.div_ceil(threads);
            std::thread::scope(|scope| -> Result<()> {
                let workers: Vec<_> = ranks
                    .chunks_mut(chunk)
                    .enumerate()
                    .map(|(index, slice)| {
                        scope.spawn(move || self.match_names(needle, index * chunk, slice, cx))
                    })
                    .collect();
                for worker in workers {
                    worker.join().map_err(|_| IndexError::Unsupported {
                        detail: "a search-array match thread panicked".to_owned(),
                    })??;
                }
                Ok(())
            })?;
        }
        cx.check()?;
        // Path-only: every row in the subtree range of a directory whose name matches. Ranges
        // are marked as +1/-1 at their ends and swept once, so nested matches cost nothing extra.
        let directory = EntryKind::Directory.as_i64() as u8;
        delta.clear();
        for position in self.dirs_by_id.positions() {
            let position = position as usize;
            if ranks[position] < PATH_ONLY && self.kind[position] == directory {
                let (start, end) = (self.subtree_start[position], self.subtree_end[position]);
                if start < end {
                    if delta.is_empty() {
                        delta.resize(count + 1, 0);
                    }
                    delta[start as usize] += 1;
                    delta[end as usize] -= 1;
                }
            }
        }
        if !delta.is_empty() {
            let mut depth = 0i32;
            for (rank, change) in ranks.iter_mut().zip(delta.iter()) {
                depth += change;
                if depth > 0 && *rank == NONE {
                    *rank = PATH_ONLY;
                }
            }
        }
        Ok(())
    }

    fn match_names(
        &self,
        needle: &[u8],
        offset: usize,
        ranks: &mut [u8],
        cx: &CallContext,
    ) -> Result<()> {
        for (stride, slots) in ranks.chunks_mut(CANCEL_STRIDE).enumerate() {
            cx.check()?;
            let base = offset + stride * CANCEL_STRIDE;
            for (index, slot) in slots.iter_mut().enumerate() {
                let name = self.name(base + index);
                *slot = if name == needle {
                    0
                } else if name.starts_with(needle) {
                    1
                } else if contains(name, needle) {
                    2
                } else {
                    NONE
                };
            }
        }
        Ok(())
    }
}

/// Exact cross-root merge. No SQL/shard result mixing: SQL supplies only bounded sort-key
/// lookups and ancestor truth in the same read view that validated every participating shard.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "The exact cross-root merge retains one validated read view through result publication."
)]
pub(crate) fn search_shards(
    connection: &Connection,
    shards: &[Arc<RootShard>],
    query: &Query,
    needle: &str,
    within_ids: &[Option<EntryId>],
    keep: usize,
    threads: usize,
    cx: &CallContext,
) -> Result<Option<ArraysAnswer>> {
    if needle.contains('/') {
        return Ok(None);
    }
    if shards.iter().any(|shard| !shard.overlay.upserts.is_empty()) {
        let matches = overlay_matches(connection, shards, query, needle, cx)?;
        if query.sort == Sort::Path
            && shards.iter().any(|shard| {
                shard
                    .overlay
                    .upserts
                    .iter()
                    .any(|row| overlay_in_scope(query, &row.path))
            })
        {
            return Ok(None);
        }
        if !matches.is_empty() {
            return merge_overlay(
                connection, shards, query, needle, within_ids, keep, threads, cx, matches,
            );
        }
    }
    // A newly added root can coexist with old container rows until the next complete pass.
    if shards
        .iter()
        .any(|a| shards.iter().any(|b| a.overlaps.contains(&b.root)))
    {
        return Ok(None);
    }
    let mut ancestor = connection.prepare_cached(
        "SELECT name_fold FROM entries WHERE root_id=?1 AND path_bytes=?2
         AND kind=1 AND deleted_generation IS NULL",
    )?;
    let mut answers = Vec::with_capacity(shards.len());
    for shard in shards {
        cx.check()?;
        let mut ancestor_match = false;
        for (root, path) in &shard.topology {
            if *root == shard.root || !is_descendant(&shard.path, path) {
                continue;
            }
            let mut parent = NativePath::from_bytes(shard.path.clone()).parent();
            while let Some(directory) = parent {
                if directory.as_bytes() != path && !is_descendant(directory.as_bytes(), path) {
                    break;
                }
                let mut rows = ancestor.query(rusqlite::params![root.0, directory.as_bytes()])?;
                if let Some(row) = rows.next()?
                    && contains(row_text(row, 0)?.as_bytes(), needle.as_bytes())
                {
                    ancestor_match = true;
                    break;
                }
                parent = directory.parent();
            }
            if ancestor_match {
                break;
            }
        }
        let Some(answer) = shard.search(
            query,
            needle,
            within_ids,
            keep,
            threads,
            cx,
            ancestor_match,
            shards.len() != 1,
            &shard.overlay.removed,
        )?
        else {
            return Ok(None);
        };
        answers.push(answer);
    }
    let total = answers.iter().map(|a| a.total).sum();
    let folders = answers.iter().map(|a| a.folders).sum();
    if shards.len() == 1 {
        let answer = answers.pop().expect("one shard");
        return Ok(Some(ArraysAnswer {
            ids: answer.ids,
            ranks: answer.ranks,
            total,
            folders,
        }));
    }
    let mut next = vec![0usize; shards.len()];
    let mut names = vec![Vec::new(); shards.len()];
    let mut name_statement =
        connection.prepare_cached("SELECT name_bytes FROM entries WHERE entry_id=?1")?;
    let mut heap = Vec::with_capacity(shards.len());
    for slot in 0..shards.len() {
        if answers[slot].positions.is_empty() {
            continue;
        }
        if query.sort == Sort::Name {
            names[slot] = name_statement.query_row(
                [shards[slot].ids.at(answers[slot].positions[0] as usize).0],
                |row| row.get(0),
            )?;
        }
        heap_push(&mut heap, slot, &|a, b| {
            merge_cmp(shards, &answers, &next, &names, query, a, b)
        });
    }
    let mut ids = Vec::with_capacity(keep.min(total as usize));
    let mut ranks = Vec::with_capacity(ids.capacity());
    while !heap.is_empty() && ids.len() < keep {
        if ids.len() % 1024 == 0 {
            cx.check()?;
        }
        let slot = heap_pop(&mut heap, &|a, b| {
            merge_cmp(shards, &answers, &next, &names, query, a, b)
        });
        let position = answers[slot].positions[next[slot]] as usize;
        ids.push(shards[slot].ids.at(position));
        ranks.push(answers[slot].ranks[next[slot]]);
        next[slot] += 1;
        if next[slot] < answers[slot].positions.len() {
            if query.sort == Sort::Name {
                names[slot] = name_statement.query_row(
                    [shards[slot]
                        .ids
                        .at(answers[slot].positions[next[slot]] as usize)
                        .0],
                    |row| row.get(0),
                )?;
            }
            heap_push(&mut heap, slot, &|a, b| {
                merge_cmp(shards, &answers, &next, &names, query, a, b)
            });
        }
    }
    Ok(Some(ArraysAnswer {
        ids,
        ranks,
        total,
        folders,
    }))
}

fn overlay_in_scope(query: &Query, path: &[u8]) -> bool {
    query.within.is_empty()
        || query
            .within
            .iter()
            .any(|within| is_descendant(path, within.as_bytes()))
}

/// Current ancestor truth is cached only for this validating read view. Parent IDs accelerate
/// the first hop; native paths also cover foreign roots and imperfect parent metadata.
fn overlay_matches(
    connection: &Connection,
    shards: &[Arc<RootShard>],
    query: &Query,
    needle: &str,
    cx: &CallContext,
) -> Result<Vec<(usize, usize, Rank)>> {
    let mut matches = Vec::new();
    let mut ancestors: std::collections::HashMap<Vec<u8>, bool> = std::collections::HashMap::new();
    let mut by_path = connection.prepare_cached(
        "SELECT name_fold FROM entries WHERE root_id=?1 AND path_bytes=?2 AND kind=1 AND deleted_generation IS NULL")?;
    let mut by_id = connection.prepare_cached(
        "SELECT name_fold FROM entries WHERE entry_id=?1 AND path_bytes=?2 AND kind=1 AND deleted_generation IS NULL")?;
    for (slot, shard) in shards.iter().enumerate() {
        let filter = Filter::of(query, shard);
        for (index, row) in shard.overlay.upserts.iter().enumerate() {
            if index % 256 == 0 {
                cx.check()?;
            }
            if !overlay_in_scope(query, &row.path)
                || !filter.passes_values(row.kind, row.flags, row.size, row.mtime)
            {
                continue;
            }
            if row.written_generation > shard.generation() {
                return Err(IndexError::SnapshotRequired {
                    reason: "overlay identity exceeds its committed view",
                });
            }
            let rank = if row.name_fold.as_ref() == needle.as_bytes() {
                Rank::Exact
            } else if row.name_fold.starts_with(needle.as_bytes()) {
                Rank::Prefix
            } else if contains(&row.name_fold, needle.as_bytes()) {
                Rank::Substring
            } else {
                let mut parent = NativePath::from_bytes(row.path.to_vec()).parent();
                let mut first = true;
                let mut ancestor_hit = false;
                while let Some(path) = parent {
                    let relevant: Vec<_> = shard
                        .topology
                        .iter()
                        .filter(|(_, root)| {
                            path.as_bytes() == root || is_descendant(path.as_bytes(), root)
                        })
                        .collect();
                    if relevant.is_empty() {
                        break;
                    }
                    let hit = if let Some(hit) = ancestors.get(path.as_bytes()) {
                        *hit
                    } else {
                        let mut hit = false;
                        if first && let Some(id) = row.parent_id {
                            let mut rows = by_id.query(rusqlite::params![id.0, path.as_bytes()])?;
                            if let Some(row) = rows.next()? {
                                hit = contains(row_text(row, 0)?.as_bytes(), needle.as_bytes());
                            }
                        }
                        for (root, _) in relevant {
                            if hit {
                                break;
                            }
                            let mut rows =
                                by_path.query(rusqlite::params![root.0, path.as_bytes()])?;
                            if let Some(row) = rows.next()? {
                                hit = contains(row_text(row, 0)?.as_bytes(), needle.as_bytes());
                            }
                        }
                        ancestors.insert(path.as_bytes().to_vec(), hit);
                        hit
                    };
                    if hit {
                        ancestor_hit = true;
                        break;
                    }
                    first = false;
                    parent = path.parent();
                }
                if !ancestor_hit {
                    continue;
                }
                Rank::PathOnly
            };
            matches.push((slot, index, rank));
        }
    }
    Ok(matches)
}

struct MergeHead {
    id: EntryId,
    rank: Rank,
    size: u64,
    mtime: i64,
    path: Vec<u8>,
    name: Vec<u8>,
}
enum MergeRows {
    Base(usize, ShardAnswer),
    Overlay(Vec<(usize, usize, Rank)>),
}
struct MergeStream {
    next: usize,
    rows: MergeRows,
    head: Option<MergeHead>,
}
impl MergeStream {
    fn load(
        &mut self,
        statement: &mut rusqlite::Statement<'_>,
        shards: &[Arc<RootShard>],
    ) -> Result<()> {
        self.head = match &self.rows {
            MergeRows::Base(slot, answer) => {
                if let Some(position) = answer.positions.get(self.next) {
                    let shard = &shards[*slot];
                    let position = *position as usize;
                    let id = shard.ids.at(position);
                    let (path, name) = statement.query_row(
                        rusqlite::params![id.0, shard.root_generation.0 as i64],
                        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
                    )?;
                    Some(MergeHead {
                        id,
                        rank: answer.ranks[self.next],
                        size: shard.size[position],
                        mtime: shard.mtime[position],
                        path,
                        name,
                    })
                } else {
                    None
                }
            }
            MergeRows::Overlay(rows) => rows.get(self.next).map(|(slot, index, rank)| {
                let row = &shards[*slot].overlay.upserts[*index];
                MergeHead {
                    id: row.entry_id,
                    rank: *rank,
                    size: row.size,
                    mtime: row.mtime,
                    path: row.path.to_vec(),
                    name: row.name.to_vec(),
                }
            }),
        };
        Ok(())
    }
}
fn compare_values(
    query: &Query,
    a: (Rank, u64, i64, &[u8], &[u8]),
    b: (Rank, u64, i64, &[u8], &[u8]),
) -> std::cmp::Ordering {
    let primary = match query.sort {
        Sort::Relevance => a.0.score().cmp(&b.0.score()).then_with(|| b.2.cmp(&a.2)),
        Sort::Name => a.4.cmp(b.4),
        Sort::Size => a.1.cmp(&b.1),
        Sort::Mtime => a.2.cmp(&b.2),
        Sort::Path => {
            return if query.descending {
                b.3.cmp(a.3)
            } else {
                a.3.cmp(b.3)
            };
        }
    };
    (if query.descending && query.sort != Sort::Relevance {
        primary.reverse()
    } else {
        primary
    })
    .then_with(|| a.3.cmp(b.3))
}
fn stream_cmp(streams: &[MergeStream], query: &Query, a: usize, b: usize) -> std::cmp::Ordering {
    let a = streams[a].head.as_ref().expect("heap head");
    let b = streams[b].head.as_ref().expect("heap head");
    compare_values(
        query,
        (a.rank, a.size, a.mtime, &a.path, &a.name),
        (b.rank, b.size, b.mtime, &b.path, &b.name),
    )
}
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Base and overlay streams are merged under one cursor fence and one retained-result limit."
)]
fn merge_overlay(
    connection: &Connection,
    shards: &[Arc<RootShard>],
    query: &Query,
    needle: &str,
    within_ids: &[Option<EntryId>],
    keep: usize,
    threads: usize,
    cx: &CallContext,
    mut changed: Vec<(usize, usize, Rank)>,
) -> Result<Option<ArraysAnswer>> {
    if shards
        .iter()
        .any(|a| shards.iter().any(|b| a.overlaps.contains(&b.root)))
    {
        return Ok(None);
    }
    let mut ancestor = connection.prepare_cached(
        "SELECT name_fold FROM entries WHERE root_id=?1 AND path_bytes=?2 AND kind=1 AND deleted_generation IS NULL")?;
    let mut total = changed.len() as u64;
    let mut folders = changed
        .iter()
        .filter(|(slot, index, _)| {
            shards[*slot].overlay.upserts[*index].kind == EntryKind::Directory.as_i64() as u8
        })
        .count() as u64;
    let mut streams = Vec::with_capacity(shards.len() + 1);
    for (slot, shard) in shards.iter().enumerate() {
        let mut ancestor_match = false;
        for (root, path) in &shard.topology {
            if *root == shard.root || !is_descendant(&shard.path, path) {
                continue;
            }
            let mut parent = NativePath::from_bytes(shard.path.clone()).parent();
            while let Some(directory) = parent {
                if directory.as_bytes() != path && !is_descendant(directory.as_bytes(), path) {
                    break;
                }
                let mut rows = ancestor.query(rusqlite::params![root.0, directory.as_bytes()])?;
                if let Some(row) = rows.next()?
                    && contains(row_text(row, 0)?.as_bytes(), needle.as_bytes())
                {
                    ancestor_match = true;
                    break;
                }
                parent = directory.parent();
            }
            if ancestor_match {
                break;
            }
        }
        let Some(answer) = shard.search(
            query,
            needle,
            within_ids,
            keep,
            threads,
            cx,
            ancestor_match,
            true,
            &shard.overlay.removed,
        )?
        else {
            return Ok(None);
        };
        total += answer.total;
        folders += answer.folders;
        streams.push(MergeStream {
            next: 0,
            rows: MergeRows::Base(slot, answer),
            head: None,
        });
    }
    changed.sort_unstable_by(|a, b| {
        let left = &shards[a.0].overlay.upserts[a.1];
        let right = &shards[b.0].overlay.upserts[b.1];
        compare_values(
            query,
            (a.2, left.size, left.mtime, &left.path, &left.name),
            (b.2, right.size, right.mtime, &right.path, &right.name),
        )
    });
    streams.push(MergeStream {
        next: 0,
        rows: MergeRows::Overlay(changed),
        head: None,
    });
    let mut statement = connection.prepare_cached(
        "SELECT path_bytes,name_bytes FROM entries WHERE entry_id=?1 AND written_generation<=?2 AND deleted_generation IS NULL")?;
    let mut heap = Vec::with_capacity(streams.len());
    for slot in 0..streams.len() {
        streams[slot].load(&mut statement, shards)?;
        if streams[slot].head.is_some() {
            heap_push(&mut heap, slot, &|a, b| stream_cmp(&streams, query, a, b));
        }
    }
    let mut ids = Vec::with_capacity(keep.min(total as usize));
    let mut ranks = Vec::with_capacity(ids.capacity());
    while !heap.is_empty() && ids.len() < keep {
        if ids.len() % 512 == 0 {
            cx.check()?;
        }
        if heap.len() == 1
            && let MergeRows::Base(slot, answer) = &streams[heap[0]].rows
        {
            for index in streams[heap[0]].next..answer.positions.len() {
                if ids.len() == keep {
                    break;
                }
                if ids.len() % 1024 == 0 {
                    cx.check()?;
                }
                ids.push(shards[*slot].ids.at(answer.positions[index] as usize));
                ranks.push(answer.ranks[index]);
            }
            break;
        }
        let slot = heap_pop(&mut heap, &|a, b| stream_cmp(&streams, query, a, b));
        let entry = streams[slot].head.take().expect("heap head");
        ids.push(entry.id);
        ranks.push(entry.rank);
        streams[slot].next += 1;
        streams[slot].load(&mut statement, shards)?;
        if streams[slot].head.is_some() {
            heap_push(&mut heap, slot, &|a, b| stream_cmp(&streams, query, a, b));
        }
    }
    Ok(Some(ArraysAnswer {
        ids,
        ranks,
        total,
        folders,
    }))
}

fn merge_cmp(
    shards: &[Arc<RootShard>],
    answers: &[ShardAnswer],
    next: &[usize],
    names: &[Vec<u8>],
    query: &Query,
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    let pa = answers[a].positions[next[a]];
    let pb = answers[b].positions[next[b]];
    let left = &shards[a];
    let right = &shards[b];
    let path = || {
        if left.path > right.path {
            let cuts = right.cuts[&left.root];
            let cut = if left.root_row && pa == 0 {
                cuts.0
            } else {
                cuts.1
            };
            if pb < cut {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        } else {
            let cuts = left.cuts[&right.root];
            let cut = if right.root_row && pb == 0 {
                cuts.0
            } else {
                cuts.1
            };
            if pa < cut {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
    };
    let primary = match query.sort {
        Sort::Relevance => rank_score(answers[a].ranks[next[a]])
            .cmp(&rank_score(answers[b].ranks[next[b]]))
            .then_with(|| right.mtime[pb as usize].cmp(&left.mtime[pa as usize])),
        Sort::Name => names[a].cmp(&names[b]),
        Sort::Size => left.size[pa as usize].cmp(&right.size[pb as usize]),
        Sort::Mtime => left.mtime[pa as usize].cmp(&right.mtime[pb as usize]),
        Sort::Path => {
            return if query.descending {
                path().reverse()
            } else {
                path()
            };
        }
    };
    let primary = if query.descending && query.sort != Sort::Relevance {
        primary.reverse()
    } else {
        primary
    };
    primary.then_with(path)
}
fn rank_score(rank: Rank) -> u8 {
    match rank {
        Rank::Exact => 0,
        Rank::Prefix => 1,
        Rank::Substring => 2,
        Rank::PathOnly => 3,
    }
}

fn heap_push(
    heap: &mut Vec<usize>,
    value: usize,
    cmp: &impl Fn(usize, usize) -> std::cmp::Ordering,
) {
    heap.push(value);
    let mut i = heap.len() - 1;
    while i > 0 {
        let parent = (i - 1) / 2;
        if !cmp(heap[i], heap[parent]).is_lt() {
            break;
        }
        heap.swap(i, parent);
        i = parent;
    }
}
fn heap_pop(heap: &mut Vec<usize>, cmp: &impl Fn(usize, usize) -> std::cmp::Ordering) -> usize {
    let result = heap.swap_remove(0);
    let mut i = 0;
    while i * 2 + 1 < heap.len() {
        let left = i * 2 + 1;
        let right = left + 1;
        let child = if right < heap.len() && cmp(heap[right], heap[left]).is_lt() {
            right
        } else {
            left
        };
        if !cmp(heap[child], heap[i]).is_lt() {
            break;
        }
        heap.swap(i, child);
        i = child;
    }
    result
}

/// Whether `path` lies strictly under `directory`, on a component boundary.
fn is_descendant(path: &[u8], directory: &[u8]) -> bool {
    if directory == b"/" {
        return path.len() > 1 && path[0] == b'/';
    }
    path.len() > directory.len() && path.starts_with(directory) && path[directory.len()] == b'/'
}

/// The first byte string after every path under `directory`: `directory0`, `0` being the byte
/// after `/`. For `/` itself that is `0`, which every absolute path sorts below.
fn subtree_past(directory: &[u8]) -> Vec<u8> {
    if directory == b"/" {
        return b"0".to_vec();
    }
    let mut past = directory.to_vec();
    past.push(b'0');
    past
}

/// Substring test for short haystacks. Names are tens of bytes, so a first-byte filter and a
/// direct comparison beat any searcher that has to be set up per haystack.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((first, rest)) = needle.split_first() else {
        return true;
    };
    if haystack.len() < needle.len() {
        return false;
    }
    let last_start = haystack.len() - needle.len();
    haystack[..=last_start]
        .iter()
        .enumerate()
        .any(|(start, byte)| byte == first && haystack[start + 1..start + needle.len()] == *rest)
}

/// Membership in a scope produced by [`normalize`]: one binary search for the last range starting
/// at or before `position`. Correct only because the ranges are disjoint and sorted — with an
/// unmerged `[1,100)` and `[10,20)`, position 30 would be tested against `[10,20)` alone.
fn in_scope(scope: &[(u32, u32)], position: u32) -> bool {
    match scope.binary_search_by(|(start, _)| start.cmp(&position)) {
        Ok(_) => true,
        Err(0) => false,
        Err(index) => position < scope[index - 1].1,
    }
}

/// Sorts ranges and merges every pair that overlaps or touches, dropping empty ones: the
/// disjoint union [`in_scope`] needs. Nested, duplicate and adjacent `within` directories are all
/// ordinary inputs here.
fn normalize(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.retain(|(start, end)| start < end);
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    debug_assert!(merged.windows(2).all(|pair| pair[0].1 < pair[1].0));
    merged
}

pub(crate) fn row_blob<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a [u8]> {
    row.get_ref(index)?.as_blob().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(error),
        )
    })
}
pub(crate) fn row_text<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a str> {
    row.get_ref(index)?.as_str().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn close_subtree(start: &mut [u32], end: &mut [u32], directory: u32, position: u32) {
    let directory = directory as usize;
    if start[directory] == UNSET {
        start[directory] = position;
    }
    end[directory] = position;
}

fn rank_of(score: u8) -> Rank {
    match score {
        0 => Rank::Exact,
        1 => Rank::Prefix,
        2 => Rank::Substring,
        _ => Rank::PathOnly,
    }
}

fn walk(order: &[u32], accept: &impl Fn(u32) -> Option<u8>, push: &mut impl FnMut(u32, u8)) {
    for position in order {
        if let Some(rank) = accept(*position) {
            push(*position, rank);
        }
    }
}

/// `order` is ascending by a key with ties by position. Walks it descending by the key while
/// keeping ties ascending by position, which is the SQL `key DESC, path` order.
fn walk_descending<K: PartialEq>(
    order: &[u32],
    key: impl Fn(u32) -> K,
    accept: &impl Fn(u32) -> Option<u8>,
    push: &mut impl FnMut(u32, u8),
) {
    let mut end = order.len();
    while end > 0 {
        let run_key = key(order[end - 1]);
        let mut start = end - 1;
        while start > 0 && key(order[start - 1]) == run_key {
            start -= 1;
        }
        walk(&order[start..end], accept, push);
        end = start;
    }
}

/// The non-text predicates of a query, precomputed.
struct Filter {
    kinds: Option<u8>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    min_mtime: Option<i64>,
    max_mtime: Option<i64>,
    include_hidden: bool,
    coverage: u8,
}

impl Filter {
    fn of(query: &Query, _arrays: &BaseImage) -> Self {
        Self {
            kinds: query
                .kinds
                .as_ref()
                .filter(|kinds| !kinds.is_empty())
                .map(|kinds| {
                    kinds
                        .iter()
                        .fold(0u8, |mask, kind| mask | (1 << kind.as_i64()))
                }),
            min_size: query.min_size,
            max_size: query.max_size,
            min_mtime: query.min_mtime_ns,
            max_mtime: query.max_mtime_ns,
            include_hidden: query.include_hidden,
            coverage: query
                .coverage_filter()
                .iter()
                .fold(0u8, |mask, tier: &Coverage| mask | (1 << tier.as_i64())),
        }
    }

    fn passes(&self, arrays: &BaseImage, position: usize) -> bool {
        self.passes_values(
            arrays.kind[position],
            arrays.flags[position],
            arrays.size[position],
            arrays.mtime[position],
        )
    }
    fn passes_values(&self, kind: u8, flags: u8, size: u64, mtime: i64) -> bool {
        if !self.include_hidden && flags & 1 != 0 {
            return false;
        }
        if self.coverage & (1 << ((flags >> 1) & 0b11)) == 0 {
            return false;
        }
        if let Some(kinds) = self.kinds
            && kinds & (1 << kind) == 0
        {
            return false;
        }
        self.min_size.is_none_or(|min| size >= min)
            && self.max_size.is_none_or(|max| size <= max)
            && self.min_mtime.is_none_or(|min| mtime >= min)
            && self.max_mtime.is_none_or(|max| mtime <= max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Index, IndexConfig, RootPolicy, ScanOptions};

    fn fixture() -> (tempfile::TempDir, Index, RootId) {
        let directory = tempfile::tempdir().unwrap();
        let tree = directory.path().join("tree");
        std::fs::create_dir_all(tree.join("folder")).unwrap();
        std::fs::write(tree.join("folder/İ-unicode-file"), b"f").unwrap();
        let index = Index::open(
            &directory.path().join("index.sqlite3"),
            IndexConfig {
                search_arrays: true,
                ..IndexConfig::default()
            },
        )
        .unwrap();
        let root = index
            .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
            .unwrap()
            .root;
        index.scan_root(root, ScanOptions::default()).unwrap();
        (directory, index, root)
    }

    #[derive(Default)]
    struct OverlayJobs(Mutex<std::collections::VecDeque<Box<dyn FnOnce() + Send>>>);
    impl crate::MaintenanceExecutor for OverlayJobs {
        fn submit(
            &self,
            _: &'static str,
            job: Box<dyn FnOnce() + Send>,
            _: Box<dyn FnOnce() + Send>,
        ) {
            self.0.lock().push_back(job);
        }
    }
    fn touch_fixture(directory: &tempfile::TempDir, index: &Index, root: RootId, bytes: &[u8]) {
        let path = directory.path().join("tree/folder/İ-unicode-file");
        std::fs::write(&path, bytes).unwrap();
        index
            .apply(crate::MutationBatch::new(
                crate::MutationSource::Watcher,
                vec![crate::Mutation::Touch {
                    root,
                    path: NativePath::from_path(&path),
                    meta: crate::EntryMeta::from_symlink_metadata(
                        &path,
                        &std::fs::symlink_metadata(&path).unwrap(),
                    ),
                }],
            ))
            .unwrap();
    }
    #[test]
    fn overlay_views_share_the_base_and_retired_bytes_remain_accounted_until_drop() {
        let (directory, index, root) = fixture();
        index.prepare_search_arrays().unwrap();
        let jobs = Arc::new(OverlayJobs::default());
        index.set_maintenance_executor(jobs.clone());
        let before = index.arrays_slot().shards.read()[&root].clone();
        touch_fixture(&directory, &index, root, b"first");
        jobs.0.lock().pop_front().unwrap()();
        let first = index.arrays_slot().shards.read()[&root].clone();
        assert!(Arc::ptr_eq(&before.base, &first.base));
        assert!(before.overlay.upserts.is_empty());
        assert!(first.generation() > before.generation());
        assert_eq!(first.overlay.upserts[0].size, 5);
        assert_eq!(
            first.overlay.upserts[0].written_generation,
            first.generation()
        );
        assert!(first.overlay.upserts[0].parent_id.is_some());
        let held = index.search_array_stats().unwrap().retained_bytes as usize;
        let refused = index.with_read(|connection| {
            index
                .arrays_slot()
                .refresh_overlay(connection, &first, held, &CallContext::internal())
        });
        assert!(matches!(refused, Err(IndexError::TooLarge { .. })));
        touch_fixture(&directory, &index, root, b"second-change");
        jobs.0.lock().pop_front().unwrap()();
        let second = index.arrays_slot().shards.read()[&root].clone();
        assert!(Arc::ptr_eq(&first.base, &second.base));
        assert_eq!(
            first.overlay.upserts[0].size, 5,
            "published old overlay was mutated"
        );
        assert_eq!(second.overlay.upserts[0].size, 13);
        let pinned = index.search_array_stats().unwrap().retained_bytes;
        let retired = first.overlay.bytes() as u64;
        drop(first);
        assert_eq!(
            index.search_array_stats().unwrap().retained_bytes,
            pinned - retired
        );
        assert!(
            jobs.0.lock().is_empty(),
            "small overlays compacted on every mutation"
        );
    }
    #[test]
    fn dropping_a_queued_overlay_job_releases_its_claim() {
        let (directory, index, root) = fixture();
        index.prepare_search_arrays().unwrap();
        let jobs = Arc::new(OverlayJobs::default());
        index.set_maintenance_executor(jobs.clone());
        touch_fixture(&directory, &index, root, b"queued");
        assert!(index.arrays_slot().updating.lock().contains_key(&root));
        let job = jobs.0.lock().pop_front().unwrap();
        drop(job);
        assert!(!index.arrays_slot().updating.lock().contains_key(&root));
        assert_eq!(index.prepare_search_overlays().unwrap(), 1);
        assert_eq!(
            index
                .search(&Query::new("unicode"), &CallContext::internal())
                .unwrap()
                .strategy,
            "arrays+overlay"
        );
    }

    #[test]
    fn columns_have_exact_capacity_and_ids_fall_back_to_wide_without_truncation() {
        let (_directory, index, root) = fixture();
        index.prepare_search_arrays().unwrap();
        let stats = index.search_array_stats().unwrap();
        assert_eq!(stats.bytes, stats.capacity_bytes);
        assert_eq!(
            stats
                .columns
                .iter()
                .find(|c| c.name == "ids")
                .unwrap()
                .bytes,
            stats.rows * 4
        );
        index
            .with_write(|connection| {
                connection.execute(
                    "UPDATE entries SET entry_id = entry_id + 4294967296 WHERE kind=0",
                    [],
                )?;
                let generation = crate::publish_generation(connection)?;
                connection.execute(
                    "UPDATE roots SET committed_generation=?1 WHERE root_id=?2",
                    rusqlite::params![generation.0 as i64, root.0],
                )?;
                Ok(())
            })
            .unwrap();
        index.prepare_search_arrays().unwrap();
        let stats = index.search_array_stats().unwrap();
        assert_eq!(stats.bytes, stats.capacity_bytes);
        assert_eq!(
            stats
                .columns
                .iter()
                .find(|c| c.name == "ids")
                .unwrap()
                .bytes,
            stats.rows * 8
        );
        let result = index
            .search(&Query::new("unicode"), &CallContext::internal())
            .unwrap();
        assert_eq!(result.strategy, "arrays");
        assert!(result.hits[0].row.entry_id.0 > i64::from(u32::MAX));
    }

    #[test]
    fn the_cap_counts_replaced_shards_still_held_by_readers() {
        let (_directory, index, root) = fixture();
        index.prepare_search_arrays().unwrap();
        let bytes = index.search_array_stats().unwrap().bytes as usize;
        let old = index.arrays_slot().shards.read()[&root].clone();
        let build = |cap| {
            index.with_read(|connection| {
                index
                    .arrays_slot()
                    .build(connection, root, cap, &CallContext::internal())
            })
        };
        assert!(matches!(build(bytes), Err(IndexError::TooLarge { .. })));
        assert_eq!(
            index.search_array_stats().unwrap().retained_bytes,
            bytes as u64
        );
        index.arrays_slot().install(build(bytes * 2).unwrap());
        assert_eq!(
            index.search_array_stats().unwrap().retained_bytes,
            (bytes * 2) as u64
        );
        assert!(matches!(build(bytes * 2), Err(IndexError::TooLarge { .. })));
        drop(old);
        assert_eq!(
            index.search_array_stats().unwrap().retained_bytes,
            bytes as u64
        );
        assert!(build(bytes * 2).is_ok());
    }

    #[test]
    fn a_queued_build_keeps_its_claim_and_dropping_the_job_releases_it() {
        #[derive(Default)]
        struct Held(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
        impl crate::MaintenanceExecutor for Held {
            fn submit(
                &self,
                _: &'static str,
                job: Box<dyn FnOnce() + Send>,
                _: Box<dyn FnOnce() + Send>,
            ) {
                self.0.lock().push(job);
            }
        }
        let (_directory, index, root) = fixture();
        let held = Arc::new(Held::default());
        index.set_maintenance_executor(held.clone());
        let result = index
            .search(&Query::new("unicode"), &CallContext::internal())
            .unwrap();
        assert_eq!(result.total, Some(1));
        let claimed_while_queued = index.arrays_slot().building.lock().contains(&root);
        let job = held.0.lock().pop().expect("one queued build");
        drop(job);
        assert!(claimed_while_queued, "claim escaped the queued closure");
        assert!(!index.arrays_slot().building.lock().contains(&root));
        assert!(
            index.search_array_stats().unwrap().roots[0]
                .refusal
                .is_some()
        );
    }

    #[test]
    fn memory_pressure_keeps_active_shards_and_pauses_rebuilding() {
        let (_directory, index, root) = fixture();
        index.prepare_search_arrays().unwrap();
        let active = index.arrays_slot().shards.read()[&root].clone();
        assert_eq!(index.evict_unused_search_arrays().0, 0);
        assert!(index.search_array_stats().unwrap().bytes > 0);
        drop(active);
        let (roots, bytes) = index.evict_unused_search_arrays();
        assert_eq!(roots, 1);
        assert!(bytes > 0);
        assert_eq!(index.search_array_stats().unwrap().retained_bytes, 0);
        assert!(!index.arrays_slot().begin_build(root));
        assert!(matches!(
            index.prepare_search_arrays(),
            Err(IndexError::TooLarge { .. })
        ));
        assert_eq!(
            index
                .search(&Query::new("unicode"), &CallContext::internal())
                .unwrap()
                .total,
            Some(1)
        );
    }

    #[test]
    fn a_refused_first_shard_exposes_the_reason_and_keeps_sql_available() {
        let (_directory, index, root) = fixture();
        let result = index.with_read(|connection| {
            index
                .arrays_slot()
                .build(connection, root, 1, &CallContext::internal())
        });
        assert!(matches!(result, Err(IndexError::TooLarge { .. })));
        let stats = index.search_array_stats().unwrap();
        assert_eq!(stats.bytes, 0);
        assert_eq!(stats.retained_bytes, 0);
        assert_eq!(stats.roots[0].root, root);
        assert!(stats.roots[0].refusal.is_some());
        let result = index
            .search_with(
                &crate::query::Fts5TrigramPlanner,
                &Query::new("unicode"),
                &CallContext::internal(),
            )
            .unwrap();
        assert_eq!(result.total, Some(1));
    }
}
