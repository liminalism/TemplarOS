//! What the watcher promises: a change becomes searchable within a bounded window, and the window
//! costs exactly one generation however many events it held.
//!
//! The clock is a fake and the kernel is a fake, on purpose. A batching bound tested by sleeping
//! is a bound that is either slow or flaky, and a queue overflow tested by filling a real kernel
//! queue is a test nobody runs.

use std::{fs, time::Duration};

use lege_file_index::{
    Index, IndexConfig, MutationSource, NativePath, RootPolicy, RootState, ScanScope,
    coalesce::{CoalesceConfig, Coalescer, FakeClock},
    query::Query,
    reconcile::{self, Trigger},
    watch::{FakeSource, RawKind, WatchBudget, WatchOwner},
};

/// A fixture tree and an index over it, already `current`.
struct Fixture {
    _temp: tempfile::TempDir,
    tree: std::path::PathBuf,
    index: Index,
    #[allow(dead_code)] // documents the fixture's root; not read by this test today
    root: lege_file_index::RootId,
    owner: WatchOwner<FakeSource>,
    clock: FakeClock,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let tree = temp.path().join("tree");
    fs::create_dir_all(tree.join("notes")).unwrap();
    fs::write(tree.join("notes/first.txt"), b"one").unwrap();

    let index = Index::open(&temp.path().join("index.sqlite3"), IndexConfig::default())
        .expect("opening the index");
    let policy = RootPolicy::default_home();
    let root = index
        .add_root(&NativePath::from_path(&tree), policy.clone())
        .expect("adding the root")
        .root;

    let mut owner = WatchOwner::new(FakeSource::new(), WatchBudget { max_watches: 64 });
    owner.track_root(root, &tree, policy.clone());

    // The first pass is the watcher's own walk: arm a directory, then read it. Doing it the other
    // way round leaves a window in which a file is in neither the scan nor the event stream.
    let options = lege_file_index::ScanOptions::reconcile(policy);
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Requested,
        options,
    )
    .expect("the first pass");
    assert!(outcome.complete, "the fixture tree is entirely readable");
    assert_eq!(index.root_status(root).unwrap().state, RootState::Current);

    Fixture {
        _temp: temp,
        tree,
        index,
        root,
        owner,
        clock: FakeClock::new(0),
    }
}

#[test]
fn watch_create_appears_within_bound() {
    let mut fixture = fixture();
    let before = fixture.index.generation().unwrap().0;

    // Two files land in the same burst, the way an unpacking archive or a save-all lands.
    fs::write(fixture.tree.join("notes/second.txt"), b"two").unwrap();
    fs::write(fixture.tree.join("notes/third.txt"), b"three").unwrap();
    let notes = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree.join("notes"))
        .expect("the notes directory was armed by the first pass");
    fixture
        .owner
        .source_mut()
        .push_named(notes, "second.txt", RawKind::Created, false);
    fixture
        .owner
        .source_mut()
        .push_named(notes, "third.txt", RawKind::Created, false);

    let pumped = fixture.owner.pump(Duration::ZERO, &fixture.clock);
    assert_eq!(pumped.mutations.len(), 2);
    assert!(
        pumped.repairs.is_empty(),
        "nothing was lost, so nothing to repair"
    );

    let mut coalescer = Coalescer::new(CoalesceConfig::default());
    for mutation in pumped.mutations {
        coalescer.push(&fixture.clock, mutation);
    }
    assert!(
        !coalescer.ready(&fixture.clock),
        "a batch must not close the instant an event arrives, or a busy tree commits a generation \
         per event"
    );

    fixture.clock.advance(250);
    assert!(
        coalescer.ready(&fixture.clock),
        "250 ms of quiet closes the batch"
    );
    let batch = coalescer.take(MutationSource::Watcher).expect("a batch");
    let applied = fixture.index.apply(batch).expect("applying the batch");

    assert_eq!(applied.changes.len(), 2);
    assert_eq!(
        applied.generation.0,
        before + 1,
        "one batch is one generation, whatever it held"
    );

    for name in ["second", "third"] {
        let result = fixture.index.search(&Query::new(name)).expect("a search");
        assert!(
            result
                .hits
                .iter()
                .any(|hit| hit.row.path.display().ends_with(&format!("{name}.txt"))),
            "{name}.txt should be searchable one batch after it was created"
        );
    }
}
