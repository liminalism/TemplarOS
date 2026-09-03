//! What repair promises: the index stops claiming to be current the moment it cannot prove it,
//! keeps answering from the generation it already has while it repairs, and never turns "I could
//! not read that" into "that is deleted".

use std::{fs, time::Duration};

use lege_file_index::{
    Index, IndexConfig, MutationSource, NativePath, RootId, RootPolicy, RootState, ScanBudget,
    ScanOptions, ScanScope,
    coalesce::{CoalesceConfig, Coalescer, FakeClock},
    query::Query,
    reconcile::{self, Trigger},
    watch::{FakeSource, WatchBudget, WatchOwner},
};

fn open(dir: &std::path::Path) -> Index {
    Index::open(&dir.join("index.sqlite3"), IndexConfig::default()).expect("opening the index")
}

fn hits(index: &Index, text: &str) -> usize {
    index
        .search(&Query::new(text))
        .expect("a search")
        .hits
        .len()
}

/// Build a small tree, index it completely, and hand back the pieces.
fn seeded(temp: &tempfile::TempDir) -> (Index, RootId, std::path::PathBuf, WatchOwner<FakeSource>) {
    let tree = temp.path().join("tree");
    fs::create_dir_all(tree.join("notes")).unwrap();
    fs::write(tree.join("notes/first.txt"), b"one").unwrap();
    fs::write(tree.join("notes/second.txt"), b"two").unwrap();

    let index = open(temp.path());
    let policy = RootPolicy::default_home();
    let root = index
        .add_root(&NativePath::from_path(&tree), policy.clone())
        .expect("adding the root")
        .root;
    let mut owner = WatchOwner::new(FakeSource::new(), WatchBudget { max_watches: 64 });
    owner.track_root(root, &tree, policy.clone());
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Requested,
        ScanOptions::reconcile(policy),
    )
    .expect("the first pass");
    assert!(outcome.complete);
    (index, root, tree, owner)
}

#[test]
fn reconcile_overflow_marks_stale_then_reconciles() {
    let temp = tempfile::tempdir().unwrap();
    let (index, root, tree, mut owner) = seeded(&temp);
    let clock = FakeClock::new(0);
    assert_eq!(index.root_status(root).unwrap().state, RootState::Current);

    // The change the watcher never sees: the kernel drops the event, and the file appears behind
    // its back. This is the whole hazard — without overflow handling the root would keep calling
    // itself current while missing this file for as long as the daemon runs.
    fs::write(tree.join("notes/unseen.txt"), b"three").unwrap();
    owner.source_mut().push_overflow();

    let pumped = owner.pump(Duration::ZERO, &clock);
    assert_eq!(pumped.repairs.len(), 1, "an overflow schedules a repair");
    assert!(pumped.repairs[0].mark_root_stale);
    assert_eq!(pumped.repairs[0].scope, ScanScope::Root);

    let mut coalescer = Coalescer::new(CoalesceConfig::default());
    for mutation in pumped.mutations {
        coalescer.push(&clock, mutation);
    }
    clock.advance(250);
    index
        .apply(coalescer.take(MutationSource::Watcher).expect("a batch"))
        .expect("applying the invalidation");

    let status = index.root_status(root).unwrap();
    assert_eq!(status.state, RootState::Stale);
    assert_eq!(status.reason.as_deref(), Some("watcher-overflow"));
    assert_eq!(
        hits(&index, "first"),
        1,
        "a stale root keeps answering from the generation it already has"
    );
    assert_eq!(
        hits(&index, "unseen"),
        0,
        "the missed file is genuinely missing"
    );

    // The repair the overflow asked for.
    let (outcome, report) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Overflow,
        ScanOptions::reconcile(index.root_policy(root).unwrap()),
    )
    .expect("the repair pass");

    assert!(outcome.complete);
    assert_eq!(index.root_status(root).unwrap().state, RootState::Current);
    assert_eq!(
        hits(&index, "unseen"),
        1,
        "the repair found what the overflow lost"
    );
    assert_eq!(hits(&index, "first"), 1, "and kept what was already there");
    assert!(report.dirs_walked >= 2);
}

#[test]
fn reconcile_downtime_keeps_old_generation_visible() {
    let temp = tempfile::tempdir().unwrap();
    let (index, root, tree, mut owner) = seeded(&temp);
    let generation_before = index.generation().unwrap().0;
    drop(index);

    // The daemon is not running. Nothing that happens here produces an event, and nothing about
    // it can be reconstructed afterwards — which is the reason a restart may not call itself
    // current.
    fs::write(tree.join("notes/added-while-down.txt"), b"new").unwrap();
    fs::remove_file(tree.join("notes/second.txt")).unwrap();

    let index = open(temp.path());
    assert_eq!(
        hits(&index, "first"),
        1,
        "a restart serves the previous generation before it reconciles anything"
    );
    assert_eq!(
        index.generation().unwrap().0,
        generation_before,
        "opening the index walks nothing and commits nothing"
    );

    // What the watcher host does at startup for every root it was serving.
    index
        .mark_stale(root, &lege_file_index::StaleReason::Downtime)
        .unwrap();
    let status = index.root_status(root).unwrap();
    assert_eq!(status.state, RootState::Stale);
    assert_eq!(status.reason.as_deref(), Some("downtime"));
    assert_eq!(hits(&index, "first"), 1, "still searchable while stale");

    // A repair that cannot finish — here because a budget stops it — must leave the previous
    // generation exactly where it was. An incomplete pass that collected rows would delete files
    // that are still on disk.
    let mut options = ScanOptions::reconcile(index.root_policy(root).unwrap());
    options.budget = ScanBudget::unlimited().with_max_entries(1);
    let (partial, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Downtime,
        options,
    )
    .expect("the interrupted pass");
    assert!(!partial.complete, "a budget stopped it");
    assert_ne!(
        partial.state,
        RootState::Current,
        "an incomplete pass is never current"
    );
    assert_eq!(
        hits(&index, "first"),
        1,
        "the previous generation survived an interrupted repair"
    );

    // And the complete pass converges: the file added during downtime appears, the one removed
    // during downtime goes, and only now does the root call itself current.
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Downtime,
        ScanOptions::reconcile(index.root_policy(root).unwrap()),
    )
    .expect("the complete pass");
    assert!(outcome.complete);
    assert_eq!(index.root_status(root).unwrap().state, RootState::Current);
    assert_eq!(hits(&index, "added-while-down"), 1);
    assert_eq!(
        hits(&index, "second"),
        0,
        "collected only by a complete pass"
    );
    assert!(outcome.generation.0 > generation_before);
}
