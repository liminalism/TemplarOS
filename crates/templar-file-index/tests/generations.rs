//! The generation publication invariant: every commit that changes visible rows publishes a head
//! strictly greater than every earlier head, and every row it writes carries that head.
//!
//! A scan takes its generation when it starts. Before this invariant, a scan that committed after a
//! newer mutation left the published head where the mutation put it while stamping its own rows
//! with the older, smaller generation — so arrays built in between still described "the current
//! generation" without the scan's rows, and a cursor fence comparing `written_generation` against
//! the cursor's generation never saw them change.

use proptest::prelude::*;
use std::path::Path;
use templar_file_index::{
    EntryMeta, Generation, Index, IndexConfig, IndexError, Mutation, MutationBatch, MutationSource,
    NativePath, Query, RootId, RootPolicy, ScanOptions, scan::walk_into,
};

fn open(dir: &Path, arrays: bool) -> Index {
    Index::open(
        &dir.join("index.sqlite3"),
        IndexConfig {
            search_arrays: arrays,
            ..IndexConfig::default()
        },
    )
    .expect("opening the index")
}

fn write(path: &Path, bytes: &[u8]) -> NativePath {
    std::fs::write(path, bytes).expect("writing a fixture file");
    NativePath::from_path(path)
}

fn upsert(index: &Index, root: RootId, path: &NativePath) -> Generation {
    let native = path.to_path();
    let metadata = std::fs::symlink_metadata(&native).expect("the fixture exists");
    index
        .apply(MutationBatch::new(
            MutationSource::Watcher,
            vec![Mutation::Upsert {
                root,
                path: path.clone(),
                meta: EntryMeta::from_symlink_metadata(&native, &metadata),
            }],
        ))
        .expect("applying a mutation")
        .generation
}

fn scanned_root(index: &Index, tree: &Path) -> RootId {
    std::fs::create_dir_all(tree).expect("the tree");
    let root = index
        .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
        .expect("registering the root")
        .root;
    index
        .scan_root(root, ScanOptions::default())
        .expect("the first scan");
    root
}

fn names(index: &Index, text: &str) -> (Vec<Vec<u8>>, String) {
    let result = index
        .search(
            &Query::new(text),
            &templar_file_index::CallContext::internal(),
        )
        .expect("searching");
    let names = result
        .hits
        .iter()
        .map(|hit| hit.row.name.as_bytes().to_vec())
        .collect();
    (names, result.strategy)
}

#[test]
fn a_scan_that_finishes_after_a_newer_mutation_publishes_a_newer_generation() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let index = open(dir.path(), true);
    std::fs::create_dir_all(&tree).unwrap();
    write(&tree.join("alpha.txt"), b"a");
    let root = scanned_root(&index, &tree);

    // The scan stages a file nobody has mutated in yet.
    write(&tree.join("beta-late.txt"), b"b");
    let mut session = index
        .begin_scan(root, ScanOptions::default())
        .expect("opening a scan");
    walk_into(&mut session, &NativePath::from_path(&tree)).expect("walking");

    // A mutation publishes while the scan is still open; arrays are built for that head.
    let mutated = upsert(&index, root, &write(&tree.join("gamma.txt"), b"g"));
    index.prepare_search_arrays().expect("building the arrays");

    let outcome = session.commit().expect("committing the scan");
    let head = index.generation().unwrap();
    assert!(
        head > mutated,
        "the scan committed visible rows without publishing past {mutated}: head {head}"
    );
    assert_eq!(outcome.generation, head);

    // Whatever answers now must include the scan's row.
    let (found, strategy) = names(&index, "beta");
    assert_eq!(
        found,
        vec![b"beta-late.txt".to_vec()],
        "answered by {strategy}"
    );
}

#[test]
fn rows_a_scan_commits_carry_the_generation_it_published() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let index = open(dir.path(), false);
    std::fs::create_dir_all(&tree).unwrap();
    let root = scanned_root(&index, &tree);

    let late = write(&tree.join("late.txt"), b"l");
    let mut session = index.begin_scan(root, ScanOptions::default()).unwrap();
    walk_into(&mut session, &NativePath::from_path(&tree)).unwrap();
    upsert(&index, root, &write(&tree.join("other.txt"), b"o"));
    let outcome = session.commit().unwrap();

    let row = index.entry(root, &late).unwrap().expect("the scanned row");
    assert_eq!(row.written_generation, outcome.generation);
    assert_eq!(
        index.root_status(root).unwrap().generation,
        outcome.generation
    );
    assert_eq!(index.generation().unwrap(), outcome.generation);
}

#[test]
fn forgetting_a_root_advances_the_generation_and_arrays_stop_answering_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let index = open(dir.path(), true);
    let kept = dir.path().join("kept");
    let doomed = dir.path().join("doomed");
    for tree in [&kept, &doomed] {
        std::fs::create_dir_all(tree).unwrap();
    }
    write(&kept.join("shared-kept.txt"), b"k");
    write(&doomed.join("shared-doomed.txt"), b"d");
    scanned_root(&index, &kept);
    let doomed_root = scanned_root(&index, &doomed);
    index.prepare_search_arrays().unwrap();
    assert_eq!(names(&index, "shared").1, "arrays");

    let before = index.generation().unwrap();
    index.forget_root(doomed_root).unwrap();
    assert!(index.generation().unwrap() > before);

    let (found, strategy) = names(&index, "shared");
    assert_eq!(
        found,
        vec![b"shared-kept.txt".to_vec()],
        "answered by {strategy}"
    );
}

