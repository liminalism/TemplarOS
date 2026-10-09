//! The in-memory search arrays answer exactly what SQL answers: same membership, rank, order,
//! totals and native path bytes, on the first page and on every page after it.
//!
//! The oracle is the same index searched through the SQL planner (`search_with`), which never
//! consults the arrays. Every generated case builds a real tree on disk — non-UTF-8 names, names
//! that sort around the separator (`-`, `.`, `0`), hidden names, tied sizes and times, and a
//! second root registered inside the first — then compares every sort, both directions, nested
//! and duplicate `within` scopes, root subsets, tiers, kinds and size/time filters.

use proptest::prelude::*;
use std::{
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use templar_file_index::{
    Coverage, EntryKind, Index, IndexConfig, NativePath, Query, RootId, RootPolicy, ScanOptions,
    SearchResult, Sort, query::Fts5TrigramPlanner,
};

fn open(dir: &Path) -> Index {
    Index::open(
        &dir.join("index.sqlite3"),
        IndexConfig {
            search_arrays: true,
            ..IndexConfig::default()
        },
    )
    .unwrap()
}

fn scanned(index: &Index, tree: &Path) -> RootId {
    let root = index
        .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
        .unwrap()
        .root;
    index.scan_root(root, ScanOptions::default()).unwrap();
    root
}

/// Ordered `(path bytes, rank)` of every retained hit, paging through the cursor `page` rows at a
/// time exactly as a client would.
fn sequence(index: &Index, result: &SearchResult, page: u32) -> Vec<(Vec<u8>, &'static str)> {
    let mut rows: Vec<(Vec<u8>, &'static str)> = result
        .hits
        .iter()
        .map(|hit| (hit.row.path.as_bytes().to_vec(), hit.rank.as_str()))
        .collect();
    let retained = result.total.unwrap_or(0).min(100_000) as usize;
    while rows.len() < retained && !result.cursor.is_empty() {
        let fetched = index
            .fetch_page(
                &result.cursor,
                rows.len() as u32,
                page,
                &templar_file_index::CallContext::internal(),
            )
            .unwrap();
        if fetched.hits.is_empty() {
            break;
        }
        rows.extend(
            fetched
                .hits
                .iter()
                .map(|hit| (hit.row.path.as_bytes().to_vec(), hit.rank.as_str())),
        );
    }
    rows
}

fn both(index: &Index, query: &Query) -> (SearchResult, SearchResult) {
    let arrays = index
        .search(query, &templar_file_index::CallContext::internal())
        .unwrap();
    let sql = index
        .search_with(
            &Fts5TrigramPlanner,
            query,
            &templar_file_index::CallContext::internal(),
        )
        .unwrap();
    (arrays, sql)
}

/// Asserts parity and returns whether the arrays (rather than a SQL fallback) answered.
fn assert_parity(index: &Index, query: &Query) -> bool {
    let (arrays, sql) = both(index, query);
    let page = query.page_size.max(1);
    assert_eq!(arrays.total, sql.total, "total for {query:?}");
    assert_eq!(
        arrays.total_files, sql.total_files,
        "file total for {query:?}"
    );
    assert_eq!(
        arrays.total_folders, sql.total_folders,
        "folder total for {query:?}"
    );
    assert_eq!(arrays.retained, sql.retained, "retained rows for {query:?}");
    assert_eq!(arrays.truncated, sql.truncated, "truncated for {query:?}");
    assert_eq!(
        sequence(index, &arrays, page),
        sequence(index, &sql, page),
        "sequence for {query:?} (arrays answered by {})",
        arrays.strategy
    );
    arrays.strategy == "arrays"
}

#[test]
fn a_folder_and_its_own_subfolder_scope_every_match_in_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    for sub in ["parent/child", "parent/other"] {
        std::fs::create_dir_all(tree.join(sub)).unwrap();
    }
    std::fs::write(tree.join("parent/child/in-child-match.txt"), b"c").unwrap();
    std::fs::write(tree.join("parent/other/in-other-match.txt"), b"o").unwrap();
    let index = open(dir.path());
    scanned(&index, &tree);
    index.prepare_search_arrays().unwrap();

    let parent = NativePath::from_path(&tree.join("parent"));
    let child = NativePath::from_path(&tree.join("parent/child"));
    let mut query = Query::new("match");
    query.within = vec![parent, child];
    let (arrays, sql) = both(&index, &query);
    assert_eq!(arrays.strategy, "arrays");
    assert_eq!(sql.total, Some(2));
    assert!(assert_parity(&index, &query));
}

#[test]
fn duplicate_and_adjacent_scopes_do_not_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    for sub in ["a", "a-b", "a.c", "a/z"] {
        std::fs::create_dir_all(tree.join(sub)).unwrap();
        std::fs::write(tree.join(sub).join("hit.txt"), b"h").unwrap();
    }
    let index = open(dir.path());
    scanned(&index, &tree);
    index.prepare_search_arrays().unwrap();
    let path = |sub: &str| NativePath::from_path(&tree.join(sub));
    let mut query = Query::new("hit");
    query.within = vec![path("a"), path("a"), path("a-b"), path("a/z"), path("a.c")];
    assert!(assert_parity(&index, &query));
    assert_eq!(
        index
            .search(&query, &templar_file_index::CallContext::internal())
            .unwrap()
            .total,
        Some(4)
    );
}

