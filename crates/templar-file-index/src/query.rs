//! Search, ordering and paging.
//!
//! Two things here are load-bearing. The first is that **the order is total**: every sort ends in
//! `path_bytes` ascending, so paging cannot repeat or skip a row the way a sort with ties does.
//! The second is that a cursor is **pinned to the generation it was opened on** and never drifts
//! to a newer one. When it can no longer be honoured — expired, evicted, or the daemon restarted —
//! the answer is [`IndexError::SnapshotRequired`], and the client asks again. A service that
//! quietly re-runs the query against newer data has silently changed the answer under a user who
//! is still scrolling the old one.
//!
//! A search is **keys first**. The planner finds the entries whose *name* matches (FTS5 trigram
//! over names for three characters or more, a `LIKE` scan of folded names otherwise); the rows
//! under a matching directory are the path-only matches. SQLite then counts every match, orders
//! them and keeps the best [`crate::IndexConfig::max_cursor_rows`], returning only ids and ranks.
//! Full rows are read for the page a caller is shown and for nothing else. So `total` is always
//! exact, and a cut-off result is the best rows rather than the first ones found. The planner sits
//! behind [`QueryPlanner`], so a measured need can replace the accelerator without touching the
//! IPC contract.

use crate::{
    ENTRY_COLUMNS, IndexError, Result,
    arrays::SearchArrays,
    entry_row_from_row,
    generation::fold,
    model::{
        Coverage, EntryKind, EntryRow, Generation, MetadataFreshness, NativePath, RootId,
        RootStatus,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Write as _,
    time::{Duration, Instant},
};

/// Which fields a caller wants. Trimming them is what keeps a two-million-row snapshot from being
/// a two-million-row JSON document.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FieldMask(pub u32);

impl FieldMask {
    pub const NAME: Self = Self(1);
    pub const PATH: Self = Self(2);
    pub const KIND: Self = Self(4);
    pub const SIZE: Self = Self(8);
    pub const TIMES: Self = Self(16);
    pub const MODE: Self = Self(32);
    pub const OBJECT_KEY: Self = Self(64);
    pub const PARENT: Self = Self(128);
    pub const ALL: Self = Self(255);
    /// The recursive totals a `snapshot::SnapshotDetail::Summary` entry carries for an `Opaque`
    /// directory (`aggregate::AggregateRow`). Not part of [`Self::ALL`]: it changes the payload's
    /// byte shape, so a payload that carries it stamps a newer snapshot format
    /// (`snapshot::FORMAT_EXTENDED`) rather than silently growing the one an old reader expects.
    pub const AGGREGATE: Self = Self(256);
    /// The row's [`crate::model::Coverage`] tier. Same format-bump rule as [`Self::AGGREGATE`],
    /// and for the same reason a summary snapshot always carries it: a caller cannot tell whether
    /// a directory's aggregate is meaningful without knowing it is `Opaque`.
    pub const COVERAGE: Self = Self(512);

    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl Default for FieldMask {
    fn default() -> Self {
        Self::ALL
    }
}

/// How a query is sorted. Whichever is chosen, the final tie-break is the path, so the order is
/// total and a page boundary is stable.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sort {
    /// Exact, then prefix, then substring, then path-only; most recently modified within each.
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    Mtime,
}

/// Why a row matched, which is what lets a UI group and explain results.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rank {
    Exact,
    Prefix,
    Substring,
    PathOnly,
}

