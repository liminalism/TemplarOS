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
//! Substring matching goes through FTS5's trigram tokenizer, which is what makes
//! `LIKE '%needle%'` over a million rows stop being the plan. Trigrams need three characters, so a
//! one- or two-character substring falls back to a bounded candidate scan that reports itself as
//! truncated rather than pretending to be exhaustive. Both live behind [`QueryPlanner`], so a
//! measured need can replace the accelerator without touching the IPC contract.

use crate::{
    ENTRY_COLUMNS, IndexError, Result, entry_row_from_row,
    generation::fold,
    model::{EntryKind, EntryRow, Generation, NativePath, RootId, RootStatus},
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

    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
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
    pub total: Option<u64>,
    /// The candidate phase hit its limit: `total` is a floor, not a count.
    pub truncated: bool,
    pub strategy: String,
    /// The state of every root in scope, so nothing can render results without their freshness.
    pub roots: Vec<RootStatus>,
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

/// A SQL candidate phase, chosen by a planner.
#[derive(Clone, Debug)]
pub struct PlannedQuery {
    pub sql: String,
    pub binds: Vec<rusqlite::types::Value>,
    pub strategy: &'static str,
    /// Whether hitting the candidate limit means the answer is a floor rather than a count.
    pub bounded: bool,
}

/// How a query becomes candidate rows.
///
/// Behind a trait on purpose: FTS5 is a measured choice, not an architectural one, and a sorted or
/// memory-mapped index can replace it without any consumer noticing.
pub trait QueryPlanner: Send + Sync + std::fmt::Debug {
    fn plan(
        &self,
        query: &Query,
        needle: &str,
        fts_available: bool,
        candidate_limit: usize,
    ) -> PlannedQuery;
    fn name(&self) -> &'static str;
}

/// The default: FTS5 trigram for substrings of three characters or more, a bounded scan otherwise.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fts5TrigramPlanner;

impl QueryPlanner for Fts5TrigramPlanner {
    fn plan(
        &self,
        query: &Query,
        needle: &str,
        fts_available: bool,
        candidate_limit: usize,
    ) -> PlannedQuery {
        let mut binds: Vec<rusqlite::types::Value> = Vec::new();
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

        // The trigram tokenizer cannot index a term shorter than three characters, so a one- or
        // two-character substring is honestly a scan — bounded, and reported as truncated.
        let trigram_capable = fts_available && needle.chars().count() >= 3;
        let sql = if trigram_capable {
            binds.push(rusqlite::types::Value::Text(format!(
                "\"{}\"",
                needle.replace('"', "\"\"")
            )));
            format!(
                "SELECT {ENTRY_COLUMNS} FROM entries
                  WHERE entry_id IN (SELECT rowid FROM entries_fts WHERE entries_fts MATCH ?1)
                    AND deleted_generation IS NULL{filters}
                  LIMIT {}",
                candidate_limit + 1
            )
        } else {
            binds.push(rusqlite::types::Value::Text(escape_like(needle)));
            format!(
                "SELECT {ENTRY_COLUMNS} FROM entries
                  WHERE (name_fold LIKE '%' || ?1 || '%' ESCAPE '\\'
                         OR path_display LIKE '%' || ?1 || '%' ESCAPE '\\')
                    AND deleted_generation IS NULL{filters}
                  LIMIT {}",
                candidate_limit + 1
            )
        };

        PlannedQuery {
            sql,
            binds,
            strategy: if trigram_capable {
                "fts5-trigram"
            } else {
                "like-scan"
            },
            bounded: true,
        }
    }

    fn name(&self) -> &'static str {
        "fts5-trigram"
    }
}

impl crate::Index {
    /// Searches with the default planner.
    pub fn search(&self, query: &Query) -> Result<SearchResult> {
        self.search_with(&Fts5TrigramPlanner, query)
    }