#[test]
fn a_nested_root_sorts_into_its_parent_in_path_order() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    for sub in ["proj/src", "proj-old/src", "proj.bak", "zz"] {
        std::fs::create_dir_all(tree.join(sub)).unwrap();
        std::fs::write(tree.join(sub).join("main-hit.rs"), b"m").unwrap();
    }
    let index = open(dir.path());
    let outer = scanned(&index, &tree);
    let nested = scanned(&index, &tree.join("proj"));
    // The container gives the nested subtree up at its next complete pass.
    index.scan_root(outer, ScanOptions::default()).unwrap();
    index.prepare_search_arrays().unwrap();
    for sort in [Sort::Path, Sort::Name, Sort::Relevance] {
        for descending in [false, true] {
            let mut query = Query::new("hit");
            query.sort = sort;
            query.descending = descending;
            assert!(assert_parity(&index, &query), "{sort:?} {descending}");
            query.within = vec![NativePath::from_path(&tree)];
            assert!(assert_parity(&index, &query));
            query.roots = vec![nested];
            assert!(assert_parity(&index, &query));
        }
    }
    let mut query = Query::new("hit");
    query.sort = Sort::Path;
    let paths: Vec<Vec<u8>> = sequence(
        &index,
        &index
            .search(&query, &templar_file_index::CallContext::internal())
            .unwrap(),
        100,
    )
    .into_iter()
    .map(|(path, _)| path)
    .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "global path order across the nested root");
    assert_eq!(paths.len(), 4);
}

#[test]
fn paging_through_arrays_returns_exactly_the_sql_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    for number in 0..300 {
        std::fs::write(
            tree.join(format!("page-{number:03}")),
            vec![0u8; number % 7],
        )
        .unwrap();
    }
    let index = open(dir.path());
    scanned(&index, &tree);
    index.prepare_search_arrays().unwrap();
    for page_size in [1, 7, 100] {
        for sort in [
            Sort::Relevance,
            Sort::Name,
            Sort::Path,
            Sort::Size,
            Sort::Mtime,
        ] {
            let mut query = Query::new("page").with_page_size(page_size);
            query.sort = sort;
            assert!(assert_parity(&index, &query), "{sort:?} {page_size}");
        }
    }
}

// ---- the generated cases --------------------------------------------------------------------

const NAME_BYTES: &[u8] = b"abA-.0x\xff\xfe";
const NEEDLE_BYTES: &[u8] = b"abA-.0x";

#[derive(Clone, Debug)]
struct GenEntry {
    parent: u16,
    name: Vec<u8>,
    directory: bool,
    size: u64,
    mtime: u8,
}

fn name() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(NAME_BYTES), 1..6)
        .prop_filter("not . or ..", |name| {
            name.as_slice() != b"." && name.as_slice() != b".."
        })
}

fn entry() -> impl Strategy<Value = GenEntry> {
    (
        any::<u16>(),
        name(),
        prop::bool::weighted(0.3),
        0..4u64,
        0..3u8,
    )
        .prop_map(|(parent, name, directory, size, mtime)| GenEntry {
            parent,
            name,
            directory,
            size,
            mtime,
        })
}

#[derive(Clone, Debug)]
struct GenQuery {
    needle: Vec<u8>,
    path_needle: bool,
    within: Vec<u16>,
    within_root: bool,
    within_missing: bool,
    roots: u8,
    sort: Sort,
    descending: bool,
    hidden: bool,
    coverage: u8,
    kinds: u8,
    min_size: Option<u64>,
    max_mtime: Option<u8>,
    page: u32,
}

