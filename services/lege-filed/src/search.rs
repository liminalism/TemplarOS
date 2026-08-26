//! Search over indexed roots — what makes every Home Surface drawer searchable, and what lets
//! a drawer *be* a saved query rather than only a directory.
//!
//! Deliberately an index of names, paths and metadata, not of content. Content indexing is a
//! daemon that reads every file the user owns, and the first release has not earned that: it
//! costs I/O, battery and a much larger privacy surface, and the drawers the plan describes —
//! Recent, Downloads, Documents, Drives — are answered by names and metadata. The schema leaves
//! room for content later; the policy does not pretend to have it now.

use anyhow::Result;
use lege_state::Store;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// One indexed item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub mime: Option<String>,
}

/// A saved query, which a drawer can be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQuery {
    pub name: String,
    pub query: String,
    pub roots: Vec<PathBuf>,
}

/// How a query is matched. Substring on a case-folded name is what a user means by "search" in
/// a file browser; the ordering is what makes it feel right.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rank {
    /// The whole name, exactly.
    Exact,
    /// The name starts with the query.
    Prefix,
    /// The query appears in the name.
    Substring,
    /// The query appears in the path but not the name.
    PathOnly,
}

impl Rank {
    fn score(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Prefix => 1,
            Self::Substring => 2,
            Self::PathOnly => 3,
        }
    }
}

/// A result with the reason it matched, so a UI can group and explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub entry: Entry,
    pub rank: Rank,
}

/// The index, in the one per-user state store.
#[derive(Clone, Debug)]
pub struct Index {
    store: Store,
    /// Directories never descended into: caches, version-control internals and the trash. Their
    /// contents are noise in every search result a person actually wants.
    pub skip_names: Vec<String>,
    /// Depth cap, so a pathological tree cannot make indexing unbounded.
    pub max_depth: usize,
    /// Entry cap, so a broad tree cannot make indexing unbounded even when it is shallow.
    pub max_entries: usize,
}