    /// Searches with a specific planner. The seam a replacement accelerator plugs into.
    pub fn search_with(&self, planner: &dyn QueryPlanner, query: &Query) -> Result<SearchResult> {
        let needle = fold(query.text.trim());
        let generation = self.generation()?;
        let roots = self.scoped_roots(query)?;
        if needle.is_empty() {
            // A search box that has been cleared must not dump the filesystem.
            return Ok(SearchResult {
                cursor: String::new(),
                generation,
                total: Some(0),
                truncated: false,
                strategy: planner.name().to_owned(),
                roots,
                hits: Vec::new(),
            });
        }

        let plan = planner.plan(
            query,
            &needle,
            self.fts_available(),
            self.config().candidate_limit,
        );
        let candidate_limit = self.config().candidate_limit;
        let rows: Vec<EntryRow> = self.with_read(|connection| {
            let mut statement = connection.prepare(&plan.sql)?;
            let mapped = statement.query_map(
                rusqlite::params_from_iter(plan.binds.iter()),
                entry_row_from_row,
            )?;
            let mut rows = Vec::new();
            for row in mapped {
                rows.push(row?);
            }
            Ok(rows)
        })?;
        let truncated = plan.bounded && rows.len() > candidate_limit;

        let mut ranked: Vec<(Rank, EntryRow)> = rows
            .into_iter()
            .take(candidate_limit)
            .map(|row| (rank_of(&row, &needle), row))
            .collect();
        sort_hits(&mut ranked, query);

        let max_rows = self.config().max_cursor_rows;
        let total = if truncated {
            None
        } else {
            Some(ranked.len() as u64)
        };
        ranked.truncate(max_rows);

        let cursor = Cursor {
            id: uuid::Uuid::new_v4().to_string(),
            generation,
            roots: if query.roots.is_empty() {
                roots.iter().map(|status| status.root).collect()
            } else {
                query.roots.clone()
            },
            ids: ranked.iter().map(|(_, row)| row.entry_id).collect(),
            ranks: ranked.iter().map(|(rank, _)| *rank).collect(),
            total,
            truncated,
            last_used: Instant::now(),
        };
        let cursor_id = cursor.id.clone();
        let page_size = query.effective_page_size();
        let hits: Vec<Hit> = ranked
            .into_iter()
            .take(page_size)
            .map(|(rank, row)| Hit {
                row,
                rank,
                present: true,
            })
            .collect();

        {
            let config = self.config().clone();
            self.cursors()
                .lock()
                .insert(cursor, config.max_cursors, config.cursor_ttl);
        }

        Ok(SearchResult {
            cursor: cursor_id,
            generation,
            total,
            truncated,
            strategy: plan.strategy.to_owned(),
            roots,
            hits,
        })
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

        let mut hits = Vec::with_capacity(slice.len());
        let mut dropped = 0u32;
        self.with_read(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM entries WHERE entry_id = ?1"
            ))?;
            for (index, entry_id) in slice.iter().enumerate() {
                match statement.query_row([entry_id.0], entry_row_from_row) {
                    Ok(row) => {
                        let present = row.deleted_generation.is_none();
                        if !present {
                            dropped += 1;
                        }
                        hits.push(Hit {
                            row,
                            rank: ranks.get(index).copied().unwrap_or(Rank::Substring),
                            present,
                        });
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => dropped += 1,
                    Err(error) => return Err(IndexError::Sqlite(error)),
                }
            }
            Ok(())
        })?;

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

    fn scoped_roots(&self, query: &Query) -> Result<Vec<RootStatus>> {
        let all = self.roots()?;
        if query.roots.is_empty() {
            return Ok(all);
        }
        Ok(all
            .into_iter()
            .filter(|status| query.roots.contains(&status.root))
            .collect())
    }
}

fn rank_of(row: &EntryRow, needle: &str) -> Rank {
    let name = fold(&row.name.display());
    if name == needle {
        Rank::Exact
    } else if name.starts_with(needle) {
        Rank::Prefix
    } else if name.contains(needle) {
        Rank::Substring
    } else {
        Rank::PathOnly
    }
}

/// Sorts into a **total** order: whatever the caller asked for, ties break on the path, so a page
/// boundary means the same thing on every call.
fn sort_hits(rows: &mut [(Rank, EntryRow)], query: &Query) {
    let descending = query.descending;
    rows.sort_by(|left, right| {
        let ordering = match query.sort {
            Sort::Relevance => left
                .0
                .score()
                .cmp(&right.0.score())
                .then(right.1.meta.mtime_ns.cmp(&left.1.meta.mtime_ns)),
            Sort::Name => left.1.name.cmp(&right.1.name),
            Sort::Path => left.1.path.cmp(&right.1.path),
            Sort::Size => left.1.meta.size_bytes.cmp(&right.1.meta.size_bytes),
            Sort::Mtime => left.1.meta.mtime_ns.cmp(&right.1.meta.mtime_ns),
        };
        let ordering = if descending && query.sort != Sort::Relevance {
            ordering.reverse()
        } else {
            ordering
        };
        ordering.then_with(|| left.1.path.cmp(&right.1.path))
    });
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
