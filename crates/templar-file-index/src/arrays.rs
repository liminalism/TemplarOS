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
//! They are a cache of one committed generation, never an authority. A query uses them only when
//! they describe the generation the index is at and the roots it has; otherwise the search goes
//! through SQL and asks for a rebuild in the background. A build is demand-driven — only a search
//! starts one — so an index nobody searches costs nothing.

use crate::{
    EntryId, Result,
    model::{Coverage, EntryKind, Generation, NativePath, RootId},
    query::{Query, Rank, Sort},
};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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

/// One generation's live rows, flat, in path order.
#[derive(Debug)]
pub(crate) struct SearchArrays {
    generation: Generation,
    /// Every root and the position range its rows occupy, in position order.
    roots: Vec<RootSpan>,
    ids: Vec<EntryId>,
    /// Folded names, back to back; name `i` is `names[name_end[i - 1]..name_end[i]]`.
    names: Vec<u8>,
    name_end: Vec<u32>,
    /// The half-open position range of everything under a directory; empty for anything else.
    subtree_start: Vec<u32>,
    subtree_end: Vec<u32>,
    kind: Vec<u8>,
    /// Bit 0 hidden; bits 1-2 the [`Coverage`] tier.
    flags: Vec<u8>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    /// Directories by entry id, for resolving a `within` scope to its range.
    dirs_by_id: Vec<(EntryId, u32)>,
    /// Positions by modification time descending, ties by position.
    by_mtime_desc: Vec<u32>,
    /// Positions by raw name bytes, ties by position, and each position's dense name rank.
    by_name: Vec<u32>,
    name_rank: Vec<u32>,
    /// Positions by size ascending, ties by position.
    by_size: Vec<u32>,
}

/// One root's place in the arrays.
#[derive(Debug)]
struct RootSpan {
    root: RootId,
    path: Vec<u8>,
    /// First position of the root's rows (the root's own row, when it has one).
    start: u32,
    /// First position strictly under the root.
    descendants: u32,
    end: u32,
}

/// What a query against the arrays produced: the best `keep` positions' ids and ranks in order,
/// and how many rows matched in all.
pub(crate) struct ArraysAnswer {
    pub ids: Vec<EntryId>,
    pub ranks: Vec<Rank>,
    pub total: u64,
}

/// Where an index keeps its arrays, whether a rebuild is running, and when the last one ran.
#[derive(Debug, Default)]
pub(crate) struct ArraysSlot {
    current: RwLock<Option<Arc<SearchArrays>>>,
    building: AtomicBool,
    /// When the last background build started, and how long it took once it finished.
    last_build: Mutex<Option<(Instant, Duration)>>,
}

/// The shortest gap between two background builds. A tree that changes every second (a build
/// directory, a download) would otherwise keep one core rebuilding arrays that are stale again
/// before anyone searches them; SQL answers in between.
const MIN_REBUILD_INTERVAL: Duration = Duration::from_secs(5);

impl ArraysSlot {
    /// The arrays, if they describe exactly `generation` and `roots`.
    pub(crate) fn current_for(
        &self,
        generation: Generation,
        roots: &[RootId],
    ) -> Option<Arc<SearchArrays>> {
        let guard = self.current.read();
        let arrays = guard.as_ref()?;
        let mut held: Vec<RootId> = arrays.roots.iter().map(|span| span.root).collect();
        let mut wanted = roots.to_vec();
        held.sort_unstable();
        wanted.sort_unstable();
        (arrays.generation == generation && held == wanted).then(|| Arc::clone(arrays))
    }

    /// Claims the right to build. False when a build is already running, or when the last one
    /// started less than `max(MIN_REBUILD_INTERVAL, 2 × its duration)` ago.
    pub(crate) fn begin_build(&self) -> bool {
        let mut last = self.last_build.lock();
        if let Some((started, took)) = *last
            && started.elapsed() < MIN_REBUILD_INTERVAL.max(took * 2)
        {
            return false;
        }
        if self
            .building
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        *last = Some((Instant::now(), Duration::ZERO));
        true
    }

    /// Replaces the arrays outright, from a build the caller ran itself.
    pub(crate) fn install(&self, arrays: SearchArrays) {
        *self.current.write() = Some(Arc::new(arrays));
    }

