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

    let pumped = fixture.owner.pump(Some(Duration::ZERO), &fixture.clock);
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

/// The budget is not an edge case on this machine, it is the normal outcome.
///
/// Measured on the reference laptop: `$HOME` holds 189,089 directories after the skip policy
/// against a watch budget of 125,037. An arm pass over a home root therefore *cannot* finish, and
/// the only question is whether the index says so.
///
/// It does now. `ScanSession::commit` counts a directory it could not arm exactly as it counts
/// one it could not read, so the pass is incomplete, collection is suppressed, and the root
/// carries `watcher-exhausted` with the armed and refused counts rather than committing `current`
/// over a subtree nothing is watching.
#[test]
fn a_pass_that_ran_out_of_watches_reports_an_incomplete_walk() {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let tree = temp.path().join("tree");
    for name in ["one", "two", "three", "four"] {
        fs::create_dir_all(tree.join(name)).unwrap();
        fs::write(tree.join(name).join("file.txt"), b"x").unwrap();
    }

    let index = Index::open(&temp.path().join("index.sqlite3"), IndexConfig::default())
        .expect("opening the index");
    let policy = RootPolicy::default_home();
    let root = index
        .add_root(&NativePath::from_path(&tree), policy.clone())
        .expect("adding the root")
        .root;

    // Two watches for five directories: the walk arms the root and one child, and is refused for
    // the rest — exactly what the kernel does at `max_user_watches`.
    let mut owner = WatchOwner::new(FakeSource::new(), WatchBudget { max_watches: 2 });
    owner.track_root(root, &tree, policy.clone());
    let (outcome, report) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Requested,
        lege_file_index::ScanOptions::reconcile(policy),
    )
    .expect("the pass itself succeeds; it is the claim it makes that matters");

    assert!(
        !report.complete,
        "a walk that could not watch every directory is not a complete walk"
    );
    assert!(
        !outcome.complete,
        "and the generation it commits must say so too, or collection will run"
    );
    assert_eq!(outcome.unwatched_dirs, report.dirs_unwatched);
    let status = index.root_status(root).unwrap();
    assert_ne!(
        status.state,
        RootState::Current,
        "an unwatched subtree may not be reported as up to date"
    );
    assert_eq!(
        status.reason.as_deref(),
        Some("watcher-exhausted:armed=2,refused=3"),
        "the reason has to send a reader to the watch budget, not to a directory's mode"
    );
    assert_eq!(
        owner.watches_armed(),
        2,
        "the budget was spent, not exceeded"
    );
    assert!(
        owner.status(0).watches_refused >= 3,
        "every directory past the budget is a refusal the host has to account for"
    );
}
