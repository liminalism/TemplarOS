//! Opening the derived cache never destroys it on an operational failure.
//!
//! The cache is rebuildable, but "rebuildable" is not a licence to delete it whenever an open
//! fails: a busy owner, a read-only file or a full disk are not corruption, and removing a live
//! owner's `-wal` loses rows it already committed. Only proven corruption or an incompatible
//! format is quarantined — moved aside with its sidecars, never deleted in place.

use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};
use templar_file_index::{
    Index, IndexConfig, IndexError, NativePath, RootPolicy, ScanOptions, error::OpenFailure,
};

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Main file and write-ahead log byte for byte, and whether the shared-memory index exists; an
/// absent file is `None`. The `-shm` content is a cache SQLite may rebuild on any open, so only its
/// deletion would be a loss.
fn files(path: &Path) -> (Option<Vec<u8>>, Option<Vec<u8>>, bool) {
    (
        std::fs::read(path).ok(),
        std::fs::read(sidecar(path, "-wal")).ok(),
        sidecar(path, "-shm").exists(),
    )
}

fn assert_untouched(path: &Path, before: &(Option<Vec<u8>>, Option<Vec<u8>>, bool)) {
    let after = files(path);
    assert!(after.0 == before.0, "the main database changed");
    assert!(after.1 == before.1, "the write-ahead log changed");
    assert!(after.2 || !before.2, "the shared-memory index was deleted");
}

fn populated(dir: &Path) -> (Index, PathBuf) {
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("kept.txt"), b"kept").unwrap();
    let path = dir.join("cache").join("file-index-v1.sqlite3");
    let index = Index::open(&path, IndexConfig::default()).unwrap();
    let root = index
        .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
        .unwrap()
        .root;
    index.scan_root(root, ScanOptions::default()).unwrap();
    (index, path)
}

fn quarantines(path: &Path) -> Vec<PathBuf> {
    let base = path.parent().unwrap().join("file-index-quarantine");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&base)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn unavailable(outcome: Result<Index, IndexError>) -> OpenFailure {
    match outcome {
        Err(IndexError::Unavailable { reason }) => reason,
        Err(other) => panic!("expected Unavailable, got {other}"),
        Ok(_) => panic!("expected Unavailable, got an open index"),
    }
}

#[test]
fn a_busy_cache_is_left_untouched_and_reported_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, path) = populated(dir.path());
    let before = files(&path);
    assert!(
        before.1.is_some(),
        "the owner's write-ahead log is the thing at risk"
    );

    let reason = unavailable(Index::open(&path, IndexConfig::default()));
    assert!(matches!(reason, OpenFailure::Busy), "{reason:?}");
    assert_untouched(&path, &before);
    assert!(quarantines(&path).is_empty());
    drop(owner);
}

#[test]
fn an_inaccessible_cache_parent_leaves_the_database_and_sidecars_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, path) = populated(dir.path());
    drop(owner);
    let cache = path.parent().unwrap();
    std::fs::write(sidecar(&path, "-wal"), b"owned retained wal").unwrap();
    std::fs::write(sidecar(&path, "-shm"), b"owned retained shm").unwrap();
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::read(sidecar(&path, "-wal")).unwrap(),
        std::fs::read(sidecar(&path, "-shm")).unwrap(),
    );
    std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o000)).unwrap();
    let outcome = Index::open(&path, IndexConfig::default());
    // Restore access before inspecting the exact owned fixture, including on assertion failure.
    std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        unavailable(outcome),
        OpenFailure::Environment { .. }
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before.0);
    assert_eq!(std::fs::read(sidecar(&path, "-wal")).unwrap(), before.1);
    assert_eq!(std::fs::read(sidecar(&path, "-shm")).unwrap(), before.2);
    assert!(quarantines(&path).is_empty());
}

#[test]
fn an_unwritable_cache_directory_never_deletes_the_database_or_its_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, path) = populated(dir.path());
    drop(owner);
    let cache = path.parent().unwrap().to_path_buf();
    // A sidecar left by an unclean stop, which a rename-or-delete recovery used to remove.
    std::fs::write(sidecar(&path, "-wal"), b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o555)).unwrap();
    let before = files(&path);

    let outcome = Index::open(&path, IndexConfig::default());
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();
    let reason = unavailable(outcome);
    assert!(
        matches!(reason, OpenFailure::Environment { .. }),
        "{reason:?}"
    );
    assert_untouched(&path, &before);
}

#[test]
fn a_read_only_cache_file_is_reported_unavailable_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, path) = populated(dir.path());
    drop(owner);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let before = files(&path);

    let reason = unavailable(Index::open(&path, IndexConfig::default()));
    assert!(
        matches!(reason, OpenFailure::Environment { .. }),
        "{reason:?}"
    );
    assert_untouched(&path, &before);
    assert!(quarantines(&path).is_empty());
}

#[test]
fn a_corrupt_cache_is_quarantined_with_its_sidecars_and_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, path) = populated(dir.path());
    drop(owner);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[..4096].fill(0xA5);
    std::fs::write(&path, &bytes).unwrap();
    std::fs::write(sidecar(&path, "-wal"), b"stale wal").unwrap();

    let index = Index::open(&path, IndexConfig::default()).expect("a rebuilt cache");
    assert!(index.roots().unwrap().is_empty());
    let moved = quarantines(&path);
    assert_eq!(moved.len(), 1);
    let kept = moved[0].join("file-index-v1.sqlite3");
    assert_eq!(std::fs::read(&kept).unwrap(), bytes);
    assert_eq!(std::fs::read(sidecar(&kept, "-wal")).unwrap(), b"stale wal");
    let reason = std::fs::read_to_string(moved[0].join("reason.txt")).unwrap();
    assert!(reason.contains("corrupt"), "{reason}");
}

#[test]
fn a_newer_schema_is_quarantined_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (owner, path) = populated(dir.path());
    drop(owner);
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO index_migrations (name, applied_ms) VALUES ('9999-from-the-future', 0)",
                [],
            )
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    Index::open(&path, IndexConfig::default()).expect("a rebuilt cache");
    let moved = quarantines(&path);
    assert_eq!(moved.len(), 1);
    assert_eq!(
        std::fs::read(moved[0].join("file-index-v1.sqlite3")).unwrap(),
        before
    );
    let reason = std::fs::read_to_string(moved[0].join("reason.txt")).unwrap();
    assert!(reason.contains("incompatible"), "{reason}");
}

#[test]
fn only_two_quarantines_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache").join("file-index-v1.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    for round in 0..4u8 {
        std::fs::write(&path, vec![round; 8192]).unwrap();
        drop(Index::open(&path, IndexConfig::default()).expect("a rebuilt cache"));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let moved = quarantines(&path);
    assert_eq!(moved.len(), 2, "{moved:?}");
    let newest = std::fs::read(moved[1].join("file-index-v1.sqlite3")).unwrap();
    assert_eq!(newest, vec![3u8; 8192]);
}
