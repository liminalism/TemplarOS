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
    watch::{FakeSource, RawEvent, RawKind, WatchBudget, WatchOwner},
};

/// A fixture tree and an index over it, already `current`.
struct Fixture {
    _temp: tempfile::TempDir,
    tree: std::path::PathBuf,
    /// The index's database file, so a test can read it back the way the delta fixture does:
    /// every live row at once, which is the only way to notice a whole subtree going missing.
    database: std::path::PathBuf,
    index: Index,
    #[allow(dead_code)] // documents the fixture's root; not read by this test today
    root: lege_file_index::RootId,
    owner: WatchOwner<FakeSource>,
    clock: FakeClock,
}

fn fixture() -> Fixture {
    fixture_with(|tree| {
        fs::create_dir_all(tree.join("notes")).unwrap();
        fs::write(tree.join("notes/first.txt"), b"one").unwrap();
    })
}

fn fixture_with(build: impl FnOnce(&std::path::Path)) -> Fixture {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let tree = temp.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    build(&tree);

    let database = temp.path().join("index.sqlite3");
    let index = Index::open(&database, IndexConfig::default()).expect("opening the index");
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
        database,
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
    // Two files, and one refresh of the directory they appeared in: a directory's mtime moves
    // when a name appears in it, and nothing else in the batch says so.
    assert_eq!(pumped.mutations.len(), 3);
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

    // The two files, and the directory they landed in.
    assert_eq!(applied.changes.len(), 3);
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

/// The row a fresh crawl of the same tree would hold for `path`.
fn as_scanned(tree: &std::path::Path, path: &std::path::Path) -> lege_file_index::EntryMeta {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let index = Index::open(&temp.path().join("fresh.sqlite3"), IndexConfig::default())
        .expect("opening a comparison index");
    let root = index
        .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
        .expect("adding the root")
        .root;
    index
        .scan_root(root, lege_file_index::ScanOptions::default())
        .expect("a fresh pass");
    index
        .entry(root, &NativePath::from_path(path))
        .expect("reading the row")
        .expect("the path is on disk")
        .meta
}

/// inotify reports the name that was opened, and a hard link has more than one name.
///
/// The Btrfs delta gate found this, but nothing about it is Btrfs: a rewrite through `two` leaves
/// `one` carrying the old size and mode, because the event names `two` and nothing else. The fix
/// belongs in `Index::apply`, so this asserts the watcher inherits it.
#[test]
fn a_write_through_one_name_of_a_hard_link_updates_the_other() {
    let mut fixture = fixture_with(|tree| {
        fs::write(tree.join("one"), b"the original contents").unwrap();
        fs::hard_link(tree.join("one"), tree.join("two")).unwrap();
    });

    fs::write(fixture.tree.join("two"), b"short").unwrap();
    fs::set_permissions(
        fixture.tree.join("two"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let watch = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree)
        .expect("the root was armed by the first pass");
    fixture
        .owner
        .source_mut()
        .push_named(watch, "two", RawKind::Written, false);

    let pumped = fixture.owner.pump(Some(Duration::ZERO), &fixture.clock);
    let batch = lege_file_index::MutationBatch::new(MutationSource::Watcher, pumped.mutations);
    fixture.index.apply(batch).expect("applying the batch");

    for name in ["one", "two"] {
        let path = fixture.tree.join(name);
        let held = fixture
            .index
            .entry(fixture.root, &NativePath::from_path(&path))
            .expect("reading the row")
            .expect("the row is there")
            .meta;
        assert_eq!(
            held,
            as_scanned(&fixture.tree, &path),
            "{name} disagrees with a fresh scan of the same file"
        );
    }
}

/// A `MOVED_FROM`/`MOVED_TO` pair over a file that already exists — `mv a b` with `b` there.
///
/// The destination row cannot be tombstoned in place: `entries_path` is unique on (root, path),
/// so before the move learned to take the row over this batch died on the constraint.
#[test]
fn a_rename_over_an_existing_file_applies() {
    let mut fixture = fixture_with(|tree| {
        fs::write(tree.join("a"), b"the source").unwrap();
        fs::write(tree.join("b"), b"the doomed destination").unwrap();
    });

    fs::rename(fixture.tree.join("a"), fixture.tree.join("b")).unwrap();
    let watch = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree)
        .expect("the root was armed by the first pass");
    fixture
        .owner
        .source_mut()
        .push_named(watch, "a", RawKind::MovedFrom, false);
    fixture
        .owner
        .source_mut()
        .push_named(watch, "b", RawKind::MovedTo, false);

    let pumped = fixture.owner.pump(Some(Duration::ZERO), &fixture.clock);
    let batch = lege_file_index::MutationBatch::new(MutationSource::Watcher, pumped.mutations);
    fixture.index.apply(batch).expect("applying the batch");

    assert!(
        fixture
            .index
            .entry(
                fixture.root,
                &NativePath::from_path(&fixture.tree.join("a"))
            )
            .expect("reading the row")
            .is_none(),
        "the source path is gone"
    );
    let destination = fixture.tree.join("b");
    assert_eq!(
        fixture
            .index
            .entry(fixture.root, &NativePath::from_path(&destination))
            .expect("reading the row")
            .expect("the row is there")
            .meta,
        as_scanned(&fixture.tree, &destination),
        "the destination holds what the move put there"
    );
}

/// The row-by-row comparison the Btrfs delta harness uses, included rather than restated. A
/// watcher batch that leaves a row in the right place with the wrong metadata is the same failure
/// as a delta that does, and comparing paths is blind to both.
#[path = "support/entry_facts.rs"]
mod entry_facts;

use entry_facts::{Fingerprint, describe_difference, fingerprint};

/// What a fresh crawl of `tree` holds.
fn crawled(tree: &std::path::Path) -> Fingerprint {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let database = temp.path().join("fresh.sqlite3");
    let index = Index::open(&database, IndexConfig::default()).expect("opening a comparison index");
    let root = index
        .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
        .expect("adding the root")
        .root;
    index
        .scan_root(root, lege_file_index::ScanOptions::default())
        .expect("a fresh pass");
    drop(index);
    fingerprint(&database)
}

/// Fail with the field-level difference rather than with two large maps.
#[track_caller]
fn assert_matches_a_crawl(database: &std::path::Path, tree: &std::path::Path, what: &str) {
    let difference = describe_difference(&fingerprint(database), &crawled(tree));
    assert!(
        difference.is_none(),
        "{what}:\n{}",
        difference.unwrap_or_default()
    );
}

fn a_branch_with_two_leaves(tree: &std::path::Path) {
    fs::create_dir_all(tree.join("branch/leaf00")).unwrap();
    fs::create_dir_all(tree.join("branch/leaf01")).unwrap();
    fs::write(tree.join("branch/leaf00/f00.txt"), b"zero").unwrap();
    fs::write(tree.join("branch/leaf01/f01.txt"), b"one").unwrap();
    fs::write(tree.join("branch/top.txt"), b"top").unwrap();
}

fn moved(
    watch: lege_file_index::watch::WatchId,
    name: &str,
    kind: RawKind,
    cookie: u32,
) -> RawEvent {
    RawEvent {
        watch,
        name: Some(name.as_bytes().to_vec()),
        kind,
        cookie,
        is_dir: true,
    }
}

/// A directory rename with a second rename nested inside it, off the kernel's own event stream.
///
/// The watcher names each path as it stands when the event arrives, so the outer rename is
/// reported first and the inner one under the outer's *new* name. That is already the order
/// `Mutation::Move` needs — it sweeps a move's destination subtree before rewriting descendants —
/// and this test is what proves the watcher does not need the re-addressing the delta plan does.
#[test]
fn a_rename_nested_inside_a_renamed_directory_keeps_its_children() {
    let mut fixture = fixture_with(a_branch_with_two_leaves);
    let root_watch = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree)
        .expect("the root was armed by the first pass");
    let branch_watch = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree.join("branch"))
        .expect("the branch was armed by the first pass");

    fs::rename(
        fixture.tree.join("branch"),
        fixture.tree.join("branch-renamed"),
    )
    .unwrap();
    fs::rename(
        fixture.tree.join("branch-renamed/leaf00"),
        fixture.tree.join("branch-renamed/leaf00-moved"),
    )
    .unwrap();

    // The kernel keeps a directory's watch across a rename, so the inner pair arrives on the same
    // descriptor under a name the watcher has to have retargeted.
    let source = fixture.owner.source_mut();
    source.push(moved(root_watch, "branch", RawKind::MovedFrom, 1));
    source.push(moved(root_watch, "branch-renamed", RawKind::MovedTo, 1));
    source.push(moved(branch_watch, "leaf00", RawKind::MovedFrom, 2));
    source.push(moved(branch_watch, "leaf00-moved", RawKind::MovedTo, 2));

    let pumped = fixture.owner.pump(Some(Duration::ZERO), &fixture.clock);
    assert_eq!(
        pumped.mutations.len(),
        4,
        "two renames, and a refresh of each directory a name changed in"
    );
    assert!(pumped.repairs.is_empty(), "nothing was lost");
    fixture
        .index
        .apply(lege_file_index::MutationBatch::new(
            MutationSource::Watcher,
            pumped.mutations,
        ))
        .expect("applying the batch");

    assert_matches_a_crawl(
        &fixture.database,
        &fixture.tree,
        "the nested rename did not leave the index where a crawl puts it",
    );
}