fn query() -> impl Strategy<Value = GenQuery> {
    (
        prop::collection::vec(prop::sample::select(NEEDLE_BYTES), 1..4),
        prop::bool::weighted(0.1),
        prop::collection::vec(any::<u16>(), 0..4),
        (prop::bool::weighted(0.1), prop::bool::weighted(0.05)),
        0..4u8,
        prop::sample::select(vec![
            Sort::Relevance,
            Sort::Name,
            Sort::Path,
            Sort::Size,
            Sort::Mtime,
        ]),
        any::<bool>(),
        any::<bool>(),
        0..8u8,
        0..3u8,
        (prop::option::of(0..4u64), prop::option::of(0..3u8)),
        prop::sample::select(vec![1u32, 7, 100]),
    )
        .prop_map(
            |(
                needle,
                path_needle,
                within,
                (within_root, within_missing),
                roots,
                sort,
                descending,
                hidden,
                coverage,
                kinds,
                (min_size, max_mtime),
                page,
            )| GenQuery {
                needle,
                path_needle,
                within,
                within_root,
                within_missing,
                roots,
                sort,
                descending,
                hidden,
                coverage,
                kinds,
                min_size,
                max_mtime,
                page,
            },
        )
}

fn epoch(bucket: u8) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + u64::from(bucket) * 1000)
}

/// Writes the generated tree under `base` and returns every directory created (base first).
fn build(base: &Path, entries: &[GenEntry]) -> Vec<PathBuf> {
    std::fs::create_dir_all(base).unwrap();
    let mut directories = vec![base.to_path_buf()];
    let mut files = Vec::new();
    for entry in entries {
        let parent = directories[entry.parent as usize % directories.len()].clone();
        let path = parent.join(std::ffi::OsString::from_vec(entry.name.clone()));
        if path.symlink_metadata().is_ok() {
            continue;
        }
        if entry.directory {
            std::fs::create_dir(&path).unwrap();
            directories.push(path.clone());
        } else {
            std::fs::write(&path, vec![b'x'; entry.size as usize * 3]).unwrap();
        }
        files.push((path, entry.mtime));
    }
    // Times last, deepest first, so creating a child never moves its parent's stamp again.
    files.reverse();
    for (path, mtime) in files {
        let handle = std::fs::File::open(&path).unwrap();
        handle.set_modified(epoch(mtime)).unwrap();
    }
    directories
}

fn to_query(generated: &GenQuery, directories: &[PathBuf], roots: &[(RootId, PathBuf)]) -> Query {
    let mut text = String::from_utf8(generated.needle.clone()).unwrap();
    if generated.path_needle {
        text = format!("{text}/");
    }
    let mut query = Query::new(text).with_page_size(generated.page);
    for pick in &generated.within {
        query.within.push(NativePath::from_path(
            &directories[*pick as usize % directories.len()],
        ));
    }
    if generated.within_root {
        query
            .within
            .push(NativePath::from_path(&roots[roots.len() - 1].1));
    }
    if generated.within_missing {
        query.within.push(NativePath::from_path(
            &directories[0].join("no-such-directory"),
        ));
    }
    query.roots = roots
        .iter()
        .enumerate()
        .filter(|(slot, _)| generated.roots & (1 << slot) != 0)
        .map(|(_, (root, _))| *root)
        .collect();
    query.sort = generated.sort;
    query.descending = generated.descending;
    query.include_hidden = generated.hidden;
    query.include_coverage = [Coverage::Opaque, Coverage::Inventory, Coverage::Detailed]
        .into_iter()
        .enumerate()
        .filter(|(bit, _)| generated.coverage & (1 << bit) != 0)
        .map(|(_, tier)| tier)
        .collect();
    query.kinds = match generated.kinds {
        1 => Some(vec![EntryKind::Regular]),
        2 => Some(vec![EntryKind::Directory]),
        _ => None,
    };
    query.min_size = generated.min_size.map(|bucket| bucket * 3);
    query.max_mtime_ns = generated
        .max_mtime
        .map(|bucket| 1_700_000_000_000_000_000 + i64::from(bucket) * 1_000_000_000_000);
    query
}

