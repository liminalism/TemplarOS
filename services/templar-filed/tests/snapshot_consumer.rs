use std::{os::unix::ffi::OsStringExt as _, path::Path};
use templar_client::tree_source::decode_snapshot_tree;
use templar_file_index::{
    Coverage, Index, IndexConfig, NativePath, RootId, RootPolicy, ScanOptions, StaleReason,
    query::FieldMask,
    reconcile::{self, Trigger},
    snapshot::{SnapshotDetail, SnapshotRequest},
    watch::{FakeSource, WatchBudget, WatchOwner},
};

fn frame(
    index: &Index,
    subtree: &Path,
) -> (Vec<u8>, templar_file_index::snapshot::SnapshotManifest) {
    frame_view(index, subtree, SnapshotDetail::Materialized)
}

fn frame_view(
    index: &Index,
    subtree: &Path,
    detail: SnapshotDetail,
) -> (Vec<u8>, templar_file_index::snapshot::SnapshotManifest) {
    let request = SnapshotRequest {
        fields: FieldMask::PATH
            .with(FieldMask::KIND)
            .with(FieldMask::SIZE)
            .with(FieldMask::COVERAGE)
            .with(FieldMask::AGGREGATE),
        detail,
        subtree: Some(NativePath::from_path(subtree)),
        max_depth: (detail == SnapshotDetail::Summary).then_some(3),
        include_coverage: vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed],
        ..SnapshotRequest::default()
    };
    let mut bytes = Vec::new();
    let manifest = index
        .write_snapshot(
            &mut std::io::Cursor::new(&mut bytes),
            &request,
            &templar_file_index::snapshot::SnapshotLimits::default(),
            &templar_file_index::CallContext::internal(),
        )
        .unwrap();
    (bytes, manifest)
}

#[test]
fn nested_registered_roots_decode_once_with_their_native_paths() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let nested = home.join(std::ffi::OsString::from_vec(b"nested-\xff".to_vec()));
    std::fs::create_dir_all(&nested).unwrap();
    let native = nested.join(std::ffi::OsString::from_vec(b"file-\xfe.txt".to_vec()));
    std::fs::write(&native, b"nested content").unwrap();
    std::fs::write(home.join("parent.txt"), b"parent content").unwrap();
    let index = Index::open(&dir.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
    let parent = index
        .add_root(&NativePath::from_path(&home), RootPolicy::default_home())
        .unwrap()
        .root;
    assert!(
        index
            .scan_root(parent, ScanOptions::default())
            .unwrap()
            .complete
    );
    let child = index
        .add_root(&NativePath::from_path(&nested), RootPolicy::default_home())
        .unwrap()
        .root;
    assert!(
        index
            .scan_root(child, ScanOptions::default())
            .unwrap()
            .complete
    );
    assert!(
        index
            .scan_root(parent, ScanOptions::reconcile(RootPolicy::default_home()))
            .unwrap()
            .complete
    );

    let (bytes, manifest) = frame(&index, &home);
    let mut load = decode_snapshot_tree(&bytes, &manifest, &home).unwrap();
    assert!(load.complete);
    assert!(load.entries.iter().any(|entry| entry.path == native));
    let paths: std::collections::HashSet<_> =
        load.entries.iter().map(|entry| &entry.path).collect();
    assert_eq!(
        paths.len(),
        load.entries.len(),
        "a native path appeared in both roots"
    );
    let scanned = templar_treemap::scan(&home, templar_treemap::ScanLimits::default());
    assert!(scanned.complete);
    let mut expected = scanned.entries;
    expected.sort_by(|a, b| a.path.cmp(&b.path));
    load.entries.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(load.entries, expected);
}

