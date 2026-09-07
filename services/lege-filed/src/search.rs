//! Search over indexed roots — what makes every Home Surface drawer searchable, and what lets
//! a drawer *be* a saved query rather than only a directory.
//!
//! This is now a thin delegate onto [`lege_file_index`], the shared, incrementally maintained
//! index core (`docs/planning/shared-incremental-file-index.md`). Root policy and saved queries
//! stay authoritative in the general session `Store` (`search_roots`, `saved_queries`); entries
//! themselves live in the derived, disposable index-core database. `Entry`/`Hit`/`Rank`/
//! `SavedQuery` and every method signature here are unchanged, so `Files.Search1` and its
//! existing clients see the same JSON they always have — the compatibility surface the plan of
//! record calls for.

use anyhow::Result;
use lege_state::Store;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// One indexed item. Unchanged shape: this is exactly what `Files.Search1` clients already
/// parse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    pub mime: Option<String>,
}

impl Entry {
    fn from_row(row: &lege_file_index::EntryRow) -> Self {
        Self {
            path: row.path.to_path(),
            is_dir: row.meta.kind == lege_file_index::EntryKind::Directory,
            size_bytes: row.meta.size_bytes,
            mtime_ms: row.meta.mtime_ns / 1_000_000,
            mime: row.meta.mime.clone(),
        }
    }
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

#[cfg(test)]
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

impl From<lege_file_index::Rank> for Rank {
    fn from(rank: lege_file_index::Rank) -> Self {
        match rank {
            lege_file_index::Rank::Exact => Self::Exact,
            lege_file_index::Rank::Prefix => Self::Prefix,
            lege_file_index::Rank::Substring => Self::Substring,
            lege_file_index::Rank::PathOnly => Self::PathOnly,
        }
    }
}

/// A result with the reason it matched, so a UI can group and explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub entry: Entry,
    pub rank: Rank,
}

/// The index: root policy and saved queries in the one per-user state store, entries in the
/// shared index core.
#[derive(Clone, Debug)]
pub struct Index {
    store: Store,
    core: lege_file_index::Index,
    /// Kept alive only when `core` was opened against a private temporary database (tests and
    /// any other caller that did not ask for a durable cache); dropping it deletes the directory.
    _core_scratch: Option<Arc<tempfile::TempDir>>,
    /// Directories never descended into: caches, version-control internals and the trash. Their
    /// contents are noise in every search result a person actually wants.
    pub skip_names: Vec<String>,
    /// Depth budget, so a pathological tree cannot make indexing unbounded.
    pub max_depth: usize,
    /// Entry budget, so a broad tree cannot make indexing unbounded even when it is shallow. A
    /// budget trip no longer truncates silently: the root commits `complete = false` and lands in
    /// `RootState::Stale` (see [`Index::root_status`]).
    pub max_entries: usize,
}

const DEFAULT_SKIP_NAMES: [&str; 8] = [
    ".git",
    ".hg",
    ".svn",
    ".cache",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
];

impl Index {
    /// The state store plus a private, non-durable index core. This is what every existing test
    /// and call site uses; it gives each caller its own isolated core, so parallel tests never
    /// collide. Production code that wants the index to survive a restart uses
    /// [`Index::with_index_db`].
    #[must_use]
    pub fn new(store: Store) -> Self {
        let scratch = tempfile::tempdir().expect("creating a scratch dir for the index core");
        let core = lege_file_index::Index::open(
            &scratch.path().join("file-index-v1.sqlite3"),
            lege_file_index::IndexConfig::default(),
        )
        .expect("opening a fresh file-index core cannot fail");
        Self {
            store,
            core,
            _core_scratch: Some(Arc::new(scratch)),
            skip_names: DEFAULT_SKIP_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            max_depth: 24,
            max_entries: 50_000,
        }
    }

    /// Opens against a durable cache database at `index_db_path`
    /// (`$XDG_CACHE_HOME/legeos/file-index-v1.sqlite3` in production), so the index survives a
    /// daemon restart instead of starting empty. `max_entries`/`max_depth` of `usize::MAX` mean
    /// uncapped.
    pub fn with_index_db(
        store: Store,
        index_db_path: &Path,
        config: lege_file_index::IndexConfig,
    ) -> Result<Self> {
        let core = lege_file_index::Index::open(index_db_path, config)?;
        Ok(Self {
            store,
            core,
            _core_scratch: None,
            skip_names: DEFAULT_SKIP_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            max_depth: usize::MAX,
            max_entries: usize::MAX,
        })
    }

