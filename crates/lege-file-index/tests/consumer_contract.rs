//! The contract a consumer of the index actually depends on, walked end to end in one test.
//!
//! `@legeos.decision.event-driven-service-authorities` says a consumer must be able to
//! "synchronize a subscription without a read/subscribe race, and recover missed history by
//! sequence or by taking a fresh snapshot", and that "consumers never read an owner's private
//! SQLite representation". Every piece of that is covered by a unit test somewhere in this crate.
//! What was not covered is the *sequence* — the thing a real consumer does, in order, where a
//! gap between two correct steps is exactly the bug the decision exists to prevent.
//!
//! So this is deliberately shaped like a client and not like a unit test. It takes a snapshot,
//! notes the sequence that snapshot was consistent with, applies changes behind its back, asks
//! for everything after that sequence, and checks that what it received is precisely what it
//! missed: no gap, and no change replayed twice. Then it lets the change log be pruned past its
//! position and checks that the answer is `SnapshotRequired` rather than a shorter history
//! presented as a complete one.
//!
//! One thing this test cannot state, and the ledger records instead: `cargo tree -p lege-client
//! -i rusqlite` *succeeds* at HEAD, because `lege-client` uses this crate's `model`, `query` and
//! `snapshot` types in `tree_source.rs` and so links rusqlite transitively. That is a linkage,
//! not a store access — the consumer path below never opens a database — and separating the types
//! from the store is a change across four manifests that this campaign did not make.

use lege_file_index::{
    EntryMeta, Index, IndexConfig, IndexError, Mutation, MutationBatch, MutationSource, NativePath,
    RootPolicy,
    snapshot::{SnapshotReader, SnapshotRequest},
};

fn open(dir: &std::path::Path, max_changes: u64) -> Index {
    Index::open(
        &dir.join("index.sqlite3"),
        IndexConfig {
            max_changes,
            ..IndexConfig::default()
        },
    )
    .expect("opening the index")
}

/// A file that exists, so a mutation carrying its metadata is the metadata a scan would record.
fn touch(root: &std::path::Path, name: &str) -> NativePath {
    let path = root.join(name);
    std::fs::write(&path, name.as_bytes()).expect("writing the fixture file");
    NativePath::from_path(&path)
}

fn upsert(root: lege_file_index::RootId, path: &NativePath) -> Mutation {
    let native = path.to_path();
    let metadata = std::fs::symlink_metadata(&native).expect("the fixture file exists");
    Mutation::Upsert {
        root,
        path: path.clone(),
        meta: EntryMeta::from_symlink_metadata(&native, &metadata),
    }
}

#[test]
fn a_consumer_synchronizes_a_snapshot_and_a_change_stream_without_a_gap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(&tree).expect("the tree");
    let index = open(dir.path(), 10_000);
    let root = index
        .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
        .expect("registering the root")
        .root;

    let before = touch(&tree, "already-there");
    index
        .apply(MutationBatch::new(
            MutationSource::Reconcile,
            vec![upsert(root, &before)],
        ))
        .expect("the initial state");

    // 1. Take the snapshot. The position it is consistent with comes back *with* it: asking
    //    for the sequence in a second call is the read/subscribe race, because anything committed
    //    between the two lands in neither the frame nor the stream.
    let mut frame = Vec::new();
    let manifest = index
        .write_snapshot(&mut frame, &SnapshotRequest::default())
        .expect("writing the snapshot");
    let position = manifest.snapshot_seq;

    let reader = SnapshotReader::new(&frame).expect("a sealed frame");
    assert_eq!(
        reader.generation, manifest.generation,
        "the frame and its manifest describe different generations"
    );
    assert_eq!(reader.entry_count, 1, "the snapshot holds the one entry");

    // 2. The world moves while the consumer is busy parsing what it was given.
    let created = touch(&tree, "arrived-later");
    index
        .apply(MutationBatch::new(
            MutationSource::Watcher,
            vec![upsert(root, &created)],
        ))
        .expect("a change behind the consumer's back");
    index
        .apply(MutationBatch::new(
            MutationSource::Operation,
            vec![Mutation::Remove {
                root,
                path: before.clone(),
            }],
        ))
        .expect("another one");

    // 3. Catch up from the noted position. Exactly the two changes that happened since, in order,
    //    and nothing that was already in the frame.
    let batch = index.changes_since(position, 100).expect("catching up");
    assert_eq!(
        batch.changes.len(),
        2,
        "the consumer saw {} change(s) after {position}, not the two it missed",
        batch.changes.len()
    );
    assert!(!batch.more, "two changes fit in a page of a hundred");
    assert!(
        batch
            .changes
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq),
        "the change log is not ordered by sequence"
    );
    assert!(
        batch.changes.iter().all(|change| change.seq > position),
        "a change at or before the snapshot's position was replayed"
    );

    // 4. And asking again from the new position yields nothing: the stream does not repeat.
    let empty = index
        .changes_since(batch.next_seq, 100)
        .expect("asking again");
    assert!(
        empty.changes.is_empty(),
        "the consumer was handed the same changes twice"
    );
}

#[test]
fn a_consumer_that_fell_behind_the_retained_log_is_told_to_resnapshot() {
    // The alternative — handing back whatever history survives — is the failure the decision
    // names: a consumer would apply a partial catch-up and believe it was whole.
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(&tree).expect("the tree");
    // A change log of two rows, so a handful of mutations prunes past any earlier position.
    let index = open(dir.path(), 2);
    let root = index
        .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
        .expect("registering the root")
        .root;

    let first = touch(&tree, "one");
    index
        .apply(MutationBatch::new(
            MutationSource::Reconcile,
            vec![upsert(root, &first)],
        ))
        .expect("the first change");
    let position = index
        .write_snapshot(&mut Vec::new(), &SnapshotRequest::default())
        .expect("the consumer's first snapshot")
        .snapshot_seq;

    for name in ["two", "three", "four", "five"] {
        let path = touch(&tree, name);
        index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![upsert(root, &path)],
            ))
            .expect("a change that pushes the floor up");
    }

    match index.changes_since(position, 100) {
        Err(IndexError::SnapshotRequired { .. }) => {}
        Err(other) => panic!("expected SnapshotRequired, got {other}"),
        Ok(batch) => panic!(
            "a consumer {} change(s) behind a pruned log was handed a partial history instead of \
             being told to resnapshot",
            batch.changes.len()
        ),
    }

    // And resnapshotting is a route back: the frame is whole, and the position it comes with is
    // one the log still retains.
    let mut fresh = Vec::new();
    let recovered = index
        .write_snapshot(&mut fresh, &SnapshotRequest::default())
        .expect("resnapshotting")
        .snapshot_seq;
    assert!(
        index.changes_since(recovered, 100).is_ok(),
        "the position a fresh snapshot hands back is itself already below the retained floor, so \
         resnapshotting is not a route back at all"
    );
    let reader = SnapshotReader::new(&fresh).expect("a sealed frame");
    assert_eq!(reader.entry_count, 5, "the fresh frame holds every entry");
}

#[test]
fn the_error_a_consumer_matches_on_is_the_one_the_interface_names() {
    // A D-Bus client cannot match on a Rust enum; it matches on the error name in
    // interfaces/dbus/org.legeos.Files.Index1.xml. If that string moves, every consumer's
    // recovery path silently becomes an unhandled failure.
    let error = IndexError::SnapshotRequired {
        reason: "the requested change sequence has been pruned",
    };
    assert_eq!(
        error.dbus_name(),
        "org.legeos.Files.Index1.Error.SnapshotRequired"
    );
}