#[test]
fn client_consumes_real_writer_frames_with_native_paths_subtree_freshness_and_cached_totals() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("notes")).unwrap();
    std::fs::write(home.join("report.pdf"), vec![0u8; 111]).unwrap();
    std::fs::write(home.join("notes/one.md"), vec![0u8; 22]).unwrap();
    let native = home.join("notes").join(std::ffi::OsString::from_vec(
        b"native-\xff ; $(command)".to_vec(),
    ));
    std::fs::write(&native, vec![0u8; 33]).unwrap();
    let index = Index::open(&dir.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
    let policy = RootPolicy::default_home();
    let root = index
        .add_root(&NativePath::from_path(&home), policy.clone())
        .unwrap()
        .root;
    assert!(
        index
            .scan_root(root, ScanOptions::reconcile(policy.clone()))
            .unwrap()
            .complete
    );

    assert_materialized_native_paths(&index, &home, &native);

    let notes = home.join("notes");
    assert_current_subtree(&index, &notes, root);

    std::fs::create_dir_all(home.join("target/deep")).unwrap();
    std::fs::write(home.join("target/deep/object.o"), vec![0u8; 17]).unwrap();
    let mut owner = WatchOwner::new(FakeSource::default(), WatchBudget { max_watches: 100 });
    owner.track_root(root, &home, policy.clone());
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        templar_file_index::ScanScope::Root,
        &Trigger::Requested,
        ScanOptions::reconcile(policy.clone()),
    )
    .unwrap();
    assert!(outcome.complete);
    let (bytes, manifest) = frame(&index, &home);
    let load = decode_snapshot_tree(&bytes, &manifest, &home).unwrap();
    assert!(load.complete);
    assert!(
        load.entries
            .iter()
            .any(|entry| entry.path == home.join("target") && entry.is_dir && entry.size == 17)
    );
    assert!(
        !load
            .entries
            .iter()
            .any(|entry| entry.path.ends_with("object.o")),
        "opaque totals do not materialize hidden leaf rows"
    );
    assert_eq!(
        templar_treemap::tree::build_tree(&load.entries).nodes[0].size,
        183,
        "cached opaque bytes survive the final rendering tree"
    );
    assert_summary_cached_totals(&index, &home);
    // An explicit real repair remeasures even a cache whose old stale bit was false.
    std::fs::write(home.join("target/deep/object.o"), vec![0u8; 29]).unwrap();
    reconcile::reconcile(
        &index,
        &mut owner,
        root,
        templar_file_index::ScanScope::Root,
        &Trigger::Requested,
        ScanOptions::reconcile(policy),
    )
    .unwrap();
    let (bytes, manifest) = frame(&index, &home);
    assert!(
        decode_snapshot_tree(&bytes, &manifest, &home)
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == home.join("target") && entry.size == 29)
    );

    index.mark_stale(root, &StaleReason::Unwatched).unwrap();
    let (bytes, manifest) = frame(&index, &notes);
    let stale = decode_snapshot_tree(&bytes, &manifest, &notes).unwrap();
    assert!(!stale.complete && !stale.entries.is_empty());
    assert!(stale.reason.unwrap().contains("unwatched"));
    let outside = dir.path().join("not-indexed");
    let (bytes, manifest) = frame(&index, &outside);
    let uncovered = decode_snapshot_tree(&bytes, &manifest, &outside).unwrap();
    assert!(!uncovered.complete && uncovered.entries.is_empty());
    assert!(uncovered.reason.unwrap().contains("no indexed coverage"));
}

fn assert_materialized_native_paths(index: &Index, home: &Path, native: &Path) {
    let (bytes, manifest) = frame(index, home);
    let mut load = decode_snapshot_tree(&bytes, &manifest, home).unwrap();
    assert!(load.complete);
    assert_eq!(load.generation, Some(manifest.generation.0));
    assert_eq!(load.snapshot_seq, Some(manifest.snapshot_seq));
    let scanned = templar_treemap::scan(home, templar_treemap::ScanLimits::default());
    assert!(scanned.complete);
    let mut entries = scanned.entries;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    load.entries.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(load.entries, entries);
    assert!(load.entries.iter().any(|entry| entry.path == native));
}

fn assert_summary_cached_totals(index: &Index, home: &Path) {
    let (summary, summary_manifest) = frame_view(index, home, SnapshotDetail::Summary);
    let summary = templar_client::tree_source::decode_snapshot_tree_view(
        &summary,
        &summary_manifest,
        home,
        SnapshotDetail::Summary,
    )
    .unwrap();
    assert!(summary.entries.iter().all(|entry| entry.is_dir));
    assert!(
        !summary.complete,
        "missing Detailed directory aggregates stay explicit"
    );
    assert_eq!(
        templar_treemap::tree::build_tree(&summary.entries).nodes[0].size,
        17,
        "missing parent aggregates do not erase an available child's cached size"
    );
}

fn assert_current_subtree(index: &Index, notes: &Path, root: RootId) {
    let (bytes, manifest) = frame(index, notes);
    let subtree = decode_snapshot_tree(&bytes, &manifest, notes).unwrap();
    assert!(
        subtree.complete,
        "a current covering root, not an exact registered-path match, proves subtree freshness"
    );
    assert!(
        subtree
            .entries
            .iter()
            .all(|entry| entry.path.starts_with(notes))
    );
    assert_eq!(subtree.roots[0].root, root);
    let mut wrong_manifest = manifest.clone();
    wrong_manifest.generation.0 += 1;
    let error = decode_snapshot_tree(&bytes, &wrong_manifest, notes).unwrap_err();
    assert!(
        !error.allows_fallback(),
        "a corrupt frame is not permission to start another crawler"
    );
}