    /// The index core underneath, for the `Index1` D-Bus surface (`index_host.rs`) and anything
    /// else that needs more than this compatibility facade exposes.
    #[must_use]
    pub fn core(&self) -> &lege_file_index::Index {
        &self.core
    }

    fn policy(&self) -> lege_file_index::RootPolicy {
        let mut policy = lege_file_index::RootPolicy::default_home();
        policy.skip_names.clone_from(&self.skip_names);
        policy
    }

    fn budget(&self) -> lege_file_index::ScanBudget {
        let mut budget = lege_file_index::ScanBudget::default();
        if self.max_entries < usize::MAX {
            budget.max_entries = Some(self.max_entries as u64);
        }
        if self.max_depth < usize::MAX {
            budget.max_depth = Some(self.max_depth as u32);
        }
        budget
    }

    /// Registers a root in the index core without scanning it. What a `Files.Search1.IndexRoot`
    /// call answers from immediately, before a background reconcile brings it up to date.
    pub fn ensure_root(&self, root: &Path) -> Result<lege_file_index::RootStatus> {
        let native = lege_file_index::NativePath::from_path(root);
        Ok(self.core.add_root(&native, self.policy())?)
    }

    /// The index core's own status for `root`: freshness, completeness and entry count. `None`
    /// when the root has never been registered.
    pub fn root_status(&self, root: &Path) -> Result<Option<lege_file_index::RootStatus>> {
        let native = lege_file_index::NativePath::from_path(root);
        Ok(self.core.root_by_path(&native)?)
    }

    /// Register a root and walk it, under this index's depth/entry budget. A budget trip commits
    /// `complete = false` and leaves the root `Stale` rather than truncating silently — see
    /// [`Index::root_status`]. Re-indexing a root repairs rather than replaces: renamed or removed
    /// files are swept only when the pass was complete (§3 of the shared-index plan).
    pub fn index_root(&self, root: &Path) -> Result<usize> {
        let key = root.to_string_lossy().into_owned();
        self.store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO search_roots (root, indexed_ms) VALUES (?1, ?2)
                 ON CONFLICT (root) DO UPDATE SET indexed_ms = excluded.indexed_ms",
                rusqlite::params![key, lege_state::now_ms()],
            )?;
            Ok(())
        })?;

        let native = lege_file_index::NativePath::from_path(root);
        let policy = self.policy();
        let root_id = self.core.add_root(&native, policy.clone())?.root;
        let options = lege_file_index::ScanOptions {
            budget: self.budget(),
            policy,
            ..lege_file_index::ScanOptions::default()
        };
        let outcome = self.core.scan_root(root_id, options)?;
        Ok(outcome.entries as usize)
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
        let needle = query.trim();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // Over-fetch, then filter by path prefix in Rust: `roots` here is any path prefix a
        // caller wants results restricted to (a subdirectory of an indexed root counts), not
        // necessarily a registered index root, so this cannot be pushed into the core's own
        // root-scoped `Query.roots`.
        let over_fetch = (limit.saturating_mul(4)).max(limit).clamp(1, 1000) as u32;
        let core_query = lege_file_index::Query::new(needle).with_page_size(over_fetch);
        let result = self.core.search(&core_query)?;
        let mut hits: Vec<Hit> = result
            .hits
            .into_iter()
            .filter(|hit| {
                roots.is_empty()
                    || roots
                        .iter()
                        .any(|root| hit.row.path.to_path().starts_with(root))
            })
            .map(|hit| Hit {
                entry: Entry::from_row(&hit.row),
                rank: Rank::from(hit.rank),
            })
            .collect();
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