fn run_case(trees: &[Vec<GenEntry>], nest_at: u16, queries: &[GenQuery]) -> u32 {
    let dir = tempfile::tempdir().unwrap();
    let index = open(dir.path());
    let mut directories = Vec::new();
    let mut roots = Vec::new();
    for (number, entries) in trees.iter().enumerate() {
        let base = dir.path().join(format!("root{number}"));
        let made = build(&base, entries);
        let root = scanned(&index, &base);
        roots.push((root, base));
        directories.extend(made);
    }
    // A second registration inside the first root, owned from its path down.
    let candidates: Vec<&PathBuf> = directories
        .iter()
        .filter(|path| path.starts_with(&roots[0].1) && **path != roots[0].1)
        .collect();
    if !candidates.is_empty() {
        let inner = candidates[nest_at as usize % candidates.len()].clone();
        let nested = scanned(&index, &inner);
        index.scan_root(roots[0].0, ScanOptions::default()).unwrap();
        roots.push((nested, inner));
    }
    index.prepare_search_arrays().unwrap();
    let mut answered = 0;
    for generated in queries {
        let query = to_query(generated, &directories, &roots);
        if assert_parity(&index, &query) {
            answered += 1;
        }
    }
    answered
}

fn cases() -> impl Strategy<Value = (Vec<Vec<GenEntry>>, u16, Vec<GenQuery>)> {
    (
        prop::collection::vec(prop::collection::vec(entry(), 50..400), 1..3),
        any::<u16>(),
        prop::collection::vec(query(), 12),
    )
}

fn config(cases: u32, seed: Option<[u8; 32]>) -> ProptestConfig {
    let mut config = ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    if let Some(seed) = seed {
        config.rng_algorithm = prop::test_runner::RngAlgorithm::ChaCha;
        config.rng_seed =
            prop::test_runner::RngSeed::Fixed(u64::from_le_bytes(seed[..8].try_into().unwrap()));
    }
    config
}

fn run(cases_to_run: u32, seed: Option<[u8; 32]>) {
    let mut runner = prop::test_runner::TestRunner::new(config(cases_to_run, seed));
    let answered = std::cell::Cell::new(0u32);
    runner
        .run(&cases(), |(trees, nest_at, queries)| {
            answered.set(answered.get() + run_case(&trees, nest_at, &queries));
            Ok(())
        })
        .unwrap();
    assert!(
        answered.get() > 0,
        "the arrays never answered; parity proved nothing"
    );
}

/// 512 cases on the default (random) seed, the acceptance count.
#[test]
fn the_arrays_answer_every_generated_query_exactly_as_sql_does() {
    run(512, None);
}

/// Three recorded seeds, so a regression reproduces on every run and not only on a lucky one.
#[test]
fn the_recorded_seeds_still_agree() {
    for seed in [[1u8; 32], [0x5a; 32], [0xc3; 32]] {
        run(512, Some(seed));
    }
}

#[test]
fn a_dirty_root_does_not_disable_arrays_for_other_roots() {
    use templar_file_index::{EntryMeta, Mutation, MutationBatch, MutationSource};
    let dir = tempfile::tempdir().unwrap();
    let quiet = dir.path().join("quiet");
    let churn = dir.path().join("build");
    for tree in [&quiet, &churn] {
        std::fs::create_dir(tree).unwrap();
        std::fs::write(tree.join("hit-original"), b"a").unwrap();
    }
    let index = open(dir.path());
    let quiet_id = scanned(&index, &quiet);
    let churn_id = scanned(&index, &churn);
    index.prepare_search_arrays().unwrap();
    let quiet_generation = index.root_status(quiet_id).unwrap().generation;
    for number in 0..12 {
        let path = churn.join(format!("hit-new-{number}"));
        std::fs::write(&path, b"churn").unwrap();
        index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Upsert {
                    root: churn_id,
                    path: NativePath::from_path(&path),
                    meta: EntryMeta::from_symlink_metadata(
                        &path,
                        &std::fs::symlink_metadata(&path).unwrap(),
                    ),
                }],
            ))
            .unwrap();
        assert_eq!(
            index.root_status(quiet_id).unwrap().generation,
            quiet_generation
        );
        for sort in [
            Sort::Relevance,
            Sort::Name,
            Sort::Path,
            Sort::Size,
            Sort::Mtime,
        ] {
            let mut query = Query::new("hit");
            query.sort = sort;
            query.roots = vec![quiet_id];
            assert!(
                assert_parity(&index, &query),
                "quiet root fell back after {number} writes"
            );
            query.roots.clear();
            query.within = vec![NativePath::from_path(&quiet)];
            assert!(
                assert_parity(&index, &query),
                "within did not isolate the quiet shard"
            );
        }
        // Rebuild deterministically for the global parity check, independent of the host's
        // optional maintenance worker and its throttle.
        index.prepare_search_arrays().unwrap();
        assert!(assert_parity(&index, &Query::new("hit")));
    }
}