/// A file appearing in a directory changes that directory's mtime, and the kernel reports the
/// child. Nothing in a batch of child events says the directory itself moved on, so a watcher that
/// only writes the child leaves the folder's row holding a timestamp from before the file existed
/// — the same failure the Btrfs delta had, in the other consumer, and equally invisible to a
/// comparison of paths.
#[test]
fn a_directory_that_gained_an_entry_is_re_read_and_only_once() {
    let mut fixture = fixture();
    let notes = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree.join("notes"))
        .expect("the notes directory was armed by the first pass");

    for name in ["second.txt", "third.txt", "fourth.txt"] {
        fs::write(fixture.tree.join("notes").join(name), b"x").unwrap();
        fixture
            .owner
            .source_mut()
            .push_named(notes, name, RawKind::Created, false);
    }

    let pumped = fixture.owner.pump(Some(Duration::ZERO), &fixture.clock);
    assert_eq!(
        pumped.mutations.len(),
        4,
        "three files and one refresh of the directory — a directory is re-read once a pump, not \
         once an event, or unpacking an archive would pay a stat per file twice over"
    );
    fixture
        .index
        .apply(lege_file_index::MutationBatch::new(
            MutationSource::Watcher,
            pumped.mutations,
        ))
        .expect("applying the batch");

    assert_matches_a_crawl(
        &fixture.database,
        &fixture.tree,
        "the directory the files appeared in kept the timestamps it had before they did",
    );
}