#[cfg(test)]
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
    fn a_trashed_file_stops_turning_up_in_search() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Both shapes the specification allows. The skip list could express neither: the home
        // trash has no leading dot, and a volume trash's uid suffix differs per machine.
        std::fs::create_dir_all(root.join("Trash/files")).unwrap();
        std::fs::create_dir_all(root.join(".Trash-1000/files")).unwrap();
        std::fs::write(root.join("Trash/files/report.pdf"), b"deleted").unwrap();
        std::fs::write(root.join(".Trash-1000/files/report.pdf"), b"deleted").unwrap();
        std::fs::write(root.join("report.pdf"), b"kept").unwrap();

        let index = Index::new(Store::in_memory().unwrap());
        index.index_root(root).unwrap();

        let paths: Vec<PathBuf> = index
            .search("report", &[], 50)
            .unwrap()
            .into_iter()
            .map(|hit| hit.entry.path)
            .collect();
        assert_eq!(
            paths,
            [root.join("report.pdf")],
            "the trash leaked into the index"
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
    fn two_roots_keep_independent_freshness_and_scoped_search() {
        // The data role (`distro/deploy/layout.toml [data]`,
        // @legeos.decision.independent-storage-roles) is a second filesystem that is not the OS
        // and is never assumed expendable. The index has to hold it as a root of its own: a data
        // disk that goes stale must not drag `$HOME` down with it, and a search bounded to one
        // must not spill rows from the other.
        //
        // No new plumbing was needed for this — `ensure_root` already registers an arbitrary root
        // with its own policy. What was missing was anything saying so, which is what fails if
        // the two ever start sharing state.
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("household budget.ods"), b"home").unwrap();
        std::fs::write(data.path().join("archive budget.ods"), b"data").unwrap();

        let index = Index::new(Store::in_memory().unwrap());
        index.index_root(home.path()).unwrap();
        let home_alone = index
            .root_status(home.path())
            .unwrap()
            .expect("home")
            .entry_count;
        index.index_root(data.path()).unwrap();

        let mut roots = index.roots().unwrap();
        roots.sort();
        let mut expected = vec![home.path().to_path_buf(), data.path().to_path_buf()];
        expected.sort();
        assert_eq!(roots, expected, "both roots are registered independently");

        // Freshness is per root: each has its own committed generation and its own state.
        let home_status = index.root_status(home.path()).unwrap().expect("home");
        let data_status = index.root_status(data.path()).unwrap().expect("data");
        assert_ne!(
            home_status.root, data_status.root,
            "the two roots share one row, so one going stale would take the other with it"
        );
        assert_eq!(
            home_status.entry_count, home_alone,
            "indexing the data root changed the home root's own count, so the two share rows"
        );
        assert!(data_status.entry_count > 0, "the data root indexed nothing");

        // Search is scoped: the same query bounded to one root returns only that root's row.
        let both = index.search("budget", &[], 10).unwrap();
        assert_eq!(both.len(), 2, "unscoped, both roots answer");

        let only_home = index
            .search("budget", &[home.path().to_path_buf()], 10)
            .unwrap();
        assert_eq!(only_home.len(), 1);
        assert_eq!(file_name(&only_home[0].entry.path), "household budget.ods");

        let only_data = index
            .search("budget", &[data.path().to_path_buf()], 10)
            .unwrap();
        assert_eq!(only_data.len(), 1);
        assert_eq!(file_name(&only_data[0].entry.path), "archive budget.ods");

        // And forgetting one leaves the other exactly as it was: a data disk that is unplugged
        // does not cost the home root its index.
        index.forget_root(data.path()).unwrap();
        assert_eq!(index.roots().unwrap(), vec![home.path().to_path_buf()]);
        assert_eq!(
            index
                .search("budget", &[home.path().to_path_buf()], 10)
                .unwrap()
                .len(),
            1
        );
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
    fn a_depth_budget_yields_incomplete() {
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
        let status = index.root_status(dir.path()).unwrap().unwrap();
        assert!(
            !status.complete,
            "a depth-budgeted pass reported itself complete"
        );
        assert_eq!(status.state, lege_file_index::RootState::Stale);
        assert!(index.search("buried", &[], 10).unwrap().is_empty());

        index.max_depth = 24;
        index.index_root(dir.path()).unwrap();
        let status = index.root_status(dir.path()).unwrap().unwrap();
        assert!(status.complete);
        assert_eq!(index.search("buried", &[], 10).unwrap().len(), 1);
    }

    #[test]
    fn an_entry_budget_yields_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        for child in 0..20 {
            std::fs::write(dir.path().join(format!("file-{child}")), b"x").unwrap();
        }
        let mut index = Index::new(Store::in_memory().unwrap());
        index.max_entries = 5;

        index.index_root(dir.path()).unwrap();
        let status = index.root_status(dir.path()).unwrap().unwrap();
        assert!(
            !status.complete,
            "an entry-budgeted pass reported itself complete"
        );
        assert_eq!(status.state, lege_file_index::RootState::Stale);

        index.max_entries = usize::MAX;
        index.index_root(dir.path()).unwrap();
        let status = index.root_status(dir.path()).unwrap().unwrap();
        assert!(status.complete);
    }
}
