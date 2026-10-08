//! Search over indexed roots — what makes every Home Surface drawer searchable, and what lets
//! a drawer *be* a saved query rather than only a directory.
//!
//! This is now a thin delegate onto [`templar_file_index`], the shared, incrementally maintained
//! index core (`docs/planning/shared-incremental-file-index.md`). Root policy and saved queries
//! stay authoritative in the general session `Store` (`search_roots`, `saved_queries`); entries
//! themselves live in the derived, disposable index-core database. `Entry`/`Hit`/`Rank`/
//! `SavedQuery` and every method signature here are unchanged, so `Files.Search1` and its
//! existing clients see the same JSON they always have — the compatibility surface the plan of
//! record calls for.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use templar_state::Store;

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
    fn from_row(row: &templar_file_index::EntryRow) -> Self {
        Self {
            path: row.path.to_path(),
            is_dir: row.meta.kind == templar_file_index::EntryKind::Directory,
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

impl From<templar_file_index::Rank> for Rank {
    fn from(rank: templar_file_index::Rank) -> Self {
        match rank {
            templar_file_index::Rank::Exact => Self::Exact,
            templar_file_index::Rank::Prefix => Self::Prefix,
            templar_file_index::Rank::Substring => Self::Substring,
            templar_file_index::Rank::PathOnly => Self::PathOnly,
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
    core: templar_file_index::Index,
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
        let core = templar_file_index::Index::open(
            &scratch.path().join("file-index-v1.sqlite3"),
            templar_file_index::IndexConfig::default(),
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
    /// (`$XDG_CACHE_HOME/templaros/file-index-v1.sqlite3` in production), so the index survives a
    /// daemon restart instead of starting empty. `max_entries`/`max_depth` of `usize::MAX` mean
    /// uncapped.
    pub fn with_index_db(
        store: Store,
        index_db_path: &Path,
        config: templar_file_index::IndexConfig,
    ) -> Result<Self> {
        let core = templar_file_index::Index::open(index_db_path, config)?;
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
    pub fn core(&self) -> &templar_file_index::Index {
        &self.core
    }

    /// Adopt pre-policy roots once; afterwards the authoritative list, including an explicitly
    /// empty list, replaces cache configuration. This never walks a filesystem tree.
    pub fn restore_configured_roots(&self, defaults: &[PathBuf]) -> Result<()> {
        use rusqlite::OptionalExtension as _;
        let cached = self.core.roots()?;
        self.store.transaction(|transaction| {
            let initialized = transaction.query_row(
                "SELECT value FROM schema_meta WHERE key = 'file_index_roots_initialized'",
                [], |row| row.get::<_, i64>(0),
            ).optional()?.is_some_and(|value| value == 1);
            if !initialized {
                let mut initial: Vec<(templar_file_index::NativePath, templar_file_index::RootPolicy)> =
                    cached.iter().map(|root| Ok((root.path.clone(), self.core.root_policy(root.root)?)))
                        .collect::<Result<_>>()?;
                if initial.is_empty() {
                    let legacy: Vec<String> = transaction.prepare("SELECT root FROM search_roots ORDER BY root")?
                        .query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
                    initial.extend(legacy.into_iter().map(|path| (
                        templar_file_index::NativePath::from_path(Path::new(&path)), self.policy(),
                    )));
                }
                if initial.is_empty() {
                    initial.extend(defaults.iter().map(|path| (templar_file_index::NativePath::from_path(path), self.policy())));
                }
                for (path, policy) in initial {
                    transaction.execute(
                        "INSERT INTO file_index_roots(path_bytes, policy_json) VALUES (?1, ?2)",
                        rusqlite::params![path.as_bytes(), serde_json::to_string(&policy)?],
                    )?;
                }
                transaction.execute(
                    "INSERT INTO schema_meta(key,value) VALUES ('file_index_roots_initialized',1)",
                    [],
                )?;
            }
            Ok(())
        })?;
        let configured: Vec<(Vec<u8>, String)> = {
            let db = self.store.lock();
            let mut statement = db.prepare("SELECT path_bytes,policy_json FROM file_index_roots ORDER BY path_bytes")?;
            let configured = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            configured
        };
        for old in cached {
            if !configured.iter().any(|(bytes, _)| bytes.as_slice() == old.path.as_bytes()) {
                self.core.forget_root(old.root)?;
            }
        }
        for (bytes, json) in configured {
            let path = templar_file_index::NativePath::from_bytes(bytes);
            let policy: templar_file_index::RootPolicy = serde_json::from_str(&json)?;
            let root = self.core.add_root(&path, policy.clone())?;
            self.core.set_root_policy(root.root, policy)?;
        }
        Ok(())
    }

    /// Internal producers read authoritative policy; native callers carry its observed hash.
    pub fn configured_policy(&self, path: &templar_file_index::NativePath) -> Result<Option<templar_file_index::RootPolicy>> {
        use rusqlite::OptionalExtension as _;
        let json: Option<String> = self.store.lock().query_row(
            "SELECT policy_json FROM file_index_roots WHERE path_bytes=?1", [path.as_bytes()], |row| row.get(0),
        ).optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(Into::into)).transpose()
    }

    pub fn root_policy_fenced(&self, path: &templar_file_index::NativePath, expected_hash: &str) -> Result<templar_file_index::RootPolicy> {
        let db = self.store.lock();
        checked_policy(&db, path, expected_hash, false)?.ok_or_else(|| {
            templar_file_index::IndexError::SnapshotRequired { reason: "root policy disappeared; refresh roots" }.into()
        })
    }

    pub fn configure_root(&self, path: &templar_file_index::NativePath, policy: templar_file_index::RootPolicy, expected_hash: &str) -> Result<templar_file_index::RootStatus> {
        anyhow::ensure!(path.as_bytes().first() == Some(&b'/') && !path.as_bytes().contains(&0),
            "Index roots need an absolute native path without NUL");
        // Keep the authoritative Store guard until its derived publication finishes, so two
        // concurrent settings changes cannot commit policy B then leave cache policy A visible.
        let db = self.store.lock();
        let transaction = db.unchecked_transaction()?;
        checked_policy(&transaction, path, expected_hash, true)?;
        transaction.execute(
            "INSERT INTO file_index_roots(path_bytes,policy_json) VALUES (?1,?2)
             ON CONFLICT(path_bytes) DO UPDATE SET policy_json=excluded.policy_json",
            rusqlite::params![path.as_bytes(), serde_json::to_string(&policy)?],
        )?;
        transaction.execute(
            "INSERT INTO schema_meta(key,value) VALUES ('file_index_roots_initialized',1)
             ON CONFLICT(key) DO UPDATE SET value=1", [],
        )?;
        transaction.commit()?;
        let root = self.core.add_root(path, policy.clone())?;
        Ok(self.core.set_root_policy(root.root, policy)?)
    }

    pub fn forget_root_native(&self, path: &templar_file_index::NativePath, expected_hash: &str) -> Result<Option<templar_file_index::RootId>> {
        let db = self.store.lock();
        let transaction = db.unchecked_transaction()?;
        checked_policy(&transaction, path, expected_hash, false)?;
        let root = self.core.root_by_path(path)?.map(|status| status.root);
        transaction.execute("DELETE FROM file_index_roots WHERE path_bytes=?1", [path.as_bytes()])?;
        if let Some(text) = path.as_path().to_str() {
            transaction.execute("DELETE FROM search_roots WHERE root=?1", [text])?;
        }
        transaction.execute(
            "INSERT INTO schema_meta(key,value) VALUES ('file_index_roots_initialized',1)
             ON CONFLICT(key) DO UPDATE SET value=1", [],
        )?;
        transaction.commit()?;
        if let Some(root) = root { self.core.forget_root(root)?; }
        Ok(root)
    }

    fn policy(&self) -> templar_file_index::RootPolicy {
        let mut policy = templar_file_index::RootPolicy::default_home();
        policy.skip_names.clone_from(&self.skip_names);
        policy
    }

    fn budget(&self) -> templar_file_index::ScanBudget {
        let mut budget = templar_file_index::ScanBudget::default();
        if self.max_entries < usize::MAX {
            budget.max_entries = Some(self.max_entries as u64);
        }
        if self.max_depth < usize::MAX {
            budget.max_depth = Some(self.max_depth as u32);
        }
        budget
    }


    /// The index core's own status for `root`: freshness, completeness and entry count. `None`
    /// when the root has never been registered.
    pub fn root_status(&self, root: &Path) -> Result<Option<templar_file_index::RootStatus>> {
        let native = templar_file_index::NativePath::from_path(root);
        Ok(self.core.root_by_path(&native)?)
    }

    /// Register a root and walk it, under this index's depth/entry budget. A budget trip commits
    /// `complete = false` and leaves the root `Stale` rather than truncating silently — see
    /// [`Index::root_status`]. Re-indexing a root repairs rather than replaces: renamed or removed
    /// files are swept only when the pass was complete (§3 of the shared-index plan).
    pub fn index_root(&self, root: &Path) -> Result<usize> {

        let native = templar_file_index::NativePath::from_path(root);
        let policy = self.policy();
        let expected = self.configured_policy(&native)?.map_or_else(|| "absent".to_owned(), |policy| policy.hash());
        let root_id = self.configure_root(&native, policy.clone(), &expected)?.root;
        let options = templar_file_index::ScanOptions {
            budget: self.budget(),
            policy,
            ..templar_file_index::ScanOptions::default()
        };
        let outcome = self.core.scan_root(root_id, options)?;
        Ok(outcome.entries as usize)
    }

    pub fn roots(&self) -> Result<Vec<PathBuf>> {
        Ok(self.core.roots()?.into_iter().map(|root| root.path.into_path()).collect())
    }

    pub fn forget_root(&self, root: &Path) -> Result<()> {
        let native = templar_file_index::NativePath::from_path(root);
        if let Some(policy) = self.configured_policy(&native)? {
            self.forget_root_native(&native, &policy.hash())?;
            return Ok(());
        }
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
        // `roots` here is any path prefix a caller wants results restricted to (a subdirectory of
        // an indexed root counts), not necessarily a registered index root, so it is the core's
        // subtree scope rather than its root-id scope. Answered in SQL: filtering a global top-N
        // afterwards lost every in-folder match that ranked below N across the whole index.
        let mut core_query =
            templar_file_index::Query::new(needle).with_page_size(limit.clamp(1, 1000) as u32);
        core_query.within = roots
            .iter()
            .map(|root| templar_file_index::NativePath::from_path(root))
            .collect();
        let result = self.core.search(&core_query)?;
        let mut hits: Vec<Hit> = result
            .hits
            .into_iter()
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
            rusqlite::params![saved.name, saved.query, roots, templar_state::now_ms()],
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

fn checked_policy(
    db: &rusqlite::Connection, path: &templar_file_index::NativePath, expected_hash: &str, allow_absent: bool,
) -> Result<Option<templar_file_index::RootPolicy>> {
    use rusqlite::OptionalExtension as _;
    let json: Option<String> = db.query_row(
        "SELECT policy_json FROM file_index_roots WHERE path_bytes=?1", [path.as_bytes()], |row| row.get(0),
    ).optional()?;
    let policy = json.map(|json| serde_json::from_str::<templar_file_index::RootPolicy>(&json)).transpose()?;
    let matches = policy.as_ref().map_or(allow_absent && expected_hash == "absent", |policy| policy.hash() == expected_hash);
    if !matches {
        return Err(templar_file_index::IndexError::SnapshotRequired {
            reason: "root policy revision changed or root disappeared; refresh roots",
        }.into());
    }
    Ok(policy)
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
        // @templaros.decision.independent-storage-roles) is a second filesystem that is not the OS
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
        assert_eq!(status.state, templar_file_index::RootState::Stale);
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
        assert_eq!(status.state, templar_file_index::RootState::Stale);

        index.max_entries = usize::MAX;
        index.index_root(dir.path()).unwrap();
        let status = index.root_status(dir.path()).unwrap().unwrap();
        assert!(status.complete);
    }

    #[test]
    fn authoritative_native_policy_survives_cache_replacement_and_removed_home_stays_removed() {
        use std::os::unix::ffi::OsStringExt as _;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let native = dir.path().join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&native).unwrap();
        let state_path = dir.path().join("session.sqlite3");
        let cache_path = dir.path().join("index.sqlite3");
        let store = Store::open(&state_path).unwrap();
        {
            let index = Index::with_index_db(store.clone(), &cache_path, templar_file_index::IndexConfig::default()).unwrap();
            index.restore_configured_roots(std::slice::from_ref(&home)).unwrap();
            let mut policy = templar_file_index::RootPolicy::default_home();
            policy.monitoring = false;
            policy.cross_mount = true;
            policy.coverage_rules.insert(0, templar_file_index::CoverageRule {
                matcher: templar_file_index::CoverageMatch::Subtree { path: templar_file_index::NativePath::from_path(&native.join("excluded")) },
                coverage: templar_file_index::Coverage::Ignored,
            });
            index.configure_root(&templar_file_index::NativePath::from_path(&native), policy, "absent").unwrap();
            index.forget_root(&home).unwrap();
        }
        std::fs::remove_file(&cache_path).unwrap();
        {
            let restored = Index::with_index_db(store.clone(), &cache_path, templar_file_index::IndexConfig::default()).unwrap();
            restored.restore_configured_roots(std::slice::from_ref(&home)).unwrap();
            assert_eq!(restored.roots().unwrap(), vec![native.clone()]);
            let status = restored.core.root_by_path(&templar_file_index::NativePath::from_path(&native)).unwrap().unwrap();
            let policy = restored.core.root_policy(status.root).unwrap();
            assert!(!policy.monitoring && policy.cross_mount);
            assert_eq!(policy.coverage_rules[0].matcher,
                templar_file_index::CoverageMatch::Subtree { path: templar_file_index::NativePath::from_path(&native.join("excluded")) });
            restored.forget_root(&native).unwrap();
        }
        std::fs::remove_file(&cache_path).unwrap();
        let empty = Index::with_index_db(store, &cache_path, templar_file_index::IndexConfig::default()).unwrap();
        empty.restore_configured_roots(std::slice::from_ref(&home)).unwrap();
        assert!(empty.roots().unwrap().is_empty(), "an initialized empty list must not recreate HOME");
    }
    #[test]
    fn root_mutations_compare_authoritative_native_revision_and_never_target_another_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = templar_file_index::NativePath::from_path(&directory.path().join("a"));
        let other = templar_file_index::NativePath::from_path(&directory.path().join("b"));
        let index = Index::new(Store::in_memory().unwrap());
        let original = templar_file_index::RootPolicy::default_home();
        let root = index.configure_root(&path, original.clone(), "absent").unwrap();
        let updated = templar_file_index::RootPolicy { monitoring: false, ..original.clone() };
        index.configure_root(&path, updated.clone(), &original.hash()).unwrap();
        for error in [
            index.root_policy_fenced(&path, &original.hash()).unwrap_err(),
            index.configure_root(&path, original.clone(), &original.hash()).unwrap_err(),
            index.configure_root(&path, original.clone(), "absent").unwrap_err(),
            index.forget_root_native(&path, &original.hash()).unwrap_err(),
        ] {
            assert!(matches!(error.downcast_ref::<templar_file_index::IndexError>(),
                Some(templar_file_index::IndexError::SnapshotRequired { .. })));
        }
        assert_eq!(index.configured_policy(&path).unwrap(), Some(updated.clone()));
        assert_eq!(index.forget_root_native(&path, &updated.hash()).unwrap(), Some(root.root));
        let replacement = index.configure_root(&other, original.clone(), "absent").unwrap();
        assert!(replacement.root.0 > root.root.0);
        assert!(index.forget_root_native(&path, &updated.hash()).is_err());
        assert_eq!(index.configured_policy(&other).unwrap(), Some(original));
        assert_eq!(index.core.roots().unwrap()[0].path, other);
    }

    #[test]
    fn concurrent_policy_updates_have_exactly_one_durable_revision_winner() {
        let directory = tempfile::tempdir().unwrap();
        let path = templar_file_index::NativePath::from_path(directory.path());
        let index = Index::new(Store::in_memory().unwrap());
        let original = templar_file_index::RootPolicy::default_home();
        index.configure_root(&path, original.clone(), "absent").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let policies = [
            templar_file_index::RootPolicy { monitoring: false, ..original.clone() },
            templar_file_index::RootPolicy { cross_mount: true, ..original.clone() },
        ];
        let workers = policies.into_iter().map(|policy| {
            let (index, path, barrier, expected) = (index.clone(), path.clone(), barrier.clone(), original.hash());
            std::thread::spawn(move || {
                barrier.wait();
                let result = index.configure_root(&path, policy.clone(), &expected);
                (policy, result)
            })
        }).collect::<Vec<_>>();
        barrier.wait();
        let outcomes = workers.into_iter().map(|worker| worker.join().unwrap()).collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|(_, result)| result.is_ok()).count(), 1);
        let winner = &outcomes.iter().find(|(_, result)| result.is_ok()).unwrap().0;
        let loser = outcomes.iter().find(|(_, result)| result.is_err()).unwrap().1.as_ref().unwrap_err();
        assert!(matches!(loser.downcast_ref::<templar_file_index::IndexError>(),
            Some(templar_file_index::IndexError::SnapshotRequired { .. })));
        assert_eq!(index.configured_policy(&path).unwrap().as_ref(), Some(winner));
        let root = index.core.root_by_path(&path).unwrap().unwrap();
        assert_eq!(&index.core.root_policy(root.root).unwrap(), winner, "cache publication lost durable mutation ordering");
    }
}