#[test]
fn root_paths_that_sort_before_and_after_another_roots_row_merge_like_sql() {
    let dir = tempfile::tempdir().unwrap();
    let index = open(dir.path());
    for name in ["hit", "hit-old", "hit.old", "hit0"] {
        let tree = dir.path().join(name);
        std::fs::create_dir_all(tree.join("inside")).unwrap();
        std::fs::write(tree.join("inside/same-hit"), b"same").unwrap();
        scanned(&index, &tree);
    }
    index.prepare_search_arrays().unwrap();
    for sort in [
        Sort::Relevance,
        Sort::Name,
        Sort::Path,
        Sort::Size,
        Sort::Mtime,
    ] {
        for descending in [false, true] {
            let mut query = Query::new("hit").with_page_size(2);
            query.sort = sort;
            query.descending = descending;
            assert!(assert_parity(&index, &query), "{sort:?} {descending}");
        }
    }
}

#[test]
fn nested_shards_keep_foreign_ancestor_matches_without_retaining_foreign_generations() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    let inner = tree.join("hit-parent/child");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("unrelated"), b"u").unwrap();
    let index = open(dir.path());
    let outer = scanned(&index, &tree);
    let nested = scanned(&index, &inner);
    index.scan_root(outer, ScanOptions::default()).unwrap();
    index.prepare_search_arrays().unwrap();
    let mut query = Query::new("hit");
    query.roots = vec![nested];
    assert!(assert_parity(&index, &query));
    assert_eq!(both(&index, &query).0.total, Some(2));
}

#[test]
fn a_reused_entry_id_is_never_matched_to_its_old_shard_row() {
    use templar_file_index::{EntryMeta, Mutation, MutationBatch, MutationSource};
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let old = tree.join("old-hit");
    std::fs::write(&old, b"old").unwrap();
    let index = Index::open(
        &dir.path().join("index.sqlite3"),
        IndexConfig {
            search_arrays: true,
            tombstone_retention_generations: 0,
            ..IndexConfig::default()
        },
    )
    .unwrap();
    let root = scanned(&index, &tree);
    let old_id = index
        .entry(root, &NativePath::from_path(&old))
        .unwrap()
        .unwrap()
        .entry_id;
    index.prepare_search_arrays().unwrap();
    std::fs::remove_file(&old).unwrap();
    index
        .apply(MutationBatch::new(
            MutationSource::Watcher,
            vec![Mutation::Remove {
                root,
                path: NativePath::from_path(&old),
            }],
        ))
        .unwrap();
    // A complete scan runs the production tombstone collector with retention zero.
    index.scan_root(root, ScanOptions::default()).unwrap();
    let new = tree.join("new-hit");
    std::fs::write(&new, b"new").unwrap();
    index
        .apply(MutationBatch::new(
            MutationSource::Watcher,
            vec![Mutation::Upsert {
                root,
                path: NativePath::from_path(&new),
                meta: EntryMeta::from_symlink_metadata(
                    &new,
                    &std::fs::symlink_metadata(&new).unwrap(),
                ),
            }],
        ))
        .unwrap();
    assert_eq!(
        index
            .entry(root, &NativePath::from_path(&new))
            .unwrap()
            .unwrap()
            .entry_id,
        old_id,
        "fixture did not reuse the maximum row id"
    );
    let (answer, reference) = both(&index, &Query::new("old"));
    assert_eq!(answer.total, Some(0));
    assert_eq!(answer.total, reference.total);
    assert!(answer.hits.is_empty(), "stale shard aliased the new row");
    index.prepare_search_arrays().unwrap();
    assert!(assert_parity(&index, &Query::new("new")));
}

#[derive(Debug)]
struct ImmediateMaintenance;
impl templar_file_index::MaintenanceExecutor for ImmediateMaintenance {
    fn submit(&self, _: &'static str, job: Box<dyn FnOnce() + Send>, _: Box<dyn FnOnce() + Send>) {
        job();
    }
}