impl Rank {
    #[must_use]
    pub fn score(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Prefix => 1,
            Self::Substring => 2,
            Self::PathOnly => 3,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
            Self::Substring => "substring",
            Self::PathOnly => "path-only",
        }
    }
}

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    /// Empty searches every root.
    #[serde(default)]
    pub roots: Vec<RootId>,
    #[serde(default)]
    pub kinds: Option<Vec<EntryKind>>,
    #[serde(default)]
    pub min_size: Option<u64>,
    #[serde(default)]
    pub max_size: Option<u64>,
    #[serde(default)]
    pub min_mtime_ns: Option<i64>,
    #[serde(default)]
    pub max_mtime_ns: Option<i64>,
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub descending: bool,
    #[serde(default)]
    pub fields: FieldMask,
    #[serde(default)]
    pub page_size: u32,
    /// Which coverage tiers a result may contain. Empty — the default, and what an absent JSON
    /// field means — is `[Detailed]`: a filename search sees what a person wrote and not the
    /// contents of a build directory. A drive visualiser asks for `[Opaque, Inventory, Detailed]`.
    ///
    /// Additive on purpose. `Index1.Search` takes a JSON string precisely so a new filter costs no
    /// D-Bus signature change.
    #[serde(default)]
    pub include_coverage: Vec<Coverage>,
    /// Set by a caller whose answer depends on sizes, times or modes rather than on names —
    /// `sort:size`, a drive visualiser, a "largest files" panel.
    ///
    /// It never changes which rows come back and it never blocks. It asks the index to name the
    /// roots whose metadata was not re-read, in [`SearchResult::stale_metadata_roots`], and to
    /// tell the host to queue a repair for them.
    #[serde(default)]
    pub require_metadata: bool,
    /// Directories the results must lie under (descendants only, not the directory itself). Empty
    /// is everywhere. Answered in SQL with the same byte ranges a subtree read uses, so a search
    /// scoped to one folder sees every match in it, not the slice of a global top-N that happens
    /// to fall inside.
    #[serde(default)]
    pub within: Vec<NativePath>,
}

impl Query {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            include_hidden: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn in_roots(mut self, roots: Vec<RootId>) -> Self {
        self.roots = roots;
        self
    }

    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size;
        self
    }

    /// Which tiers this query may return. Empty means the default rather than "nothing": a
    /// deserialised `"include_coverage": []` and an absent field have to mean the same thing, or
    /// a client that sends one gets an empty result set with no error to explain it.
    #[must_use]
    pub fn coverage_filter(&self) -> Vec<Coverage> {
        if self.include_coverage.is_empty() {
            vec![Coverage::Detailed]
        } else {
            self.include_coverage.clone()
        }
    }

    /// Every tier, for a consumer that accounts for a whole drive.
    #[must_use]
    pub fn including_every_tier(mut self) -> Self {
        self.include_coverage = vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed];
        self
    }

    fn effective_page_size(&self) -> usize {
        if self.page_size == 0 {
            100
        } else {
            self.page_size.clamp(1, 1000) as usize
        }
    }
}

/// One result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub row: EntryRow,
    pub rank: Rank,
    /// False when the row was removed after the cursor pinned it. The page keeps its shape rather
    /// than shifting every later row up by one.
    pub present: bool,
}

/// A page of a pinned result set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub cursor: CursorId,
    pub generation: Generation,
    pub offset: u32,
    pub hits: Vec<Hit>,
    pub total: Option<u64>,
    /// Rows the cursor listed that no longer exist.
    pub dropped: u32,
}

/// A search, its first page, and the freshness of everything it looked at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub cursor: CursorId,
    pub generation: Generation,
    /// Every match, counted exactly.
    pub total: Option<u64>,
    /// The cursor holds the best [`crate::IndexConfig::max_cursor_rows`] of `total` matches, in
    /// order; the rest were counted but cannot be paged to.
    pub truncated: bool,
    pub strategy: String,
    /// The state of every root in scope, so nothing can render results without their freshness.
    pub roots: Vec<RootStatus>,
    /// Roots that answered this query out of metadata nobody re-read. Empty unless the query set
    /// [`Query::require_metadata`] — a filename search neither pays for the distinction nor is
    /// told about it.
    #[serde(default)]
    pub stale_metadata_roots: Vec<RootId>,
    pub hits: Vec<Hit>,
}

pub type CursorId = String;

/// A pinned, ordered result set.
#[derive(Clone, Debug)]
pub struct Cursor {
    pub id: CursorId,
    pub generation: Generation,
    pub roots: Vec<RootId>,
    pub ids: Vec<crate::EntryId>,
    pub ranks: Vec<Rank>,
    pub total: Option<u64>,
    pub truncated: bool,
    last_used: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct CursorStore {
    cursors: HashMap<CursorId, Cursor>,
}

impl CursorStore {
    pub(crate) fn len(&self) -> usize {
        self.cursors.len()
    }