/// A deletion empties a directory the same way a creation fills it, and the row has to say so.
#[test]
fn a_directory_that_lost_an_entry_is_re_read() {
    let mut fixture = fixture();
    let notes = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree.join("notes"))
        .expect("the notes directory was armed by the first pass");

    fs::remove_file(fixture.tree.join("notes/first.txt")).unwrap();
    fixture
        .owner
        .source_mut()
        .push_named(notes, "first.txt", RawKind::Deleted, false);

    let pumped = fixture.owner.pump(Some(Duration::ZERO), &fixture.clock);
    fixture
        .index
        .apply(lege_file_index::MutationBatch::new(
            MutationSource::Watcher,
            pumped.mutations,
        ))
        .expect("applying the batch");

    assert_matches_a_crawl(
        &fixture.database,
        &fixture.tree,
        "the directory the file was deleted from kept its old timestamps",
    );
}

/// The same end state, renamed the other way round: the leaf first, then the branch it sits in.
/// Both moves land in one batch, so the batch carries a move whose source is inside the next
/// move's source — the shape the index re-paths for itself.
#[test]
fn a_nested_rename_before_the_directory_it_sits_in_keeps_its_children() {
    let mut fixture = fixture_with(a_branch_with_two_leaves);
    let root_watch = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree)
        .expect("the root was armed by the first pass");
    let branch_watch = fixture
        .owner
        .source_mut()
        .watch_for(&fixture.tree.join("branch"))
        .expect("the branch was armed by the first pass");

    // Pumped between the two renames, because the watcher stats the destination as it sees it and
    // `branch/leaf00-moved` stops existing the moment the branch is renamed.
    fs::rename(
        fixture.tree.join("branch/leaf00"),
        fixture.tree.join("branch/leaf00-moved"),
    )
    .unwrap();
    let source = fixture.owner.source_mut();
    source.push(moved(branch_watch, "leaf00", RawKind::MovedFrom, 1));
    source.push(moved(branch_watch, "leaf00-moved", RawKind::MovedTo, 1));
    let mut mutations = fixture
        .owner
        .pump(Some(Duration::ZERO), &fixture.clock)
        .mutations;

    fs::rename(
        fixture.tree.join("branch"),
        fixture.tree.join("branch-renamed"),
    )
    .unwrap();
    let source = fixture.owner.source_mut();
    source.push(moved(root_watch, "branch", RawKind::MovedFrom, 2));
    source.push(moved(root_watch, "branch-renamed", RawKind::MovedTo, 2));
    mutations.extend(
        fixture
            .owner
            .pump(Some(Duration::ZERO), &fixture.clock)
            .mutations,
    );

    assert_eq!(
        mutations.len(),
        4,
        "two renames, and a refresh of each directory a name changed in — one per pump, since \
         each rename was pumped on its own"
    );
    fixture
        .index
        .apply(lege_file_index::MutationBatch::new(
            MutationSource::Watcher,
            mutations,
        ))
        .expect("applying the batch");

    assert_matches_a_crawl(
        &fixture.database,
        &fixture.tree,
        "the nested rename did not leave the index where a crawl puts it",
    );
}