impl Index {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            skip_names: [
                ".git",
                ".hg",
                ".svn",
                ".cache",
                "node_modules",
                "target",
                "__pycache__",
                ".Trash",
                ".venv",
            ]
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
            max_depth: 24,
            max_entries: 50_000,
        }
    }

    /// Register a root and walk it. Re-indexing a root replaces its entries, so a rename cannot
    /// leave a ghost behind.
    pub fn index_root(&self, root: &Path) -> Result<usize> {
        let key = root.to_string_lossy().into_owned();
        self.store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO search_roots (root, indexed_ms) VALUES (?1, ?2)
                 ON CONFLICT (root) DO UPDATE SET indexed_ms = excluded.indexed_ms",
                rusqlite::params![key, lege_state::now_ms()],
            )?;
            transaction.execute("DELETE FROM search_entries WHERE root = ?1", [&key])?;

            let mut count = 0;
            let mut statement = transaction.prepare(
                "INSERT OR REPLACE INTO search_entries
                   (path, root, name_fold, is_dir, size_bytes, mtime_ms, mime)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            )?;
            let mut remaining = self.max_entries;
            walk(
                root,
                0,
                self.max_depth,
                &mut remaining,
                &self.skip_names,
                &mut |entry| {
                    statement.execute(rusqlite::params![
                        entry.path.to_string_lossy(),
                        key,
                        fold(&file_name(&entry.path)),
                        i64::from(entry.is_dir),
                        entry.size_bytes as i64,
                        entry.mtime_ms,
                    ])?;
                    count += 1;
                    Ok(())
                },
            )?;
            Ok(count)
        })
    }

    pub fn roots(&self) -> Result<Vec<PathBuf>> {
        let store = self.store.lock();
        let mut statement = store.prepare("SELECT root FROM search_roots ORDER BY root")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).map(PathBuf::from).collect())
    }

    pub fn forget_root(&self, root: &Path) -> Result<()> {
        self.store.lock().execute(
            "DELETE FROM search_roots WHERE root = ?1",
            [root.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Search the index. An empty query matches nothing rather than everything: a drawer that
    /// dumps the whole filesystem when a search box is cleared is a bug, not a feature.
    pub fn search(&self, query: &str, roots: &[PathBuf], limit: usize) -> Result<Vec<Hit>> {
        let needle = fold(query.trim());
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let store = self.store.lock();
        let mut statement = store.prepare(
            "SELECT path, is_dir, size_bytes, mtime_ms, mime, name_fold FROM search_entries
              WHERE name_fold LIKE '%' || ?1 || '%' ESCAPE '\\'
                 OR path LIKE '%' || ?1 || '%' ESCAPE '\\'
              ORDER BY mtime_ms DESC
              LIMIT ?2",
        )?;
        let rows = statement.query_map(
            rusqlite::params![escape_like(&needle), (limit * 4).max(limit) as i64],
            |row| {
                Ok((
                    Entry {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        is_dir: row.get::<_, i64>(1)? != 0,
                        size_bytes: row.get::<_, i64>(2)? as u64,
                        mtime_ms: row.get(3)?,
                        mime: row.get(4)?,
                    },
                    row.get::<_, String>(5)?,
                ))
            },
        )?;

        let mut hits: Vec<Hit> = rows
            .filter_map(Result::ok)
            .filter(|(entry, _)| {
                roots.is_empty() || roots.iter().any(|root| entry.path.starts_with(root))
            })
            .map(|(entry, name_fold)| {
                let rank = if name_fold == needle {
                    Rank::Exact
                } else if name_fold.starts_with(&needle) {
                    Rank::Prefix
                } else if name_fold.contains(&needle) {
                    Rank::Substring
                } else {
                    Rank::PathOnly
                };
                Hit { entry, rank }
            })
            .collect();

        // Best match first, then most recently modified — the two things a person scanning a
        // result list actually uses.
        hits.sort_by(|left, right| {
            left.rank
                .score()
                .cmp(&right.rank.score())
                .then(right.entry.mtime_ms.cmp(&left.entry.mtime_ms))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    pub fn save_query(&self, saved: &SavedQuery) -> Result<()> {
        let roots = serde_json::to_string(&saved.roots)?;
        self.store.lock().execute(
            "INSERT INTO saved_queries (name, query, roots, at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (name) DO UPDATE SET query = excluded.query, roots = excluded.roots",
            rusqlite::params![saved.name, saved.query, roots, lege_state::now_ms()],
        )?;
        Ok(())
    }

    pub fn saved_queries(&self) -> Result<Vec<SavedQuery>> {
        let store = self.store.lock();
        let mut statement =
            store.prepare("SELECT name, query, roots FROM saved_queries ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            Ok(SavedQuery {
                name: row.get(0)?,
                query: row.get(1)?,
                roots: serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }
}

fn walk(
    path: &Path,
    depth: usize,
    max_depth: usize,
    remaining: &mut usize,
    skip: &[String],
    visit: &mut impl FnMut(&Entry) -> Result<()>,
) -> Result<()> {
    if *remaining == 0 {
        return Ok(());
    }
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };
    // Links are indexed as themselves and never followed: following them turns an index into a
    // graph traversal with cycles.
    let is_dir = metadata.is_dir() && !metadata.file_type().is_symlink();
    visit(&Entry {
        path: path.to_path_buf(),
        is_dir,
        size_bytes: if is_dir { 0 } else { metadata.len() },
        mtime_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_millis() as i64),
        mime: None,
    })?;
    *remaining -= 1;

    if !is_dir || depth >= max_depth {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|child| !skip.iter().any(|name| child.ends_with(name)))
        .collect();
    children.sort();
    for child in children {
        if *remaining == 0 {
            break;
        }
        walk(&child, depth + 1, max_depth, remaining, skip, visit)?;
    }
    Ok(())
}

/// Case-fold for matching. ASCII lowercasing plus Unicode lowercasing covers the names people
/// actually search for without pulling in a full collation table.
fn fold(value: &str) -> String {
    value.to_lowercase()
}

/// `LIKE` treats `%` and `_` as wildcards; a file called `report_final` must not match
/// `reportXfinal`.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("notes/2026")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join("report.pdf"), b"a").unwrap();
        std::fs::write(root.join("notes/report-draft.md"), b"bb").unwrap();
        std::fs::write(root.join("notes/2026/annual report.md"), b"ccc").unwrap();
        std::fs::write(root.join("report_final"), b"dddd").unwrap();
        std::fs::write(root.join(".git/objects/report"), b"noise").unwrap();
        let index = Index::new(Store::in_memory().unwrap());
        index.index_root(root).unwrap();
        (dir, index)
    }

    #[test]
    fn results_are_ranked_exact_prefix_substring_then_path() {
        let (dir, index) = tree();
        let hits = index.search("report", &[], 10).unwrap();
        let names: Vec<String> = hits.iter().map(|hit| file_name(&hit.entry.path)).collect();
        assert!(names.contains(&"report.pdf".to_owned()));
        assert!(names.contains(&"report-draft.md".to_owned()));
        assert!(names.contains(&"annual report.md".to_owned()));
        assert_eq!(hits[0].rank, Rank::Prefix, "{names:?}");
        // Ranks never regress down the list.
        for pair in hits.windows(2) {
            assert!(pair[0].rank.score() <= pair[1].rank.score());
        }
        drop(dir);
    }

    #[test]
    fn skipped_directories_are_not_indexed() {
        let (dir, index) = tree();
        let hits = index.search("report", &[], 50).unwrap();
        assert!(
            hits.iter()
                .all(|hit| !hit.entry.path.starts_with(dir.path().join(".git"))),
            "the .git tree leaked into the index"
        );
    }

    #[test]
    fn like_wildcards_in_a_query_are_literal() {
        let (dir, index) = tree();
        let hits = index.search("report_", &[], 10).unwrap();
        let names: Vec<String> = hits.iter().map(|hit| file_name(&hit.entry.path)).collect();
        assert_eq!(names, ["report_final"], "underscore matched as a wildcard");

        assert!(
            index.search("%", &[], 10).unwrap().is_empty(),
            "a bare percent sign matched everything"
        );
        drop(dir);
    }

    #[test]
    fn searching_is_case_insensitive_and_an_empty_query_matches_nothing() {
        let (dir, index) = tree();
        assert!(!index.search("REPORT", &[], 10).unwrap().is_empty());
        assert!(!index.search("Annual", &[], 10).unwrap().is_empty());
        assert!(index.search("   ", &[], 10).unwrap().is_empty());
        drop(dir);
    }

    #[test]
    fn a_search_can_be_restricted_to_a_root() {
        let (dir, index) = tree();
        let hits = index
            .search("report", &[dir.path().join("notes")], 10)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits.iter()
                .all(|hit| hit.entry.path.starts_with(dir.path().join("notes")))
        );
    }

    #[test]
    fn reindexing_removes_entries_for_deleted_files() {
        let (dir, index) = tree();
        std::fs::remove_file(dir.path().join("report.pdf")).unwrap();
        index.index_root(dir.path()).unwrap();
        let names: Vec<String> = index
            .search("report", &[], 50)
            .unwrap()
            .iter()
            .map(|hit| file_name(&hit.entry.path))
            .collect();
        assert!(!names.contains(&"report.pdf".to_owned()));
    }

    #[test]
    fn roots_and_saved_queries_persist() {
        let (dir, index) = tree();
        assert_eq!(index.roots().unwrap(), vec![dir.path().to_path_buf()]);

        let saved = SavedQuery {
            name: "Reports".into(),
            query: "report".into(),
            roots: vec![dir.path().to_path_buf()],
        };
        index.save_query(&saved).unwrap();
        assert_eq!(index.saved_queries().unwrap(), vec![saved]);

        index.forget_root(dir.path()).unwrap();
        assert!(index.roots().unwrap().is_empty());
    }

    #[test]
    fn callers_can_detect_an_existing_root_without_reindexing_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kept.txt"), b"old").unwrap();
        let index = Index::new(Store::in_memory().unwrap());
        index.index_root(dir.path()).unwrap();

        assert!(index.roots().unwrap().contains(&dir.path().to_path_buf()));
        std::fs::write(dir.path().join("new.txt"), b"new").unwrap();
        assert!(index.search("new.txt", &[], 10).unwrap().is_empty());
    }

    #[test]
    fn a_depth_cap_bounds_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for level in 0..10 {
            deep = deep.join(format!("level{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("buried.txt"), b"x").unwrap();

        let mut index = Index::new(Store::in_memory().unwrap());
        index.max_depth = 3;
        index.index_root(dir.path()).unwrap();
        assert!(index.search("buried", &[], 10).unwrap().is_empty());

        index.max_depth = 24;
        index.index_root(dir.path()).unwrap();
        assert_eq!(index.search("buried", &[], 10).unwrap().len(), 1);
    }

    #[test]
    fn an_entry_cap_bounds_broad_indexing() {
        let dir = tempfile::tempdir().unwrap();
        for child in 0..20 {
            std::fs::write(dir.path().join(format!("file-{child}")), b"x").unwrap();
        }
        let mut index = Index::new(Store::in_memory().unwrap());
        index.max_entries = 5;

        assert_eq!(index.index_root(dir.path()).unwrap(), 5);
        assert_eq!(index.search("file-", &[], 100).unwrap().len(), 4);
    }
}