    pub(crate) fn oldest_generation(&self) -> Option<Generation> {
        self.cursors.values().map(|cursor| cursor.generation).min()
    }

    pub(crate) fn insert(&mut self, cursor: Cursor, max: usize, ttl: Duration) {
        self.expire(ttl);
        while self.cursors.len() >= max {
            let Some(victim) = self
                .cursors
                .values()
                .min_by_key(|cursor| cursor.last_used)
                .map(|cursor| cursor.id.clone())
            else {
                break;
            };
            self.cursors.remove(&victim);
        }
        self.cursors.insert(cursor.id.clone(), cursor);
    }

    pub(crate) fn get(&mut self, id: &str, ttl: Duration) -> Result<&Cursor> {
        self.expire(ttl);
        match self.cursors.get_mut(id) {
            Some(cursor) => {
                cursor.last_used = Instant::now();
                Ok(&*cursor)
            }
            None => Err(IndexError::SnapshotRequired {
                reason: "the cursor has expired or was never issued by this process",
            }),
        }
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.cursors.remove(id);
    }

    pub(crate) fn invalidate_root(&mut self, root: RootId) {
        self.cursors
            .retain(|_, cursor| !cursor.roots.contains(&root));
    }

    fn expire(&mut self, ttl: Duration) {
        self.cursors
            .retain(|_, cursor| cursor.last_used.elapsed() < ttl);
    }
}

/// How a needle becomes candidate rows: SQL that selects the `entry_id` of every row whose **name**
/// matches, before any filter, scope or ordering. [`crate::Index::search_with`] adds the rows under
/// matching directories (the path-only matches), the filters, the ranking and the order around it,
/// so a planner decides only how a name is found.
#[derive(Clone, Debug)]
pub struct PlannedQuery {
    /// One `SELECT` of a single `entry_id` column. May use `?1`..`?n` for `binds`, and nothing else.
    pub candidates_sql: String,
    pub binds: Vec<rusqlite::types::Value>,
    pub strategy: &'static str,
    /// Whether rows under a matching directory are path-only matches. False when the planner
    /// already matched against whole paths, as it must for a needle containing a separator.
    pub expand_directories: bool,
}

/// How a query becomes candidate rows.
///
/// Behind a trait on purpose: FTS5 is a measured choice, not an architectural one, and a sorted or
/// memory-mapped index can replace it without any consumer noticing.
pub trait QueryPlanner: Send + Sync + std::fmt::Debug {
    fn plan(&self, query: &Query, needle: &str, fts_available: bool) -> PlannedQuery;
    fn name(&self) -> &'static str;
}

/// The default: FTS5 trigram over names for needles of three characters or more, a `LIKE` scan of
/// the folded names otherwise, and a `LIKE` scan of whole paths for a needle that names a path.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fts5TrigramPlanner;

impl QueryPlanner for Fts5TrigramPlanner {
    fn plan(&self, query: &Query, needle: &str, fts_available: bool) -> PlannedQuery {
        // A name never contains a separator, so `a/b` can only ever be a path match — and the
        // only place that substring exists is the whole path.
        if needle.contains('/') {
            return PlannedQuery {
                candidates_sql: "SELECT entry_id FROM entries
                                  WHERE path_display LIKE '%' || ?1 || '%' ESCAPE '\\'"
                    .to_owned(),
                binds: vec![rusqlite::types::Value::Text(escape_like(needle))],
                strategy: "path-scan",
                expand_directories: false,
            };
        }
        // The accelerator holds `Detailed` rows and only those, which is what keeps it from
        // carrying a build tree's worth of trigrams. A query that wants a wider set cannot be
        // answered from it at all: it goes down the scan rather than quietly returning the
        // searchable subset of what was asked for.
        let searchable_only = query.coverage_filter() == [Coverage::Detailed];
        // The trigram tokenizer cannot index a term shorter than three characters.
        if fts_available && searchable_only && needle.chars().count() >= 3 {
            return PlannedQuery {
                candidates_sql: "SELECT rowid FROM entries_fts WHERE entries_fts MATCH ?1"
                    .to_owned(),
                binds: vec![rusqlite::types::Value::Text(format!(
                    "\"{}\"",
                    needle.replace('"', "\"\"")
                ))],
                strategy: "fts5-trigram",
                expand_directories: true,
            };
        }
        PlannedQuery {
            candidates_sql: "SELECT entry_id FROM entries
                              WHERE name_fold LIKE '%' || ?1 || '%' ESCAPE '\\'"
                .to_owned(),
            binds: vec![rusqlite::types::Value::Text(escape_like(needle))],
            strategy: "like-scan",
            expand_directories: true,
        }
    }

