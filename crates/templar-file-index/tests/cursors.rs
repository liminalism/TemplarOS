//! A cursor belongs to the caller that opened it and pins tombstones only while it is alive.

use std::{path::Path, sync::Arc, time::Duration};
use templar_file_index::{
    CallContext, Index, IndexConfig, IndexError, NativePath, Query, RootId, RootPolicy, ScanOptions,
};

fn open(dir: &Path, config: IndexConfig) -> Index {
    Index::open(&dir.join("index.sqlite3"), config).unwrap()
}

fn tree_with(dir: &Path, names: &[&str]) -> std::path::PathBuf {
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    for name in names {
        std::fs::write(tree.join(name), b"x").unwrap();
    }
    tree
}

fn scanned(index: &Index, tree: &Path) -> RootId {
    let root = index
        .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
        .unwrap()
        .root;
    index.scan_root(root, ScanOptions::default()).unwrap();
    root
}

fn owner(name: &str) -> CallContext {
    CallContext::for_owner(Some(Arc::from(name)))
}

fn tombstones(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE deleted_generation IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn an_expired_cursor_no_longer_pins_tombstones_at_the_next_commit() {
    for (ttl, survives) in [
        (Duration::from_secs(300), true),
        (Duration::from_millis(1), false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let tree = tree_with(dir.path(), &["doomed.txt", "kept.txt"]);
        let index = open(
            dir.path(),
            IndexConfig {
                cursor_ttl: ttl,
                tombstone_retention_generations: 1,
                ..IndexConfig::default()
            },
        );
        let root = scanned(&index, &tree);
        let pinned = index
            .search(&Query::new("txt"), &CallContext::internal())
            .unwrap();
        assert!(!pinned.cursor.is_empty());
        std::fs::remove_file(tree.join("doomed.txt")).unwrap();
        index.scan_root(root, ScanOptions::default()).unwrap();
        assert_eq!(tombstones(&dir.path().join("index.sqlite3")), 1);
        std::thread::sleep(Duration::from_millis(5));
        // Nothing touches the cursor between these commits: only expiry can release it.
        for _ in 0..3 {
            index.scan_root(root, ScanOptions::default()).unwrap();
        }
        let left = tombstones(&dir.path().join("index.sqlite3"));
        assert_eq!(left == 1, survives, "ttl {ttl:?}: {left} tombstones left");
    }
}

#[test]
fn a_departed_caller_releases_every_cursor_it_owned() {
    let dir = tempfile::tempdir().unwrap();
    let tree = tree_with(dir.path(), &["one.txt", "two.txt"]);
    let index = open(dir.path(), IndexConfig::default());
    scanned(&index, &tree);
    let leaving = owner(":1.7");
    for _ in 0..3 {
        index.search(&Query::new("txt"), &leaving).unwrap();
    }
    let staying = index.search(&Query::new("txt"), &owner(":1.8")).unwrap();
    assert_eq!(index.diagnostics().unwrap().cursor_count, 4);
    assert_eq!(index.release_owner(":1.7"), 3);
    assert_eq!(index.diagnostics().unwrap().cursor_count, 1);
    index
        .fetch_page(&staying.cursor, 0, 1, &owner(":1.8"))
        .expect("another caller's cursor survives");
}

#[test]
fn one_caller_cannot_evict_another_callers_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let tree = tree_with(dir.path(), &["one.txt", "two.txt"]);
    let index = open(
        dir.path(),
        IndexConfig {
            max_cursors_per_owner: 2,
            ..IndexConfig::default()
        },
    );
    scanned(&index, &tree);
    let quiet = index.search(&Query::new("txt"), &owner(":1.2")).unwrap();
    let busy = owner(":1.3");
    let mut opened = Vec::new();
    for _ in 0..6 {
        opened.push(index.search(&Query::new("txt"), &busy).unwrap().cursor);
    }
    index
        .fetch_page(&quiet.cursor, 0, 1, &owner(":1.2"))
        .expect("a busy neighbour evicted a quiet caller's cursor");
    assert!(index.fetch_page(&opened[5], 0, 1, &busy).is_ok());
    assert!(index.fetch_page(&opened[4], 0, 1, &busy).is_ok());
    assert!(matches!(
        index.fetch_page(&opened[0], 0, 1, &busy),
        Err(IndexError::SnapshotRequired { .. })
    ));
    assert_eq!(index.diagnostics().unwrap().cursor_count, 3);
}

#[test]
fn another_callers_cursor_reads_as_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let tree = tree_with(dir.path(), &["one.txt"]);
    let index = open(dir.path(), IndexConfig::default());
    scanned(&index, &tree);
    let mine = owner(":1.4");
    let theirs = owner(":1.5");
    let result = index.search(&Query::new("one"), &mine).unwrap();
    let foreign = index.fetch_page(&result.cursor, 0, 1, &theirs).unwrap_err();
    let missing = index
        .fetch_page("no-such-cursor", 0, 1, &theirs)
        .unwrap_err();
    assert_eq!(
        foreign.to_string(),
        missing.to_string(),
        "the answer reveals the cursor exists"
    );
    index.close_cursor(&result.cursor, &theirs);
    assert!(
        index.fetch_page(&result.cursor, 0, 1, &mine).is_ok(),
        "a stranger closed it"
    );
}