#[test]
fn a_cursor_opened_before_a_late_scan_commit_refuses_the_rows_it_changed() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let index = open(dir.path(), false);
    std::fs::create_dir_all(&tree).unwrap();
    for number in 0..5 {
        write(&tree.join(format!("match-{number}.txt")), b"x");
    }
    let root = scanned_root(&index, &tree);

    // The scan restages match-0 with new metadata; a mutation then publishes past the scan start.
    write(&tree.join("match-0.txt"), b"grown to a different size");
    let mut session = index.begin_scan(root, ScanOptions::default()).unwrap();
    walk_into(&mut session, &NativePath::from_path(&tree)).unwrap();
    upsert(&index, root, &write(&tree.join("unrelated.txt"), b"u"));

    let opened = index
        .search(
            &Query::new("match").with_page_size(2),
            &templar_file_index::CallContext::internal(),
        )
        .unwrap();
    session.commit().unwrap();

    match index.fetch_page(
        &opened.cursor,
        0,
        5,
        &templar_file_index::CallContext::internal(),
    ) {
        Err(IndexError::SnapshotRequired { .. }) => {}
        other => panic!("a cursor read rows newer than its generation: {other:?}"),
    }
}

#[derive(Clone, Copy, Debug)]
enum Step {
    BeginScan(usize),
    CommitScan(usize),
    Mutate(usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0..2usize).prop_map(Step::BeginScan),
        (0..2usize).prop_map(Step::CommitScan),
        (0..2usize).prop_map(Step::Mutate),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4, ..ProptestConfig::default() })]

    #[test]
    fn the_published_head_is_strictly_monotonic_across_interleaved_commits(
        steps in proptest::collection::vec(step(), 1000)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = open(dir.path(), false);
        let trees = [dir.path().join("one"), dir.path().join("two")];
        let roots: Vec<RootId> = trees.iter().map(|tree| scanned_root(&index, tree)).collect();
        let mut sessions: [Option<templar_file_index::ScanSession>; 2] = [None, None];
        let mut head = index.generation().unwrap();
        let mut commits = 0u32;
        for (number, step) in steps.into_iter().enumerate() {
            let published = match step {
                Step::BeginScan(slot) => {
                    if sessions[slot].is_none() {
                        let mut session = index.begin_scan(roots[slot], ScanOptions::default()).unwrap();
                        walk_into(&mut session, &NativePath::from_path(&trees[slot])).unwrap();
                        sessions[slot] = Some(session);
                    }
                    // Every generated step publishes once, even while keeping this older
                    // scan open for a later interleaved commit.
                    let file = write(&trees[slot].join(format!("m{number}")), b"m");
                    upsert(&index, roots[slot], &file)
                }
                Step::CommitScan(slot) => {
                    let session = sessions[slot].take().unwrap_or_else(|| {
                        let mut session = index.begin_scan(roots[slot], ScanOptions::default()).unwrap();
                        walk_into(&mut session, &NativePath::from_path(&trees[slot])).unwrap();
                        session
                    });
                    session.commit().unwrap().generation
                }
                Step::Mutate(slot) => {
                    let file = write(&trees[slot].join(format!("m{number}")), b"m");
                    upsert(&index, roots[slot], &file)
                }
            };
            let now = index.generation().unwrap();
            prop_assert!(now > head, "head went from {} to {} at step {}", head, now, number);
            prop_assert_eq!(now, published);
            head = now;
            commits += 1;
        }
        prop_assert_eq!(commits, 1000);
    }
}

#[test]
fn a_rescan_that_finds_nothing_changed_writes_nothing_and_still_collects() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let index = open(dir.path(), false);
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    write(&tree.join("a"), b"a");
    write(&tree.join("sub/b"), b"b");
    write(&tree.join("sub/c"), b"c");
    let root = scanned_root(&index, &tree);
    let written_at = |generation: Generation| -> i64 {
        rusqlite::Connection::open(dir.path().join("index.sqlite3"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE written_generation = ?1",
                [generation.0 as i64],
                |row| row.get(0),
            )
            .unwrap()
    };

    let quiet = index.scan_root(root, ScanOptions::default()).unwrap();
    // Only the root directory itself, whose parent lies outside the index, is rewritten.
    assert!(
        written_at(quiet.generation) <= 1,
        "an unchanged rescan rewrote rows"
    );
    assert_eq!(quiet.entries, index.entry_count(root).unwrap());

    std::fs::remove_file(tree.join("sub/c")).unwrap();
    write(&tree.join("sub/d"), b"d");
    let subtree = index
        .scan_subtree(
            root,
            &NativePath::from_path(&tree.join("sub")),
            ScanOptions::default(),
        )
        .unwrap();
    assert_eq!(subtree.entries, index.entry_count(root).unwrap());
    assert_eq!(
        index.root_status(root).unwrap().entry_count,
        subtree.entries
    );
    let (found, _) = names(&index, "c");
    assert!(
        !found.contains(&b"c".to_vec()),
        "an unseen row survived a complete pass"
    );
    let (found, _) = names(&index, "d");
    assert!(found.contains(&b"d".to_vec()));
}