    fn name(&self) -> &'static str {
        "fts5-trigram"
    }
}

/// The predicates every candidate passes after matching, as SQL over `entries` with literal
/// integers (nothing here comes from free text) and `?n` binds for the byte-valued scope.
fn filter_sql(query: &Query, binds: &mut Vec<rusqlite::types::Value>) -> String {
    let mut filters = String::new();
    if !query.roots.is_empty() {
        let list = query
            .roots
            .iter()
            .map(|root| root.0.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(filters, " AND root_id IN ({list})");
    }
    if let Some(kinds) = &query.kinds
        && !kinds.is_empty()
    {
        let list = kinds
            .iter()
            .map(|kind| kind.as_i64().to_string())
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(filters, " AND kind IN ({list})");
    }
    if let Some(min) = query.min_size {
        let _ = write!(filters, " AND size_bytes >= {min}");
    }
    if let Some(max) = query.max_size {
        let _ = write!(filters, " AND size_bytes <= {max}");
    }
    if let Some(min) = query.min_mtime_ns {
        let _ = write!(filters, " AND mtime_ns >= {min}");
    }
    if let Some(max) = query.max_mtime_ns {
        let _ = write!(filters, " AND mtime_ns <= {max}");
    }
    if !query.include_hidden {
        filters.push_str(" AND hidden = 0");
    }
    // The tier predicate applies to every row, not only to the accelerator's: a `LIKE` scan and a
    // directory expansion both read `entries` directly, and would otherwise return an
    // `Inventory` row the accelerator never held.
    let tiers = query
        .coverage_filter()
        .iter()
        .map(|coverage| coverage.as_i64().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let _ = write!(filters, " AND coverage IN ({tiers})");
    if !query.within.is_empty() {
        let mut ranges = Vec::with_capacity(query.within.len());
        for directory in &query.within {
            let (low, high) = directory.subtree_bounds();
            binds.push(rusqlite::types::Value::Blob(low));
            let low_index = binds.len();
            binds.push(rusqlite::types::Value::Blob(high));
            ranges.push(format!(
                "(path_bytes >= ?{low_index} AND path_bytes < ?{})",
                low_index + 1
            ));
        }
        let _ = write!(filters, " AND ({})", ranges.join(" OR "));
    }
    filters
}

/// The `ORDER BY` for a sort: the requested key, then the path, so the order is total. SQLite
/// compares blobs with `memcmp`, which is exactly how [`NativePath`] orders, so this is the same
/// order the rows had when they were sorted in Rust.
fn order_sql(query: &Query) -> &'static str {
    match (query.sort, query.descending) {
        (Sort::Relevance, _) => "rank, mtime_ns DESC, path_bytes",
        (Sort::Name, false) => "name_bytes, path_bytes",
        (Sort::Name, true) => "name_bytes DESC, path_bytes",
        (Sort::Path, false) => "path_bytes",
        (Sort::Path, true) => "path_bytes DESC",
        (Sort::Size, false) => "size_bytes, path_bytes",
        (Sort::Size, true) => "size_bytes DESC, path_bytes",
        (Sort::Mtime, false) => "mtime_ns, path_bytes",
        (Sort::Mtime, true) => "mtime_ns DESC, path_bytes",
    }
}

/// [`Rank`] as SQL over `name_fold`, which holds `fold(name)` — the same comparison `Rank` has
/// always meant, made where the rows are rather than after decoding every one of them.
fn rank_sql(needle_param: usize) -> String {
    format!(
        "CASE WHEN name_fold = ?{needle_param} THEN 0
              WHEN substr(name_fold, 1, length(?{needle_param})) = ?{needle_param} THEN 1
              WHEN instr(name_fold, ?{needle_param}) > 0 THEN 2
              ELSE 3 END"
    )
}

fn rank_from_score(score: i64) -> Rank {
    match score {
        0 => Rank::Exact,
        1 => Rank::Prefix,
        2 => Rank::Substring,
        _ => Rank::PathOnly,
    }
}

/// The directories among the name matches whose subtrees are path-only matches, with every
/// directory inside another one dropped: its rows are already covered, and expanding it again
/// would read them twice. Returned as `[root, low, high]` byte ranges in JSON for `json_each`.
fn directory_ranges(
    connection: &rusqlite::Connection,
    plan: &PlannedQuery,
) -> Result<Option<String>> {
    if !plan.expand_directories {
        return Ok(None);
    }
    let mut statement = connection.prepare(&format!(
        "SELECT root_id, path_bytes FROM entries
          WHERE entry_id IN ({}) AND kind = {} AND deleted_generation IS NULL
          ORDER BY root_id, path_bytes",
        plan.candidates_sql,
        EntryKind::Directory.as_i64()
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(plan.binds.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut json = String::from("[");
    let mut kept: Option<(i64, NativePath)> = None;
    let mut any = false;
    for row in rows {
        let (root, path) = row?;
        let path = NativePath::from_bytes(path);
        if let Some((kept_root, kept_path)) = &kept
            && *kept_root == root
            && path.starts_with(kept_path)
        {
            continue;
        }
        let (low, high) = path.subtree_bounds();
        if any {
            json.push(',');
        }
        let _ = write!(json, "[{root},\"{}\",\"{}\"]", hex(&low), hex(&high));
        any = true;
        kept = Some((root, path));
    }
    json.push(']');
    Ok(any.then_some(json))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl crate::Index {
    /// Searches with the default plan: the in-memory arrays when [`IndexConfig::search_arrays`]
    /// is on and they describe the current generation, SQL otherwise.
    ///
    /// [`IndexConfig::search_arrays`]: crate::IndexConfig::search_arrays
    pub fn search(&self, query: &Query) -> Result<SearchResult> {
        self.run_search(None, query)
    }

    /// Searches through SQL with a specific planner. The seam a replacement accelerator plugs
    /// into; never answered from the arrays.
    pub fn search_with(&self, planner: &dyn QueryPlanner, query: &Query) -> Result<SearchResult> {
        self.run_search(Some(planner), query)
    }

    /// Builds the search arrays for the current generation now, on this thread, and returns how
    /// many rows they hold. A search builds them in the background on its own; this is for a
    /// caller that wants them ready before it measures or serves.
    pub fn prepare_search_arrays(&self) -> Result<usize> {
        let arrays = self.with_read(SearchArrays::build)?;
        let rows = arrays.len();
        self.arrays_slot().install(arrays);
        Ok(rows)
    }

    fn run_search(
        &self,
        planner: Option<&dyn QueryPlanner>,
        query: &Query,
    ) -> Result<SearchResult> {
        let needle = fold(query.text.trim());
        let generation = self.generation()?;
        let all_roots = self.roots()?;
        let root_ids: Vec<RootId> = all_roots.iter().map(|status| status.root).collect();
        let roots = scoped_roots(query, all_roots);
        let stale_metadata_roots = self.flag_stale_metadata(query, &roots);
        if needle.is_empty() {
            // A search box that has been cleared must not dump the filesystem.
            return Ok(SearchResult {
                cursor: String::new(),
                generation,
                total: Some(0),
                truncated: false,
                strategy: planner
                    .map_or("fts5-trigram", QueryPlanner::name)
                    .to_owned(),
                roots,
                stale_metadata_roots,
                hits: Vec::new(),
            });
        }

        let keep = self.config().max_cursor_rows;
        let page_size = query.effective_page_size();
        let from_arrays = if planner.is_none() && self.config().search_arrays {
            self.search_arrays(query, &needle, generation, &root_ids, keep, page_size)?
        } else {
            None
        };
        let ((ids, ranks, total, hits), strategy) = if let Some(answer) = from_arrays {
            (answer, "arrays")
        } else {
            let planner = planner.unwrap_or(&Fts5TrigramPlanner);
            let plan = planner.plan(query, &needle, self.fts_available());
            let answer = self.with_read(|connection| {
                ranked_matches(connection, &plan, query, &needle, keep, page_size)
            })?;
            (answer, plan.strategy)
        };
        // The cursor holds the best `keep` rows of `total`, never an arbitrary `keep`: the order
        // is applied before the cut.
        let truncated = total > ids.len() as u64;

        let cursor = Cursor {
            id: uuid::Uuid::new_v4().to_string(),
            generation,
            roots: if query.roots.is_empty() {
                root_ids
            } else {
                query.roots.clone()
            },
            ids,
            ranks,
            total: Some(total),
            truncated,
            last_used: Instant::now(),
        };
        let cursor_id = cursor.id.clone();

        {
            let config = self.config().clone();
            self.cursors()
                .lock()
                .insert(cursor, config.max_cursors, config.cursor_ttl);
        }

        Ok(SearchResult {
            cursor: cursor_id,
            generation,
            total: Some(total),
            truncated,
            strategy: strategy.to_owned(),
            roots,
            stale_metadata_roots,
            hits,
        })
    }

    /// The arrays' answer, or `None` when they cannot give one: not built for this generation and
    /// these roots (a background build is requested, and SQL answers this time), a needle that
    /// names a path, or a `within` directory they do not hold.
    fn search_arrays(
        &self,
        query: &Query,
        needle: &str,
        generation: Generation,
        root_ids: &[RootId],
        keep: usize,
        page_size: usize,
    ) -> Result<Option<RankedMatches>> {
        let Some(arrays) = self.arrays_slot().current_for(generation, root_ids) else {
            self.request_arrays();
            return Ok(None);
        };
        self.with_read(|connection| {
            let within_ids = resolve_within(connection, &query.within)?;
            let Some(answer) = arrays.search(query, needle, &within_ids, keep) else {
                return Ok(None);
            };
            let first = answer.ids.len().min(page_size);
            let (hits, _) = materialize(connection, &answer.ids[..first], &answer.ranks[..first])?;
            Ok(Some((answer.ids, answer.ranks, answer.total, hits)))
        })
    }

    /// Starts one background build of the arrays, unless one is already running.
    fn request_arrays(&self) {
        if !self.arrays_slot().begin_build() {
            return;
        }
        let index = self.clone();
        let spawned = std::thread::Builder::new()
            .name("templar-index-arrays".to_owned())
            .spawn(move || {
                let started = Instant::now();
                match index.with_read(SearchArrays::build) {
                    Ok(arrays) => {
                        tracing::info!(
                            rows = arrays.len(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "built the search arrays"
                        );
                        index.arrays_slot().finish_build(Some(arrays));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not build the search arrays; SQL answers");
                        index.arrays_slot().finish_build(None);
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the search-array builder; SQL answers");
            self.arrays_slot().finish_build(None);
        }
    }

    /// Names the roots that cannot vouch for the metadata this query depends on, and asks the host
    /// to repair them. Never waits for the repair: the rows are served either way.
    fn flag_stale_metadata(&self, query: &Query, roots: &[RootStatus]) -> Vec<RootId> {
        if !query.require_metadata {
            return Vec::new();
        }
        let stale: Vec<RootId> = roots
            .iter()
            .filter(|status| status.metadata == MetadataFreshness::Unverified)
            .map(|status| status.root)
            .collect();
        if let Some(sink) = self.sink() {
            for root in &stale {
                sink.metadata_repair_requested(*root);
            }
        }
        stale
    }

    /// Random access inside a pinned result set.
    ///
    /// The generation is the cursor's own, never a newer one, and a row removed since is returned
    /// as absent rather than shifting the page.
    pub fn fetch_page(&self, cursor_id: &str, offset: u32, count: u32) -> Result<Page> {
        let ttl = self.config().cursor_ttl;
        let (generation, slice, ranks, total) = {
            let mut store = self.cursors().lock();
            let cursor = store.get(cursor_id, ttl)?;
            let start = offset as usize;
            let end = start.saturating_add(count.clamp(1, 1000) as usize);
            let slice: Vec<crate::EntryId> = cursor
                .ids
                .get(start..end.min(cursor.ids.len()))
                .unwrap_or(&[])
                .to_vec();
            let ranks: Vec<Rank> = cursor
                .ranks
                .get(start..end.min(cursor.ranks.len()))
                .unwrap_or(&[])
                .to_vec();
            (cursor.generation, slice, ranks, cursor.total)
        };

        let (hits, dropped) =
            self.with_read(|connection| materialize(connection, &slice, &ranks))?;

        Ok(Page {
            cursor: cursor_id.to_owned(),
            generation,
            offset,
            hits,
            total,
            dropped,
        })
    }

    /// Releases a cursor early. Expiry covers a client that simply vanished.
    pub fn close_cursor(&self, cursor_id: &str) {
        self.cursors().lock().remove(cursor_id);
    }
}

fn scoped_roots(query: &Query, all: Vec<RootStatus>) -> Vec<RootStatus> {
    if query.roots.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|status| query.roots.contains(&status.root))
        .collect()
}

/// Each `within` directory's live entry id, where it has a row of its own.
fn resolve_within(
    connection: &rusqlite::Connection,
    within: &[NativePath],
) -> Result<Vec<Option<crate::EntryId>>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT entry_id FROM entries
          WHERE path_bytes = ?1 AND kind = {} AND deleted_generation IS NULL
          LIMIT 1",
        EntryKind::Directory.as_i64()
    ))?;
    within
        .iter()
        .map(|directory| {
            match statement.query_row([directory.as_bytes()], |row| row.get::<_, i64>(0)) {
                Ok(id) => Ok(Some(crate::EntryId(id))),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(IndexError::Sqlite(error)),
            }
        })
        .collect()
}

/// Ordered ids and ranks of the best `keep` matches, the exact count of all of them, and the first
/// page's rows — in one read transaction, so `total` and the page cannot disagree about a row a
/// commit added between them.
type RankedMatches = (Vec<crate::EntryId>, Vec<Rank>, u64, Vec<Hit>);

fn ranked_matches(
    connection: &rusqlite::Connection,
    plan: &PlannedQuery,
    query: &Query,
    needle: &str,
    keep: usize,
    page_size: usize,
) -> Result<RankedMatches> {
    let transaction = connection.unchecked_transaction()?;
    let mut binds = plan.binds.clone();
    let mut candidates = plan.candidates_sql.clone();
    if let Some(ranges) = directory_ranges(&transaction, plan)? {
        binds.push(rusqlite::types::Value::Text(ranges));
        let _ = write!(
            candidates,
            " UNION SELECT e.entry_id FROM json_each(?{}) AS r
                JOIN entries AS e
                  ON e.root_id = json_extract(r.value, '$[0]')
                 AND e.path_bytes >= unhex(json_extract(r.value, '$[1]'))
                 AND e.path_bytes < unhex(json_extract(r.value, '$[2]'))",
            binds.len()
        );
    }
    let filters = filter_sql(query, &mut binds);
    let matching = format!(
        "WITH candidates(id) AS ({candidates})
         SELECT {{columns}} FROM entries
          WHERE entry_id IN (SELECT id FROM candidates)
            AND deleted_generation IS NULL{filters}"
    );

    // The needle goes last so the count below, which does not rank, binds exactly the parameters
    // it uses.
    let count_binds = binds.len();
    binds.push(rusqlite::types::Value::Text(needle.to_owned()));
    let ordered = format!(
        "{} ORDER BY {} LIMIT {keep}",
        matching.replace(
            "{columns}",
            &format!("entry_id, {} AS rank", rank_sql(binds.len()))
        ),
        order_sql(query)
    );
    let mut statement = transaction.prepare(&ordered)?;
    let rows = statement.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
        Ok((crate::EntryId(row.get(0)?), rank_from_score(row.get(1)?)))
    })?;
    let mut ids = Vec::new();
    let mut ranks = Vec::new();
    for row in rows {
        let (id, rank) = row?;
        ids.push(id);
        ranks.push(rank);
    }
    drop(statement);

    // Only a result the cut reached needs a second pass to count what lies beyond it.
    let total = if ids.len() < keep {
        ids.len() as u64
    } else {
        transaction.query_row(
            &matching.replace("{columns}", "count(*)"),
            rusqlite::params_from_iter(binds[..count_binds].iter()),
            |row| row.get::<_, i64>(0),
        )? as u64
    };
    let first = ids.len().min(page_size);
    let (hits, _) = materialize(&transaction, &ids[..first], &ranks[..first])?;
    Ok((ids, ranks, total, hits))
}

/// Full rows for an ordered slice of ids, in that order, in one statement. A row deleted since the
/// cursor pinned it comes back as absent rather than shifting the page; a row swept entirely is
/// counted in `dropped`.
fn materialize(
    connection: &rusqlite::Connection,
    ids: &[crate::EntryId],
    ranks: &[Rank],
) -> Result<(Vec<Hit>, u32)> {
    if ids.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let list = format!(
        "[{}]",
        ids.iter()
            .map(|id| id.0.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries
          WHERE entry_id IN (SELECT value FROM json_each(?1))"
    ))?;
    let mut found: HashMap<crate::EntryId, EntryRow> = HashMap::with_capacity(ids.len());
    for row in statement.query_map([list], entry_row_from_row)? {
        let row = row?;
        found.insert(row.entry_id, row);
    }
    let mut hits = Vec::with_capacity(ids.len());
    let mut dropped = 0u32;
    for (id, rank) in ids.iter().zip(ranks) {
        match found.remove(id) {
            Some(row) => {
                let present = row.deleted_generation.is_none();
                if !present {
                    dropped += 1;
                }
                hits.push(Hit {
                    row,
                    rank: *rank,
                    present,
                });
            }
            None => dropped += 1,
        }
    }
    Ok((hits, dropped))
}

/// `LIKE` treats `%` and `_` as wildcards; a file called `report_final` must not match
/// `reportXfinal`.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The compatibility shape the existing `Files.Search1` clients already parse.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyHit {
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub mime: Option<String>,
}

impl LegacyHit {
    #[must_use]
    pub fn of(row: &EntryRow) -> Self {
        Self {
            path: row.path.display(),
            is_dir: row.meta.kind == EntryKind::Directory,
            size_bytes: row.meta.size_bytes,
            mtime_ms: row.meta.mtime_ns / 1_000_000,
            mime: row.meta.mime.clone(),
        }
    }
}

/// A path as it crosses the bus: the bytes that open the file, and a string that can be drawn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WirePath {
    /// Base64 of the native bytes. The authority.
    pub bytes_b64: String,
    /// Lossy rendering. Presentation only.
    pub display: String,
}

impl WirePath {
    #[must_use]
    pub fn of(path: &NativePath) -> Self {
        Self {
            bytes_b64: base64(path.as_bytes()),
            display: path.display(),
        }
    }
}

/// Base64, implemented here rather than pulled in: it is twenty lines and this crate ships no
/// dependency it does not need.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (chunk.get(1).map_or(0, |byte| u32::from(*byte)) << 8)
            | chunk.get(2).map_or(0, |byte| u32::from(*byte));
        out.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        out.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(bits >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[bits as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The inverse, so a client's bytes come back exactly as they went out.
#[must_use]
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        bits = (bits << 6) | value;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(((bits >> held) & 0xff) as u8);
        }
    }
    Some(out)
}
