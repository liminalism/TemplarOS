//! What repair promises: the index stops claiming to be current the moment it cannot prove it,
//! keeps answering from the generation it already has while it repairs, and never turns "I could
//! not read that" into "that is deleted".

use std::{fs, time::Duration};

use lege_file_index::{
    Index, IndexConfig, MetadataFreshness, MutationSource, NativePath, RootId, RootPolicy,
    RootState, ScanBudget, ScanOptions, ScanScope,
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

/// The lie this labels: a `PolicyChanged` reconcile commits `complete = true` while
/// `take_on_trust` skips every child `stat`, so the root has always read `current` over metadata
/// nobody re-read — with no way to say so.
///
/// `RootState` is the namespace axis and does not move. `MetadataFreshness` is the other one, and
/// `namespace = current, metadata = unverified` is now both representable and reachable.
#[test]
fn a_namespace_only_repair_is_current_over_metadata_it_did_not_verify() {
    let temp = tempfile::tempdir().unwrap();
    let (index, root, tree, mut owner) = seeded(&temp);

    let fresh = index.root_status(root).unwrap();
    assert_eq!(fresh.state, RootState::Current);
    assert_eq!(
        fresh.metadata,
        MetadataFreshness::Verified,
        "a pass that stat'd everything vouches for what it stat'd"
    );

    // Rewrite a file in place. Its parent's timestamps do not move, so `take_on_trust` will take
    // the whole directory's membership on trust — which is correct, and says nothing at all about
    // the size of the file inside it.
    let file = tree.join("notes/first.txt");
    let before = fs::metadata(&file).unwrap().len();
    fs::write(&file, b"a great deal more than one").unwrap();
    let parent_mtime = fs::metadata(tree.join("notes")).unwrap();

    let (outcome, report) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::NamespaceOnly,
        ScanOptions::reconcile(index.root_policy(root).unwrap()),
    )
    .expect("the namespace-only pass");

    assert!(
        outcome.complete,
        "membership was exhaustively re-derived; that is what complete means"
    );
    assert!(report.dirs_proven_unchanged > 0, "the shortcut has to fire");
    assert_eq!(outcome.metadata, MetadataFreshness::Unverified);

    let status = index.root_status(root).unwrap();
    assert_eq!(
        status.state,
        RootState::Current,
        "the namespace axis is untouched by the metadata axis"
    );
    assert_eq!(status.metadata, MetadataFreshness::Unverified);

    let row = index
        .entry(root, &lege_file_index::NativePath::from_path(&file))
        .unwrap()
        .expect("the row is still there");
    assert_eq!(
        row.meta.size_bytes, before,
        "the pass re-committed the size it already had, which is the point of the label"
    );
    let _ = parent_mtime;

    // A query that depends on metadata is answered anyway, and told which roots it cannot trust.
    let mut sized = Query::new("first");
    sized.require_metadata = true;
    let result = index.search(&sized).expect("a search");
    assert_eq!(result.hits.len(), 1, "the query is served, never blocked");
    assert_eq!(result.stale_metadata_roots, vec![root]);

    // And an ordinary filename search neither pays for the distinction nor hears about it.
    let plain = index.search(&Query::new("first")).expect("a search");
    assert!(plain.stale_metadata_roots.is_empty());

    // A full pass restores both axes.
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Requested,
        ScanOptions::reconcile(index.root_policy(root).unwrap()),
    )
    .expect("the verifying pass");
    assert_eq!(outcome.metadata, MetadataFreshness::Verified);
    let status = index.root_status(root).unwrap();
    assert_eq!(status.state, RootState::Current);
    assert_eq!(status.metadata, MetadataFreshness::Verified);
    assert_eq!(
        index
            .entry(root, &lege_file_index::NativePath::from_path(&file))
            .unwrap()
            .unwrap()
            .meta
            .size_bytes,
        26
    );
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

    let pumped = owner.pump(Some(Duration::ZERO), &clock);
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