fn mutate_from_disk(index: &Index, root: RootId, path: &Path, old: Option<&Path>) {
    use templar_file_index::{EntryMeta, Mutation, MutationBatch, MutationSource};
    let native = NativePath::from_path(path);
    let mutation = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let meta = EntryMeta::from_symlink_metadata(path, &metadata);
            if let Some(old) = old {
                Mutation::Move {
                    root,
                    from: NativePath::from_path(old),
                    to: native,
                    meta,
                }
            } else {
                Mutation::Upsert {
                    root,
                    path: native,
                    meta,
                }
            }
        }
        Err(_) => Mutation::Remove { root, path: native },
    };
    index
        .apply(MutationBatch::new(MutationSource::Watcher, vec![mutation]))
        .unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "each property journey mutates disk and index, then compares both planners after every step"
)]
fn run_overlay_mutations(operations: &[(u8, u8)]) {
    let scratch = tempfile::tempdir().unwrap();
    let index = open(scratch.path());
    index.set_maintenance_executor(std::sync::Arc::new(ImmediateMaintenance));
    let left = scratch.path().join("left");
    let right = scratch.path().join("right");
    let mut directories = vec![
        left.join("hit-dir"),
        left.join("plain"),
        right.join("hit-dir"),
    ];
    let mut paths = Vec::new();
    for directory in &directories {
        std::fs::create_dir_all(directory).unwrap();
        for name in [
            b"hit".as_slice(),
            b"same-hit.txt",
            b"native-\xff-hit",
            b".hidden-hit",
            b"unrelated",
        ] {
            let path = directory.join(std::ffi::OsString::from_vec(name.to_vec()));
            std::fs::write(&path, b"tie").unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(epoch(1))
                .unwrap();
            paths.push(path);
        }
    }
    let left_id = scanned(&index, &left);
    let right_id = scanned(&index, &right);
    let nested_path = left.join("hit-dir/nested");
    std::fs::create_dir_all(&nested_path).unwrap();
    std::fs::write(nested_path.join("unrelated"), b"nested").unwrap();
    let nested = scanned(&index, &nested_path);
    index.scan_root(left_id, ScanOptions::default()).unwrap();
    index.prepare_search_arrays().unwrap();
    let mut overlaid = 0;
    for (step, (operation, pick)) in operations.iter().enumerate() {
        let directory = *pick as usize % directories.len();
        let root = if directory == 2 { right_id } else { left_id };
        match operation % 5 {
            0 => {
                let path = directories[directory].join(format!("hit-new-{step}"));
                std::fs::write(&path, vec![b'n'; step % 7]).unwrap();
                mutate_from_disk(&index, root, &path, None);
                paths.push(path);
            }
            1 if !paths.is_empty() => {
                let choice = *pick as usize % paths.len();
                let path = paths.remove(choice);
                let root = if path.starts_with(&right) {
                    right_id
                } else {
                    left_id
                };
                std::fs::remove_file(&path).unwrap();
                mutate_from_disk(&index, root, &path, None);
            }
            2 if !paths.is_empty() => {
                let choice = *pick as usize % paths.len();
                let old = paths[choice].clone();
                let path = old.with_file_name(format!("renamed-hit-{step}"));
                let root = if old.starts_with(&right) {
                    right_id
                } else {
                    left_id
                };
                std::fs::rename(&old, &path).unwrap();
                mutate_from_disk(&index, root, &path, Some(&old));
                paths[choice] = path;
            }
            3 => {
                // Keep the registered nested root's ancestor fixed. The other directory moves
                // exercise both adding and removing path-only matches for unchanged basenames.
                let directory = if directory == 0 { 1 } else { directory };
                let old = directories[directory].clone();
                let path = old.with_file_name(if step % 2 == 0 {
                    format!("hit-moved-{step}")
                } else {
                    format!("plain-moved-{step}")
                });
                let root = if directory == 2 { right_id } else { left_id };
                std::fs::rename(&old, &path).unwrap();
                mutate_from_disk(&index, root, &path, Some(&old));
                for file in &mut paths {
                    if let Ok(suffix) = file.strip_prefix(&old) {
                        *file = path.join(suffix);
                    }
                }
                directories[directory] = path;
            }
            _ if !paths.is_empty() => {
                let path = &paths[*pick as usize % paths.len()];
                let root = if path.starts_with(&right) {
                    right_id
                } else {
                    left_id
                };
                std::fs::write(path, vec![b'm'; step % 11]).unwrap();
                std::fs::File::open(path)
                    .unwrap()
                    .set_modified(epoch((step % 3) as u8))
                    .unwrap();
                mutate_from_disk(&index, root, path, None);
            }
            _ => {}
        }
        let global = both(&index, &Query::new("hit")).0;
        if global.strategy == "arrays+overlay" {
            overlaid += 1;
        }
        assert_parity(&index, &Query::new("hit"));
        for sort in [
            Sort::Relevance,
            Sort::Name,
            Sort::Path,
            Sort::Size,
            Sort::Mtime,
        ] {
            for descending in [false, true] {
                for needle in ["hit", "plain", "unrelated"] {
                    let mut query = Query::new(needle).with_page_size(2);
                    query.sort = sort;
                    query.descending = descending;
                    query.include_hidden = *pick % 2 == 0;
                    match *pick % 4 {
                        0 => query.roots = vec![root],
                        1 => query.within = vec![NativePath::from_path(&directories[directory])],
                        2 => query.roots = vec![nested],
                        _ => {}
                    }
                    if step % 3 == 0 {
                        query.min_size = Some(1);
                    }
                    if step % 4 == 0 {
                        query.kinds = Some(vec![EntryKind::Regular]);
                    }
                    let answer = both(&index, &query).0;
                    if answer.strategy == "arrays+overlay" {
                        overlaid += 1;
                    }
                    if sort == Sort::Path && answer.strategy == "arrays+overlay" {
                        // Removal-only projections can still preserve the base path ordering.
                        let roots = index.search_array_stats().unwrap().roots;
                        assert!(roots.iter().any(|root| root.overlay_rows != 0));
                    }
                    assert_parity(&index, &query);
                }
            }
        }
    }
    assert!(
        overlaid > 0,
        "mutation fixture never exercised an overlay answer"
    );
}