    pub(crate) fn finish_build(&self, arrays: Option<SearchArrays>) {
        if let Some(arrays) = arrays {
            *self.current.write() = Some(Arc::new(arrays));
        }
        if let Some((started, took)) = self.last_build.lock().as_mut() {
            *took = started.elapsed();
        }
        self.building.store(false, Ordering::Release);
    }
}

impl SearchArrays {
    /// Reads every live row of every root, in one read transaction, stamped with the generation
    /// that transaction saw.
    pub(crate) fn build(connection: &Connection) -> Result<Self> {
        let transaction = connection.unchecked_transaction()?;
        let generation = crate::read_generation(&transaction)?;
        let mut root_list: Vec<(RootId, Vec<u8>)> = {
            let mut statement = transaction.prepare("SELECT root_id, path_bytes FROM roots")?;
            let rows = statement.query_map([], |row| {
                Ok((RootId(row.get(0)?), row.get::<_, Vec<u8>>(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // Disjoint subtrees are contiguous in byte order, so roots in path order, each in its
        // own path order, is the global path order the SQL search sorts by.
        root_list.sort_by(|left, right| left.1.cmp(&right.1));

        let mut arrays = Self::empty(generation);
        let mut raw_names: Vec<Vec<u8>> = Vec::new();
        let mut statement = transaction.prepare(
            "SELECT entry_id, name_bytes, name_fold, path_bytes, kind, size_bytes, mtime_ns,
                    hidden, coverage
               FROM entries
              WHERE root_id = ?1 AND deleted_generation IS NULL
              ORDER BY path_bytes",
        )?;
        for (root, root_path) in &root_list {
            let start = arrays.ids.len() as u32;
            let mut descendants = start;
            // Open directories: position, path, and the first byte string past their subtree.
            let mut open: Vec<(u32, Vec<u8>, Vec<u8>)> = Vec::new();
            let mut rows = statement.query([root.0])?;
            while let Some(row) = rows.next()? {
                let position = arrays.ids.len() as u32;
                let path: Vec<u8> = row.get(3)?;
                while let Some((directory, _, past)) = open.last() {
                    if path < *past {
                        break;
                    }
                    arrays.close_subtree(*directory, position);
                    open.pop();
                }
                // Everything still open sorts before `path`, and `path` sorts before each one's
                // end; only the top can be meeting its first descendant here (anything that sat
                // above an ancestor has already closed).
                if let Some((directory, directory_path, _)) = open.last()
                    && arrays.subtree_start[*directory as usize] == UNSET
                    && is_descendant(&path, directory_path)
                {
                    arrays.subtree_start[*directory as usize] = position;
                }
                let kind = row.get::<_, i64>(4)?;
                let id = EntryId(row.get(0)?);
                arrays.ids.push(id);
                raw_names.push(row.get(1)?);
                arrays
                    .names
                    .extend_from_slice(row.get::<_, String>(2)?.as_bytes());
                arrays.name_end.push(arrays.names.len() as u32);
                arrays.subtree_start.push(position + 1);
                arrays.subtree_end.push(position + 1);
                arrays.kind.push(kind as u8);
                let hidden = u8::from(row.get::<_, i64>(7)? != 0);
                let coverage = (row.get::<_, i64>(8)? as u8) & 0b11;
                arrays.flags.push(hidden | (coverage << 1));
                arrays.size.push(row.get::<_, i64>(5)? as u64);
                arrays.mtime.push(row.get(6)?);
                if path == *root_path {
                    descendants = position + 1;
                }
                if kind == EntryKind::Directory.as_i64() {
                    arrays.dirs_by_id.push((id, position));
                    arrays.subtree_start[position as usize] = UNSET;
                    let past = subtree_past(&path);
                    open.push((position, path, past));
                }
            }
            let end = arrays.ids.len() as u32;
            for (directory, _, _) in open {
                arrays.close_subtree(directory, end);
            }
            arrays.roots.push(RootSpan {
                root: *root,
                path: root_path.clone(),
                start,
                descendants,
                end,
            });
        }
        drop(statement);
        drop(transaction);
        arrays.finish_permutations(&raw_names);
        Ok(arrays)
    }

    fn empty(generation: Generation) -> Self {
        Self {
            generation,
            roots: Vec::new(),
            ids: Vec::new(),
            names: Vec::new(),
            name_end: Vec::new(),
            subtree_start: Vec::new(),
            subtree_end: Vec::new(),
            kind: Vec::new(),
            flags: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            dirs_by_id: Vec::new(),
            by_mtime_desc: Vec::new(),
            by_name: Vec::new(),
            name_rank: Vec::new(),
            by_size: Vec::new(),
        }
    }

    /// Ends a directory's subtree at `position`; one that never met a descendant is empty.
    fn close_subtree(&mut self, directory: u32, position: u32) {
        let directory = directory as usize;
        if self.subtree_start[directory] == UNSET {
            self.subtree_start[directory] = position;
        }
        self.subtree_end[directory] = position;
    }

    fn finish_permutations(&mut self, raw_names: &[Vec<u8>]) {
        let count = self.ids.len() as u32;
        self.dirs_by_id.sort_unstable();

        let mut by_mtime_desc: Vec<u32> = (0..count).collect();
        by_mtime_desc.sort_unstable_by(|left, right| {
            self.mtime[*right as usize]
                .cmp(&self.mtime[*left as usize])
                .then(left.cmp(right))
        });
        self.by_mtime_desc = by_mtime_desc;

        let mut by_size: Vec<u32> = (0..count).collect();
        by_size.sort_unstable_by(|left, right| {
            self.size[*left as usize]
                .cmp(&self.size[*right as usize])
                .then(left.cmp(right))
        });
        self.by_size = by_size;

        let mut by_name: Vec<u32> = (0..count).collect();
        by_name.sort_unstable_by(|left, right| {
            raw_names[*left as usize]
                .cmp(&raw_names[*right as usize])
                .then(left.cmp(right))
        });
        let mut name_rank = vec![0u32; count as usize];
        let mut rank = 0u32;
        for (index, position) in by_name.iter().enumerate() {
            if index > 0 && raw_names[by_name[index - 1] as usize] != raw_names[*position as usize]
            {
                rank += 1;
            }
            name_rank[*position as usize] = rank;
        }
        self.by_name = by_name;
        self.name_rank = name_rank;
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    fn name(&self, position: usize) -> &[u8] {
        let start = if position == 0 {
            0
        } else {
            self.name_end[position - 1] as usize
        };
        &self.names[start..self.name_end[position] as usize]
    }

    /// Answers `query` for the folded `needle`, or `None` when it cannot: a needle with a
    /// separator (a whole-path match these arrays do not hold) or a `within` directory they do
    /// not know. The caller then asks SQL, which can.
    pub(crate) fn search(
        &self,
        query: &Query,
        needle: &str,
        within_ids: &[Option<EntryId>],
        keep: usize,
    ) -> Option<ArraysAnswer> {
        if needle.contains('/') {
            return None;
        }
        let scope = self.scope(query, within_ids)?;
        let ranks = self.match_ranks(needle.as_bytes());
        let filter = Filter::of(query);

        let accept = |position: u32| -> Option<u8> {
            let rank = ranks[position as usize];
            (rank != NONE && in_scope(&scope, position) && filter.passes(self, position as usize))
                .then_some(rank)
        };

        let total = std::cell::Cell::new(0u64);
        let mut ids = Vec::new();
        let mut out_ranks = Vec::new();
        let mut push = |position: u32, rank: u8| {
            total.set(total.get() + 1);
            if ids.len() < keep {
                ids.push(self.ids[position as usize]);
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
        Some(ArraysAnswer {
            ids,
            ranks: out_ranks,
            total: total.get(),
        })
    }

    /// The position ranges a query may return from: its roots, narrowed to its `within`
    /// directories. `within_ids` is aligned with `query.within` and holds each directory's entry
    /// id where it has one. `None` when a `within` directory is neither a root nor a directory
    /// these arrays hold.
    fn scope(&self, query: &Query, within_ids: &[Option<EntryId>]) -> Option<Vec<(u32, u32)>> {
        let mut ranges: Vec<(u32, u32)> = self
            .roots
            .iter()
            .filter(|span| query.roots.is_empty() || query.roots.contains(&span.root))
            .map(|span| (span.start, span.end))
            .collect();
        if query.within.is_empty() {
            return Some(ranges);
        }
        let mut within = Vec::with_capacity(query.within.len());
        for (directory, id) in query.within.iter().zip(within_ids) {
            within.push(self.subtree_of(directory, *id)?);
        }
        ranges = ranges
            .iter()
            .flat_map(|(start, end)| {
                within.iter().filter_map(move |(low, high)| {
                    let low = (*low).max(*start);
                    let high = (*high).min(*end);
                    (low < high).then_some((low, high))
                })
            })
            .collect();
        ranges.sort_unstable();
        Some(ranges)
    }

    /// The descendants of `directory` (not the directory itself) as a position range.
    fn subtree_of(&self, directory: &NativePath, id: Option<EntryId>) -> Option<(u32, u32)> {
        // A root is not always a row of its own, so its range comes from the root list.
        if let Some(span) = self
            .roots
            .iter()
            .find(|span| span.path == directory.as_bytes())
        {
            return Some((span.descendants, span.end));
        }
        let id = id?;
        let index = self
            .dirs_by_id
            .binary_search_by(|(dir_id, _)| dir_id.cmp(&id))
            .ok()?;
        let position = self.dirs_by_id[index].1;
        Some((
            self.subtree_start[position as usize],
            self.subtree_end[position as usize],
        ))
    }

    /// Ranks every position: `0..=2` for a name match, [`PATH_ONLY`] for a row under a matching
    /// directory, [`NONE`] otherwise.
    fn match_ranks(&self, needle: &[u8]) -> Vec<u8> {
        let count = self.len();
        let mut ranks = vec![NONE; count];
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(8);
        if count < PARALLEL_FROM || threads == 1 {
            self.match_names(needle, 0, &mut ranks);
        } else {
            let chunk = count.div_ceil(threads);
            std::thread::scope(|scope| {
                for (index, slice) in ranks.chunks_mut(chunk).enumerate() {
                    scope.spawn(move || self.match_names(needle, index * chunk, slice));
                }
            });
        }
        // Path-only: every row in the subtree range of a directory whose name matches. Ranges
        // are marked as +1/-1 at their ends and swept once, so nested matches cost nothing extra.
        let directory = EntryKind::Directory.as_i64() as u8;
        let mut delta: Vec<i32> = Vec::new();
        for (_, position) in &self.dirs_by_id {
            let position = *position as usize;
            if ranks[position] < PATH_ONLY && self.kind[position] == directory {
                let (start, end) = (self.subtree_start[position], self.subtree_end[position]);
                if start < end {
                    if delta.is_empty() {
                        delta = vec![0; count + 1];
                    }
                    delta[start as usize] += 1;
                    delta[end as usize] -= 1;
                }
            }
        }
        if !delta.is_empty() {
            let mut depth = 0i32;
            for (rank, change) in ranks.iter_mut().zip(&delta) {
                depth += change;
                if depth > 0 && *rank == NONE {
                    *rank = PATH_ONLY;
                }
            }
        }
        ranks
    }

    fn match_names(&self, needle: &[u8], offset: usize, ranks: &mut [u8]) {
        for (index, slot) in ranks.iter_mut().enumerate() {
            let name = self.name(offset + index);
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

fn in_scope(scope: &[(u32, u32)], position: u32) -> bool {
    match scope.binary_search_by(|(start, _)| start.cmp(&position)) {
        Ok(_) => true,
        Err(0) => false,
        Err(index) => position < scope[index - 1].1,
    }
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
    fn of(query: &Query) -> Self {
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

    fn passes(&self, arrays: &SearchArrays, position: usize) -> bool {
        let flags = arrays.flags[position];
        if !self.include_hidden && flags & 1 != 0 {
            return false;
        }
        if self.coverage & (1 << ((flags >> 1) & 0b11)) == 0 {
            return false;
        }
        if let Some(kinds) = self.kinds
            && kinds & (1 << arrays.kind[position]) == 0
        {
            return false;
        }
        let size = arrays.size[position];
        let mtime = arrays.mtime[position];
        self.min_size.is_none_or(|min| size >= min)
            && self.max_size.is_none_or(|max| size <= max)
            && self.min_mtime.is_none_or(|min| mtime >= min)
            && self.max_mtime.is_none_or(|max| mtime <= max)
    }
}