/// A subtree repair sees part of a root. Clearing a whole-root fault because that part came back
/// clean is the same lie the metadata axis already refuses to tell: the pass never looked at the
/// rest of the root, so it has nothing to say about it.
#[test]
fn a_subtree_repair_does_not_clear_a_whole_root_overflow() {
    let temp = tempfile::tempdir().unwrap();
    let (index, root, tree, mut owner) = seeded(&temp);

    // The kernel queue overflowed: nothing anywhere in this root is trustworthy until a whole-root
    // pass re-derives it.
    index
        .mark_stale(root, &lege_file_index::StaleReason::WatcherOverflow)
        .unwrap();

    // Both halves of the scope, exactly as `lege-filed`'s repair loop sets them: the walk starts
    // at the subtree, and the commit collects only inside it.
    let subtree = ScanScope::Subtree(NativePath::from_path(&tree.join("notes")));
    let scoped = ScanOptions {
        scope: subtree.clone(),
        ..ScanOptions::reconcile(index.root_policy(root).unwrap())
    };
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        subtree,
        &Trigger::Overflow,
        scoped,
    )
    .expect("the scoped repair");

    assert!(outcome.complete, "the pass did cover its own scope");
    assert_ne!(
        outcome.state,
        RootState::Current,
        "and covering a subtree is not covering the root"
    );
    let status = index.root_status(root).unwrap();
    assert_eq!(status.state, RootState::Stale);
    assert_eq!(
        status.reason.as_deref(),
        Some("watcher-overflow"),
        "a scoped pass may not answer a whole-root question"
    );

    // The whole-root pass is the only thing that may.
    let (outcome, _) = reconcile::reconcile(
        &index,
        &mut owner,
        root,
        ScanScope::Root,
        &Trigger::Overflow,
        ScanOptions::reconcile(index.root_policy(root).unwrap()),
    )
    .expect("the whole-root repair");
    assert!(outcome.complete);
    let status = index.root_status(root).unwrap();
    assert_eq!(status.state, RootState::Current);
    assert_eq!(status.reason, None);
    assert!(status.complete);
}

/// The same rule for the other whole-root fault. A budget stopped the pass somewhere the scoped
/// repair never goes, so the scoped repair cannot report the root complete — and the `complete`
/// column is what tells a reader whether the committed generation covered the whole root.
///
/// Driven through `scan_root` rather than `reconcile`, because `reconcile` re-stamps the root's
/// reason with its trigger's before it starts; here the commit is the only writer.
#[test]
fn a_subtree_repair_does_not_complete_a_root_a_budget_left_unfinished() {
    let temp = tempfile::tempdir().unwrap();
    let (index, root, tree, _owner) = seeded(&temp);
    let policy = index.root_policy(root).unwrap();

    let mut budgeted = ScanOptions::reconcile(policy.clone());
    budgeted.budget = ScanBudget::unlimited().with_max_entries(1);
    let partial = index.scan_root(root, budgeted).expect("the budgeted pass");
    assert!(!partial.complete, "a budget stopped it");
    let stopped = index.root_status(root).unwrap();
    assert_eq!(stopped.state, RootState::Stale);
    assert_eq!(stopped.reason.as_deref(), Some("budget:entries"));
    assert!(!stopped.complete);

    let scoped = index
        .scan_root(
            root,
            ScanOptions {
                scope: ScanScope::Subtree(NativePath::from_path(&tree.join("notes"))),
                ..ScanOptions::reconcile(policy.clone())
            },
        )
        .expect("the scoped pass");
    assert!(scoped.complete);
    let after = index.root_status(root).unwrap();
    assert_eq!(after.state, RootState::Stale);
    assert_eq!(
        after.reason.as_deref(),
        Some("budget:entries"),
        "a scoped pass may not replace a whole-root reason"
    );
    assert!(
        !after.complete,
        "nor may it report the root's generation complete"
    );

    let whole = index
        .scan_root(root, ScanOptions::reconcile(policy))
        .expect("the whole-root pass");
    assert!(whole.complete);
    let done = index.root_status(root).unwrap();
    assert_eq!(done.state, RootState::Current);
    assert_eq!(done.reason, None);
    assert!(done.complete);
}