#[test]
fn overlays_match_sql_after_random_committed_mutation_sequences() {
    let mut runner = prop::test_runner::TestRunner::new(config(48, Some([0x31; 32])));
    runner
        .run(
            &prop::collection::vec((any::<u8>(), any::<u8>()), 12..24),
            |operations| {
                run_overlay_mutations(&operations);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn overlays_merge_deleted_moved_and_recreated_native_rows_in_every_order() {
    run_overlay_mutations(&[
        (0, 0),
        (0, 2),
        (1, 0),
        (2, 3),
        (3, 1),
        (4, 2),
        (3, 2),
        (0, 1),
        (2, 0),
        (1, 1),
    ]);
}

#[test]
fn a_directory_move_beyond_the_overlay_bound_falls_back_to_sql() {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Held(Mutex<VecDeque<Box<dyn FnOnce() + Send>>>);
    impl templar_file_index::MaintenanceExecutor for Held {
        fn submit(
            &self,
            _: &'static str,
            job: Box<dyn FnOnce() + Send>,
            _: Box<dyn FnOnce() + Send>,
        ) {
            self.0.lock().unwrap().push_back(job);
        }
    }
    let scratch = tempfile::tempdir().unwrap();
    let tree = scratch.path().join("tree");
    let old = tree.join("old");
    std::fs::create_dir_all(&old).unwrap();
    for number in 0..10_001 {
        std::fs::write(old.join(format!("f{number}")), b"f").unwrap();
    }
    let index = open(scratch.path());
    let root = scanned(&index, &tree);
    index.prepare_search_arrays().unwrap();
    let held = Arc::new(Held::default());
    index.set_maintenance_executor(held.clone());
    let moved = tree.join("hit-moved");
    std::fs::rename(&old, &moved).unwrap();
    mutate_from_disk(&index, root, &moved, Some(&old));
    let job = held
        .0
        .lock()
        .unwrap()
        .pop_front()
        .expect("committed overlay update");
    job(); // A bounded update refuses the subtree and queues a throttled base rebuild.
    let query = Query::new("hit");
    let answer = both(&index, &query).0;
    assert!(!answer.strategy.starts_with("arrays"));
    assert_eq!(answer.total, Some(10_002));
    assert!(
        index.search_array_stats().unwrap().roots[0]
            .refusal
            .as_ref()
            .unwrap()
            .contains("bound")
    );
    assert_parity(&index, &query);
    held.0.lock().unwrap().clear();
}
