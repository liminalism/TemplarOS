//! The measurement that decides whether a Btrfs metadata delta can replace a recursive
//! reconciliation for the period the index was not watching.
//!
//! The tests that need a real `btrfs send` are `#[ignore]`d and do nothing unless
//! `LEGE_BTRFS_FIXTURE` names a mounted Btrfs fixture, because this host is ext4 end-to-end.
//! `scripts/btrfs-index-fixture.sh` builds that fixture out of a loopback image; it is the one
//! privileged step, and it is the user's to run.
//!
//! One test here does *not* need Btrfs and always runs: a hand-written [`btrfs_delta::DeltaPlan`]
//! applied to a real tree in a temporary directory. A plan is just paths, so everything after the
//! decoder — placing a subvolume-relative path in the live tree, stat-ing it, and landing the row
//! at the right address — can be proved on any filesystem. That is the half that was wrong the
//! first time the privileged harness ran, and it should not take a root shell to catch it again.
//!
//! Two questions, in this order.
//!
//! **Is `baseline + delta` the same index as a fresh scan?** Not "close enough" — the same rows,
//! the same paths, the same metadata. Anything less and the backend is a way of putting quiet
//! errors into the index, which is worse than a crawl. The harness builds three indexes for every
//! scenario: one carried forward by deltas, one carried forward by the existing recursive
//! reconciliation, and one built from nothing against the final tree. The delta index must equal
//! the fresh one field for field.
//!
//! **Does catch-up cost track the change rather than the tree?** That is the whole architectural
//! claim. The same change is applied to trees of very different sizes; if the delta's cost moves
//! with the tree, the idea is dead regardless of how well it works.
//!
//! The fixture mounts under the temporary directory on purpose: `watch::guard` panics on a watch
//! root outside it, which is exactly the protection that makes this runnable as an ordinary test.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use lege_file_index::{
    Index, IndexConfig, NativePath, RootId, RootPolicy, ScanOptions, ScanScope,
    btrfs_delta::{self, DeltaReport, DeltaTarget, FsMetadata},
    reconcile::{self, Trigger},
    watch::{FakeSource, WatchBudget, WatchOwner},
};

// --- fixture plumbing ---------------------------------------------------------------------------

/// The mounted fixture, or `None` when this run has nothing to measure against.
fn fixture() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("LEGE_BTRFS_FIXTURE")?);
    if !path.join("@home").is_dir() {
        eprintln!(
            "LEGE_BTRFS_FIXTURE={} has no @home subvolume; run \
             `sudo scripts/btrfs-index-fixture.sh setup` first",
            path.display()
        );
        return None;
    }
    Some(path)
}

fn btrfs(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("btrfs")
        .args(args)
        .output()
        .map_err(|error| format!("spawning btrfs {args:?}: {error}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    // The single most likely failure, and the one worth naming: BTRFS_IOC_SEND is privileged.
    if stderr.contains("Operation not permitted") || stderr.contains("not permitted") {
        return Err(format!(
            "btrfs {args:?} was refused ({stderr}). btrfs send needs CAP_SYS_ADMIN; run the \
             harness through `sudo scripts/btrfs-index-fixture.sh measure`, which builds as you \
             and runs only the built test binary as root."
        ));
    }
    Err(format!("btrfs {args:?} failed: {stderr}"))
}

struct Fixture {
    home: PathBuf,
    snapshots: PathBuf,
    streams: PathBuf,
    databases: PathBuf,
}

impl Fixture {
    fn new(mount: &Path) -> Self {
        let fixture = Self {
            home: mount.join("@home"),
            snapshots: mount.join("@snapshots"),
            streams: mount.join("streams"),
            databases: mount.join("index-db"),
        };
        for dir in [&fixture.streams, &fixture.databases] {
            let _ = fs::create_dir_all(dir);
        }
        fixture
    }

    /// A read-only snapshot of `@home` — the checkpoint a dormant index would be resuming from.
    fn checkpoint(&self, name: &str) -> Result<PathBuf, String> {
        let destination = self.snapshots.join(name);
        if destination.exists() {
            Self::discard(&destination)?;
        }
        btrfs(&[
            "subvolume",
            "snapshot",
            "-r",
            &self.home.to_string_lossy(),
            &destination.to_string_lossy(),
        ])?;
        Ok(destination)
    }

    fn discard(path: &Path) -> Result<(), String> {
        btrfs(&["subvolume", "delete", &path.to_string_lossy()]).map(|_| ())
    }

    /// `btrfs send --no-data -p <parent> <child>` into a file, and how long it took.
    fn delta(&self, parent: &Path, child: &Path, name: &str) -> Result<(PathBuf, Cost), String> {
        let out = self.streams.join(format!("{name}.stream"));
        let _ = fs::remove_file(&out);
        let (result, cost) = measure(|| {
            btrfs(&[
                "send",
                "--no-data",
                "-p",
                &parent.to_string_lossy(),
                "-f",
                &out.to_string_lossy(),
                &child.to_string_lossy(),
            ])
        });
        result?;
        Ok((out, cost))
    }
}

// --- cost accounting ----------------------------------------------------------------------------

/// Wall clock, CPU and read I/O across one phase. CPU comes from `schedstat` field 0 (nanoseconds
/// on-CPU) rather than `utime`/`stime`, which are quantised to clock ticks and would report a fast
/// delta as zero. Read I/O is `/proc/self/io`, which is process-wide and therefore only meaningful
/// when one phase runs at a time.
#[derive(Clone, Copy, Debug, Default)]
struct Cost {
    wall: Duration,
    cpu: Duration,
    read_bytes: u64,
}

impl Cost {
    fn row(self) -> String {
        format!(
            "{:>9.1} | {:>9.1} | {:>10}",
            self.wall.as_secs_f64() * 1000.0,
            self.cpu.as_secs_f64() * 1000.0,
            human_bytes(self.read_bytes)
        )
    }
}

/// `/proc/thread-self`, not `/proc/self`: a `schedstat` read through the process entry reports the
/// *main* thread, and a `cargo test` harness runs every test on a worker thread — which is how a
/// CPU-bound pass reads as zero milliseconds.
fn schedstat_ns() -> u64 {
    fs::read_to_string("/proc/thread-self/schedstat")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn read_bytes() -> u64 {
    fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("read_bytes:")?.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn measure<T>(body: impl FnOnce() -> T) -> (T, Cost) {
    let (cpu, io, clock) = (schedstat_ns(), read_bytes(), Instant::now());
    let value = body();
    let cost = Cost {
        wall: clock.elapsed(),
        cpu: Duration::from_nanos(schedstat_ns().saturating_sub(cpu)),
        read_bytes: read_bytes().saturating_sub(io),
    };
    (value, cost)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// --- the index under test -------------------------------------------------------------------

/// Every field the index stores that a scan can prove, keyed by path. This is the comparison the
/// lane lives or dies by: a delta that got any of these wrong would be silently corrupting rows.
type Fingerprint = BTreeMap<Vec<u8>, EntryFacts>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryFacts {
    kind: i64,
    size_bytes: i64,
    alloc_bytes: i64,
    mtime_ns: i64,
    ctime_ns: i64,
    mode: i64,
    hidden: i64,
    dev: Option<i64>,
    inode: Option<i64>,
    nlink: i64,
}

fn fingerprint(database: &Path) -> Fingerprint {
    let connection =
        rusqlite::Connection::open(database).expect("opening the index for comparison");
    let mut statement = connection
        .prepare(
            "SELECT path_bytes, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden,
                    dev, inode, nlink
               FROM entries WHERE deleted_generation IS NULL ORDER BY path_bytes",
        )
        .expect("preparing the comparison query");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                EntryFacts {
                    kind: row.get(1)?,
                    size_bytes: row.get(2)?,
                    alloc_bytes: row.get(3)?,
                    mtime_ns: row.get(4)?,
                    ctime_ns: row.get(5)?,
                    mode: row.get(6)?,
                    hidden: row.get(7)?,
                    dev: row.get(8)?,
                    inode: row.get(9)?,
                    nlink: row.get(10)?,
                },
            ))
        })
        .expect("reading rows");
    rows.map(|row| row.expect("a row")).collect()
}

/// What differs between two indexes, as text a person can act on.
fn describe_difference(delta: &Fingerprint, fresh: &Fingerprint) -> Option<String> {
    let mut problems = Vec::new();
    for (path, facts) in fresh {
        match delta.get(path) {
            None => problems.push(format!("missing from the delta index: {}", show(path))),
            Some(other) if other != facts => problems.push(format!(
                "differs at {}:\n    delta: {other:?}\n    fresh: {facts:?}",
                show(path)
            )),
            Some(_) => {}
        }
    }
    for path in delta.keys() {
        if !fresh.contains_key(path) {
            problems.push(format!("only in the delta index: {}", show(path)));
        }
    }
    if problems.is_empty() {
        return None;
    }
    let total = problems.len();
    problems.truncate(20);
    Some(format!(
        "{total} difference(s); first {}:\n  {}",
        problems.len(),
        problems.join("\n  ")
    ))
}

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Open an index, run `body`, and close it — the close matters, because the harness copies the
/// SQLite file between arms and WAL only folds back in on the last connection closing.
fn with_index<T>(database: &Path, body: impl FnOnce(&Index) -> T) -> T {
    let index = Index::open(database, IndexConfig::default()).expect("opening the index");
    let value = body(&index);
    drop(index);
    value
}

fn copy_database(from: &Path, to: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut source = from.as_os_str().to_os_string();
        source.push(suffix);
        let mut destination = to.as_os_str().to_os_string();
        destination.push(suffix);
        let (source, destination) = (PathBuf::from(source), PathBuf::from(destination));
        let _ = fs::remove_file(&destination);
        if source.exists() {
            fs::copy(&source, &destination).expect("copying the baseline index");
        }
    }
}

/// A full recursive pass — the thing the delta is competing with.
fn full_reconcile(index: &Index, root: RootId, tree: &Path, trigger: &Trigger) -> u64 {
    let policy = index.root_policy(root).expect("the root's policy");
    // The budget has to cover the tree. A refused watch now makes the pass incomplete — an
    // unwatched subtree that reports `current` is the lie `ScanSession::commit` exists to refuse —
    // and an incomplete pass collects nothing, which would leave deleted files in the very index
    // this function exists to be the reference for. `FakeSource` arms in memory, so the walk is
    // still what is being measured.
    let mut owner = WatchOwner::new(
        FakeSource::new(),
        WatchBudget {
            max_watches: u64::MAX,
        },
    );
    owner.track_root(root, tree, policy.clone());
    let (outcome, _) = reconcile::reconcile(
        index,
        &mut owner,
        root,
        ScanScope::Root,
        trigger,
        ScanOptions::reconcile(policy),
    )
    .expect("a reconcile pass");
    assert!(outcome.complete, "the reference pass must be complete");
    outcome.entries
}

fn add_root(index: &Index, tree: &Path) -> RootId {
    index
        .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
        .expect("adding the root")
        .root
}

// --- the tree and the scenarios -----------------------------------------------------------------

/// A tree shaped like a home directory: some breadth, some depth, a policy-excluded build
/// directory, and one file per leaf.
fn build_tree(root: &Path, files: usize) {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).expect("creating the tree root");
    // 32 files per leaf directory, 16 leaves per branch — close enough to a real home's shape that
    // the directory-to-file ratio is not the thing being measured.
    let mut made = 0usize;
    let mut branch = 0usize;
    while made < files {
        let branch_dir = root.join(format!("branch{branch:03}"));
        for leaf in 0..16 {
            let leaf_dir = branch_dir.join(format!("leaf{leaf:02}"));
            fs::create_dir_all(&leaf_dir).expect("creating a leaf");
            for file in 0..32 {
                if made >= files {
                    break;
                }
                fs::write(leaf_dir.join(format!("f{file:02}.txt")), b"x").expect("writing a file");
                made += 1;
            }
            if made >= files {
                break;
            }
        }
        branch += 1;
    }
    // A build directory the policy excludes: the delta must not smuggle these into the index.
    let excluded = root.join("project/target/debug");
    fs::create_dir_all(&excluded).expect("creating the excluded directory");
    for file in 0..64 {
        fs::write(excluded.join(format!("{file}.o")), b"obj").expect("writing an object file");
    }
    fs::create_dir_all(root.join("project/src")).expect("creating src");
    fs::write(root.join("project/src/main.rs"), b"fn main() {}").expect("writing src");
    // A subtree the mass-delete scenario removes wholesale.
    let bulk = root.join("bulk");
    fs::create_dir_all(&bulk).expect("creating bulk");
    for file in 0..(files / 2).max(64) {
        fs::write(bulk.join(format!("b{file:06}")), b"b").expect("writing a bulk file");
    }
}

struct Scenario {
    name: &'static str,
    /// What the user did while the index was not watching.
    apply: fn(&Path),
}

#[allow(clippy::too_many_lines)]
fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "nothing at all",
            apply: |_| {},
        },
        Scenario {
            name: "100 new files",
            apply: |root| {
                let dir = root.join("inbox");
                fs::create_dir_all(&dir).unwrap();
                for file in 0..100 {
                    fs::write(dir.join(format!("new{file:03}.txt")), b"new").unwrap();
                }
            },
        },
        Scenario {
            name: "an hour of desktop use",
            apply: |root| {
                let dir = root.join("branch000/leaf00");
                for file in 0..12 {
                    fs::write(dir.join(format!("f{file:02}.txt")), b"edited in place").unwrap();
                }
                for file in 12..18 {
                    fs::remove_file(dir.join(format!("f{file:02}.txt"))).unwrap();
                }
                for file in 18..24 {
                    fs::rename(
                        dir.join(format!("f{file:02}.txt")),
                        dir.join(format!("renamed{file:02}.txt")),
                    )
                    .unwrap();
                }
                for file in 24..28 {
                    let path = dir.join(format!("f{file:02}.txt"));
                    let mut permissions = fs::metadata(&path).unwrap().permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(true);
                    fs::set_permissions(&path, permissions).unwrap();
                }
                let downloads = root.join("Downloads");
                fs::create_dir_all(&downloads).unwrap();
                for file in 0..25 {
                    fs::write(downloads.join(format!("d{file:02}.bin")), vec![0u8; 4096]).unwrap();
                }
                std::os::unix::fs::symlink("../branch000/leaf00", root.join("shortcut")).unwrap();
                fs::hard_link(
                    root.join("project/src/main.rs"),
                    root.join("project/src/main-alias.rs"),
                )
                .unwrap();
            },
        },
        Scenario {
            name: "a build-shaped burst (5,000 files)",
            apply: |root| {
                let dir = root.join("workspace/build");
                fs::create_dir_all(&dir).unwrap();
                for file in 0..5_000 {
                    fs::write(dir.join(format!("o{file:05}.o")), b"object").unwrap();
                }
                // The same burst inside a policy-excluded directory: the index must ignore it,
                // and the delta must still be cheap.
                let excluded = root.join("project/target/debug");
                for file in 0..2_000 {
                    fs::write(excluded.join(format!("burst{file:05}.o")), b"object").unwrap();
                }
            },
        },
        Scenario {
            name: "a whole-directory rename",
            apply: |root| {
                fs::rename(root.join("branch000"), root.join("branch000-renamed")).unwrap();
            },
        },
        Scenario {
            name: "a mass delete",
            apply: |root| {
                fs::remove_dir_all(root.join("bulk")).unwrap();
            },
        },
        Scenario {
            name: "one large file (1 GiB)",
            apply: |root| {
                let path = root.join("big.bin");
                let chunk = vec![0u8; 1 << 20];
                let mut file = fs::File::create(&path).unwrap();
                for _ in 0..1024 {
                    file.write_all(&chunk).unwrap();
                }
                file.sync_all().unwrap();
            },
        },
        Scenario {
            name: "truncate, chmod and rewrite in place",
            apply: |root| {
                let path = root.join("project/src/main.rs");
                let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.set_len(4).unwrap();
                drop(file);
                fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                    .unwrap();
                fs::write(root.join("project/src/main-alias.rs"), b"rewritten").ok();
            },
        },
    ]
}

// --- one measured step ---------------------------------------------------------------------

struct StepResult {
    scenario: &'static str,
    tree_files: u64,
    stream_bytes: usize,
    plan_paths: usize,
    mutations: usize,
    send: Cost,
    apply: Cost,
    crawl: Cost,
    delta_total: Cost,
    report: DeltaReport,
}

/// Runs one scenario against one tree, in three arms, and checks the delta arm against the fresh
/// one before returning anything.
#[allow(clippy::too_many_lines)]
fn run_step(
    fixture: &Fixture,
    tree: &Path,
    scenario: &Scenario,
    parent: &Path,
    step: usize,
) -> (StepResult, PathBuf) {
    let delta_db = fixture.databases.join("delta.sqlite3");
    let crawl_db = fixture.databases.join("crawl.sqlite3");
    let fresh_db = fixture.databases.join("fresh.sqlite3");

    // Both arms start from exactly the same baseline index.
    copy_database(&delta_db, &crawl_db);

    (scenario.apply)(tree);

    let child = fixture
        .checkpoint(&format!("step{step:02}-b"))
        .expect("the post-change checkpoint");
    let (stream_path, send) = fixture
        .delta(parent, &child, &format!("step{step:02}"))
        .expect("the metadata delta");
    let stream_bytes = fs::metadata(&stream_path).map_or(0, |m| m.len() as usize);

    // Arm 1: the delta.
    let (outcome, apply_cost) = measure(|| {
        with_index(&delta_db, |index| {
            let root = index.roots().expect("roots")[0].root;
            let stream = btrfs_delta::decode_file(&stream_path).expect("decoding the delta");
            assert!(
                stream.is_incremental(),
                "a delta must carry a parent; this stream is a full send"
            );
            let plan = btrfs_delta::plan(&stream).expect("planning the delta");
            // The two roots are different and that is the point: `btrfs send` spells every path
            // from the root of the subvolume it snapshotted (`@home`), while the index covers
            // `@home/bench`. This is the production shape — `@home` mounts at `/home` and the
            // index root is `/home/<user>` — so the harness must not flatten it.
            let target = DeltaTarget {
                root,
                subvolume_path: NativePath::from_path(&fixture.home),
                root_path: NativePath::from_path(tree),
                policy: index.root_policy(root).expect("policy"),
            };
            let (batch, mut report) = btrfs_delta::to_mutations(&plan, &target, &FsMetadata);
            report.stream_bytes = stream.bytes;
            report.commands = stream.commands.len();
            let mutations = batch.mutations.len();
            index.apply(batch).expect("applying the delta");
            (plan.len(), mutations, report)
        })
    });
    let (plan_paths, mutations, report) = outcome;

    // Arm 2: what the index does today — walk the whole tree again.
    let ((), crawl) = measure(|| {
        with_index(&crawl_db, |index| {
            let root = index.roots().expect("roots")[0].root;
            full_reconcile(index, root, tree, &Trigger::Downtime);
        });
    });

    // Arm 3: the ground truth — an index that never saw the old tree at all.
    for suffix in ["", "-wal", "-shm"] {
        let mut path = fresh_db.as_os_str().to_os_string();
        path.push(suffix);
        let _ = fs::remove_file(PathBuf::from(path));
    }
    let tree_files = with_index(&fresh_db, |index| {
        let root = add_root(index, tree);
        full_reconcile(index, root, tree, &Trigger::Requested)
    });

    let (delta_fp, crawl_fp, fresh_fp) = (
        fingerprint(&delta_db),
        fingerprint(&crawl_db),
        fingerprint(&fresh_db),
    );
    assert!(
        describe_difference(&crawl_fp, &fresh_fp).is_none(),
        "the *reference* arm disagrees with a fresh scan after '{}' — the harness itself is \
         wrong, not the delta:\n{}",
        scenario.name,
        describe_difference(&crawl_fp, &fresh_fp).unwrap_or_default()
    );
    assert!(
        describe_difference(&delta_fp, &fresh_fp).is_none(),
        "baseline + delta != fresh scan after '{}':\n{}",
        scenario.name,
        describe_difference(&delta_fp, &fresh_fp).unwrap_or_default()
    );

    let delta_total = Cost {
        wall: send.wall + apply_cost.wall,
        cpu: send.cpu + apply_cost.cpu,
        read_bytes: send.read_bytes + apply_cost.read_bytes,
    };
    (
        StepResult {
            scenario: scenario.name,
            tree_files,
            stream_bytes,
            plan_paths,
            mutations,
            send,
            apply: apply_cost,
            crawl,
            delta_total,
            report,
        },
        child,
    )
}

// --- the tests -------------------------------------------------------------------------------

#[test]
#[ignore = "needs the loopback Btrfs fixture from scripts/btrfs-index-fixture.sh"]
fn a_delta_equals_a_fresh_scan_and_costs_what_the_change_costs() {
    let Some(mount) = fixture() else {
        eprintln!("skipping: LEGE_BTRFS_FIXTURE is not set");
        return;
    };
    let fixture = Fixture::new(&mount);
    let sizes: Vec<usize> = std::env::var("LEGE_BTRFS_TREE_SIZES").ok().map_or_else(
        || vec![1_000, 10_000, 100_000],
        |text| {
            text.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect()
        },
    );

    let mut results: Vec<StepResult> = Vec::new();
    for size in sizes {
        eprintln!("\n=== tree of {size} files ===");
        let tree = fixture.home.join("bench");
        build_tree(&tree, size);

        // The baseline index: what a daemon had before it stopped watching.
        for name in ["delta", "crawl", "fresh"] {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = fixture
                    .databases
                    .join(format!("{name}.sqlite3"))
                    .into_os_string();
                path.push(suffix);
                let _ = fs::remove_file(PathBuf::from(path));
            }
        }
        let delta_db = fixture.databases.join("delta.sqlite3");
        let (baseline_entries, baseline_cost) = measure(|| {
            with_index(&delta_db, |index| {
                let root = add_root(index, &tree);
                full_reconcile(index, root, &tree, &Trigger::Requested)
            })
        });
        eprintln!(
            "initial acquisition: {baseline_entries} entries in {:.1} ms",
            baseline_cost.wall.as_secs_f64() * 1000.0
        );

        let mut parent = fixture
            .checkpoint("step00-a")
            .expect("the baseline checkpoint");
        for (step, scenario) in scenarios().into_iter().enumerate() {
            let (result, child) = run_step(&fixture, &tree, &scenario, &parent, step + 1);
            eprintln!(
                "  {:<38} delta {:>8.1} ms  crawl {:>9.1} ms  x{:.1}",
                scenario.name,
                result.delta_total.wall.as_secs_f64() * 1000.0,
                result.crawl.wall.as_secs_f64() * 1000.0,
                result.crawl.wall.as_secs_f64() / result.delta_total.wall.as_secs_f64().max(1e-9),
            );
            results.push(result);
            let _ = Fixture::discard(&parent);
            parent = child;
        }
        let _ = Fixture::discard(&parent);
    }

    let table = render(&results);
    println!("{table}");
    if let Some(dir) = std::env::var_os("LEGE_BTRFS_REPORT_DIR") {
        write_report(Path::new(&dir), &table);
    }
}

#[test]
#[ignore = "needs the loopback Btrfs fixture from scripts/btrfs-index-fixture.sh"]
fn an_interrupted_delta_is_refused_rather_than_half_applied() {
    let Some(mount) = fixture() else {
        eprintln!("skipping: LEGE_BTRFS_FIXTURE is not set");
        return;
    };
    let fixture = Fixture::new(&mount);
    let tree = fixture.home.join("interrupted");
    build_tree(&tree, 200);
    let parent = fixture.checkpoint("interrupted-a").expect("a checkpoint");
    fs::write(tree.join("after.txt"), b"after").unwrap();
    for file in 0..20 {
        fs::write(tree.join(format!("more{file}.txt")), b"more").unwrap();
    }
    let child = fixture.checkpoint("interrupted-b").expect("a checkpoint");
    let (stream_path, _) = fixture
        .delta(&parent, &child, "interrupted")
        .expect("a delta");

    let whole = fs::read(&stream_path).expect("reading the stream");
    assert!(
        btrfs_delta::decode(&whole).is_ok(),
        "the complete stream must decode"
    );
    // Every prefix of a real stream: none of them may parse into a usable delta.
    let mut refused = 0;
    for cut in (1..whole.len()).step_by((whole.len() / 64).max(1)) {
        assert!(
            btrfs_delta::decode(&whole[..cut]).is_err(),
            "a stream cut at {cut} of {} decoded as if it were complete",
            whole.len()
        );
        refused += 1;
    }
    eprintln!(
        "refused {refused} truncated prefixes of a {}-byte stream",
        whole.len()
    );

    let _ = Fixture::discard(&child);
    let _ = Fixture::discard(&parent);
    let _ = fs::remove_dir_all(&tree);
}

#[test]
#[ignore = "needs the loopback Btrfs fixture from scripts/btrfs-index-fixture.sh"]
fn hard_links_symlinks_and_a_directory_rename_survive_a_round_trip() {
    let Some(mount) = fixture() else {
        eprintln!("skipping: LEGE_BTRFS_FIXTURE is not set");
        return;
    };
    let fixture = Fixture::new(&mount);
    let tree = fixture.home.join("edges");
    build_tree(&tree, 300);

    let delta_db = fixture.databases.join("edges-delta.sqlite3");
    let fresh_db = fixture.databases.join("edges-fresh.sqlite3");
    for database in [&delta_db, &fresh_db] {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = database.clone().into_os_string();
            path.push(suffix);
            let _ = fs::remove_file(PathBuf::from(path));
        }
    }
    with_index(&delta_db, |index| {
        let root = add_root(index, &tree);
        full_reconcile(index, root, &tree, &Trigger::Requested);
    });

    let parent = fixture.checkpoint("edges-a").expect("a checkpoint");

    // Every hard case in one step: a hard link, a symlink, a directory rename that also carries a
    // rename inside it, a replaced path, and a rename cycle.
    fs::hard_link(tree.join("project/src/main.rs"), tree.join("alias.rs")).unwrap();
    std::os::unix::fs::symlink("project/src/main.rs", tree.join("link-to-main")).unwrap();
    fs::rename(
        tree.join("branch000/leaf00"),
        tree.join("branch000/leaf00-moved"),
    )
    .unwrap();
    fs::rename(tree.join("branch000"), tree.join("branch-renamed")).unwrap();
    fs::remove_file(tree.join("project/src/main.rs")).unwrap();
    fs::write(tree.join("project/src/main.rs"), b"replaced").unwrap();
    fs::rename(tree.join("bulk"), tree.join("bulk-tmp")).unwrap();
    fs::rename(tree.join("project"), tree.join("bulk")).unwrap();
    fs::rename(tree.join("bulk-tmp"), tree.join("project")).unwrap();

    let child = fixture.checkpoint("edges-b").expect("a checkpoint");
    let (stream_path, _) = fixture.delta(&parent, &child, "edges").expect("a delta");

    with_index(&delta_db, |index| {
        let root = index.roots().expect("roots")[0].root;
        let stream = btrfs_delta::decode_file(&stream_path).expect("decoding");
        let plan = btrfs_delta::plan(&stream).expect("planning");
        let target = DeltaTarget {
            root,
            // `@home`, not `@home/edges`: the stream names `edges/...`, and anything the other
            // trees in this subvolume contribute belongs to no root of this index.
            subvolume_path: NativePath::from_path(&fixture.home),
            root_path: NativePath::from_path(&tree),
            policy: index.root_policy(root).expect("policy"),
        };
        let (batch, report) = btrfs_delta::to_mutations(&plan, &target, &FsMetadata);
        eprintln!("edge-case delta: {report:?}");
        index.apply(batch).expect("applying");
    });
    with_index(&fresh_db, |index| {
        let root = add_root(index, &tree);
        full_reconcile(index, root, &tree, &Trigger::Requested);
    });

    let difference = describe_difference(&fingerprint(&delta_db), &fingerprint(&fresh_db));
    assert!(
        difference.is_none(),
        "the edge cases diverge:\n{}",
        difference.unwrap_or_default()
    );

    let _ = Fixture::discard(&child);
    let _ = Fixture::discard(&parent);
    let _ = fs::remove_dir_all(&tree);
}

/// `baseline + delta == fresh scan`, on ext4, with the indexed root a strict subdirectory of the
/// subvolume the plan speaks in — the production shape (`@home` mounts at `/home`, the root is
/// `/home/<user>`) and the one the first privileged run got wrong.
///
/// No `btrfs send` and no Btrfs: a [`btrfs_delta::DeltaPlan`] is nothing but subvolume-relative
/// paths, so it can be written down. What is under test is everything downstream of the decoder —
/// the join, the containment test, the policy filter, the `lstat`, and the rows `Index::apply`
/// ends up with.
#[test]
fn a_plan_applies_to_the_right_paths_when_the_root_is_a_subdirectory_of_the_subvolume() {
    let temp = tempfile::tempdir().expect("a temporary directory");
    // `subvolume` stands in for `@home`: it holds this user's root and a sibling's.
    let subvolume = temp.path().join("subvolume");
    let tree = subvolume.join("dk");
    let sibling = subvolume.join("other");
    fs::create_dir_all(&sibling).expect("the sibling home");
    fs::create_dir_all(tree.join("docs")).expect("docs");
    fs::create_dir_all(tree.join("photos")).expect("photos");
    fs::create_dir_all(tree.join("project/target")).expect("an excluded build directory");
    fs::write(tree.join("notes.md"), b"notes").expect("notes");
    fs::write(tree.join("gone.txt"), b"gone").expect("gone");
    fs::write(tree.join("docs/one.txt"), b"one").expect("one");
    fs::write(tree.join("docs/two.txt"), b"two").expect("two");
    fs::write(tree.join("photos/a.jpg"), b"jpg").expect("a.jpg");
    fs::write(tree.join("project/target/x.o"), b"obj").expect("an object file");
    fs::write(sibling.join("theirs.txt"), b"theirs").expect("the sibling's file");

    let delta_db = temp.path().join("delta.sqlite3");
    let fresh_db = temp.path().join("fresh.sqlite3");
    with_index(&delta_db, |index| {
        let root = add_root(index, &tree);
        full_reconcile(index, root, &tree, &Trigger::Requested);
    });

    // What happened while the index was not watching.
    fs::create_dir_all(tree.join("inbox")).expect("inbox");
    fs::write(tree.join("inbox/new.txt"), b"new").expect("a new file");
    fs::remove_file(tree.join("gone.txt")).expect("removing a file");
    fs::rename(tree.join("notes.md"), tree.join("notes-2.md")).expect("renaming a file");
    fs::rename(tree.join("docs"), tree.join("papers")).expect("renaming a directory");
    fs::write(tree.join("photos/a.jpg"), b"rewritten").expect("rewriting in place");
    fs::create_dir_all(tree.join("node_modules")).expect("node_modules");
    fs::write(tree.join("node_modules/x.js"), b"js").expect("a dependency file");
    // The sibling home changed too. A delta of the whole subvolume names it, and this root must
    // not acquire a single row for it.
    fs::write(sibling.join("secret.txt"), b"secret").expect("the sibling's new file");

    // Exactly what `plan()` would produce for that, spelled the way the stream spells it:
    // relative to the subvolume, not to the indexed root.
    let plan = btrfs_delta::DeltaPlan {
        moves: vec![
            (b"dk/notes.md".to_vec(), b"dk/notes-2.md".to_vec()),
            (b"dk/docs".to_vec(), b"dk/papers".to_vec()),
        ],
        removes: vec![b"dk/gone.txt".to_vec()],
        creates: vec![
            b"dk/inbox".to_vec(),
            b"dk/node_modules".to_vec(),
            b"dk/inbox/new.txt".to_vec(),
            b"dk/node_modules/x.js".to_vec(),
            b"other/secret.txt".to_vec(),
        ],
        // The root's own row: adding, removing and renaming its children moved its mtime, ctime
        // and link count, and a scan would see all three.
        touches: vec![b"dk".to_vec(), b"dk/photos/a.jpg".to_vec()],
    };

    let target = DeltaTarget {
        root: with_index(&delta_db, |index| index.roots().expect("roots")[0].root),
        subvolume_path: NativePath::from_path(&subvolume),
        root_path: NativePath::from_path(&tree),
        policy: RootPolicy::default_home(),
    };
    let (batch, report) = btrfs_delta::to_mutations(&plan, &target, &FsMetadata);
    assert_eq!(
        report.vanished, 0,
        "every path was looked for where it really is: {report:?}"
    );
    assert_eq!(
        report.outside_root, 1,
        "the sibling's file is not this root's business: {report:?}"
    );
    assert_eq!(
        report.excluded, 1,
        "the file inside node_modules gets no row, the directory itself does: {report:?}"
    );
    for mutation in &batch.mutations {
        let path = match mutation {
            lege_file_index::Mutation::Upsert { path, .. }
            | lege_file_index::Mutation::Touch { path, .. }
            | lege_file_index::Mutation::Remove { path, .. } => path.clone(),
            lege_file_index::Mutation::Move { to, .. } => to.clone(),
            other @ lege_file_index::Mutation::Invalidate { .. } => {
                panic!("unexpected mutation {other:?}")
            }
        };
        assert!(
            path.starts_with(&NativePath::from_path(&tree)),
            "{} is not under the indexed root",
            path.display()
        );
    }
    with_index(&delta_db, |index| {
        index.apply(batch).expect("applying the delta");
    });

    with_index(&fresh_db, |index| {
        let root = add_root(index, &tree);
        full_reconcile(index, root, &tree, &Trigger::Requested);
    });
    let (delta_fp, fresh_fp) = (fingerprint(&delta_db), fingerprint(&fresh_db));
    assert!(
        !delta_fp.is_empty(),
        "the delta index is empty, which no comparison would catch"
    );
    let difference = describe_difference(&delta_fp, &fresh_fp);
    assert!(
        difference.is_none(),
        "baseline + delta != fresh scan:\n{}",
        difference.unwrap_or_default()
    );
}

// --- reporting ---------------------------------------------------------------------------------

fn render(results: &[StepResult]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n## Delta catch-up versus recursive reconciliation\n");
    let _ = writeln!(
        out,
        "| tree files | scenario | plan paths | mutations | stream | send ms | apply ms | \
         delta ms | crawl ms | speedup |"
    );
    let _ = writeln!(out, "|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for result in results {
        let speedup =
            result.crawl.wall.as_secs_f64() / result.delta_total.wall.as_secs_f64().max(1e-9);
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1}x |",
            result.tree_files,
            result.scenario,
            result.plan_paths,
            result.mutations,
            human_bytes(result.stream_bytes as u64),
            result.send.wall.as_secs_f64() * 1000.0,
            result.apply.wall.as_secs_f64() * 1000.0,
            result.delta_total.wall.as_secs_f64() * 1000.0,
            result.crawl.wall.as_secs_f64() * 1000.0,
            speedup,
        );
    }
    let _ = writeln!(out, "\n### CPU and read I/O\n");
    let _ = writeln!(
        out,
        "| tree files | scenario | delta wall ms | delta cpu ms | delta read | crawl wall ms | \
         crawl cpu ms | crawl read |"
    );
    let _ = writeln!(out, "|---:|---|---:|---:|---:|---:|---:|---:|");
    for result in results {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            result.tree_files,
            result.scenario,
            result.delta_total.row(),
            result.crawl.row(),
        );
    }
    let _ = writeln!(out, "\n### What the decoder saw\n");
    for result in results {
        let _ = writeln!(
            out,
            "- {} files, {}: {:?}",
            result.tree_files, result.scenario, result.report
        );
    }
    out
}

fn write_report(dir: &Path, table: &str) {
    let _ = fs::create_dir_all(dir);
    let path = dir.join("btrfs-delta-measured.md");
    let mut body = String::from(
        "# Btrfs metadata delta versus recursive reconciliation — raw harness output\n\n\
         Generated by `cargo test -p lege-file-index --test btrfs_delta_fixture -- --ignored`\n\
         against the loopback fixture from `scripts/btrfs-index-fixture.sh`.\n",
    );
    body.push_str(table);
    if fs::write(&path, body).is_ok() {
        eprintln!("wrote {}", path.display());
        if let Some(owner) = std::env::var_os("LEGE_BTRFS_REPORT_OWNER") {
            // The harness may be running as root through the fixture script; the report is the
            // developer's file, not root's.
            let _ = Command::new("chown").arg(owner).arg(&path).status();
        }
    }
}

/// What the delta has to beat, measured without any Btrfs at all.
///
/// The claim under test in this lane is not "a delta is fast" but "a delta's cost tracks the
/// change while a crawl's tracks the tree". The second half of that is measurable on any
/// filesystem, right now: hold the change fixed, grow the tree, and watch what
/// [`reconcile`] does. Running it here means the fixture only has to supply the numerator.
///
/// Gated on `LEGE_RECONCILE_BASELINE=1` and `#[ignore]`d, because it writes a six-figure number of
/// files. Fixture directories only — `tempfile`, never a real root.
#[test]
#[ignore = "writes ~150k files; set LEGE_RECONCILE_BASELINE=1 to run"]
fn recursive_reconciliation_costs_the_tree_and_not_the_change() {
    if std::env::var_os("LEGE_RECONCILE_BASELINE").is_none() {
        eprintln!("skipping: set LEGE_RECONCILE_BASELINE=1");
        return;
    }
    let sizes: Vec<usize> = std::env::var("LEGE_BTRFS_TREE_SIZES").ok().map_or_else(
        || vec![1_000, 10_000, 100_000],
        |text| {
            text.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect()
        },
    );

    println!(
        "\n| tree files | indexed entries | acquisition ms | no-change reconcile ms | \
              100-file-change reconcile ms | reconcile cpu ms | reconcile read |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|");
    for size in sizes {
        let temp = tempfile::tempdir().expect("a fixture directory");
        let tree = temp.path().join("tree");
        build_tree(&tree, size);
        let database = temp.path().join("index.sqlite3");

        let (entries, acquire) = measure(|| {
            with_index(&database, |index| {
                let root = add_root(index, &tree);
                full_reconcile(index, root, &tree, &Trigger::Requested)
            })
        });
        // Nothing changed: this is the floor a delta of zero paths is competing with.
        let ((), idle) = measure(|| {
            with_index(&database, |index| {
                let root = index.roots().expect("roots")[0].root;
                full_reconcile(index, root, &tree, &Trigger::Downtime);
            });
        });
        let inbox = tree.join("inbox");
        fs::create_dir_all(&inbox).expect("creating inbox");
        for file in 0..100 {
            fs::write(inbox.join(format!("new{file:03}.txt")), b"new").expect("a new file");
        }
        let ((), changed) = measure(|| {
            with_index(&database, |index| {
                let root = index.roots().expect("roots")[0].root;
                full_reconcile(index, root, &tree, &Trigger::Downtime);
            });
        });

        println!(
            "| {size} | {entries} | {:.0} | {:.0} | {:.0} | {:.0} | {} |",
            acquire.wall.as_secs_f64() * 1000.0,
            idle.wall.as_secs_f64() * 1000.0,
            changed.wall.as_secs_f64() * 1000.0,
            changed.cpu.as_secs_f64() * 1000.0,
            human_bytes(changed.read_bytes),
        );
    }
}

/// The half of delta catch-up that needs no Btrfs: applying the mutations.
///
/// A delta's total cost is `btrfs send` plus `Index::apply`. The first needs a Btrfs filesystem;
/// the second does not, and if *it* turns out to be proportional to the tree then the idea is dead
/// before the fixture is ever built — no send is fast enough to rescue a catch-up whose second
/// stage already walks the index. Worth checking on its own, because `apply` ends every batch with
/// `UPDATE roots SET entry_count = (SELECT COUNT(*) ...)` and an `entries`-wide `refresh_fts`
/// subquery, both of which look tree-shaped.
///
/// Gated on `LEGE_RECONCILE_BASELINE=1`, fixture directories only.
#[test]
#[ignore = "writes ~150k files; set LEGE_RECONCILE_BASELINE=1 to run"]
fn applying_a_batch_costs_the_batch_and_not_the_tree() {
    if std::env::var_os("LEGE_RECONCILE_BASELINE").is_none() {
        eprintln!("skipping: set LEGE_RECONCILE_BASELINE=1");
        return;
    }
    let sizes: Vec<usize> = std::env::var("LEGE_BTRFS_TREE_SIZES").ok().map_or_else(
        || vec![1_000, 10_000, 100_000],
        |text| {
            text.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect()
        },
    );

    println!("\n| tree entries | batch mutations | apply ms | apply cpu ms |");
    println!("|---:|---:|---:|---:|");
    // The trailing repeat of `1` is deliberate: it says whether the first small batch was paying
    // for a cold page cache or for the tree itself.
    for size in sizes {
        let temp = tempfile::tempdir().expect("a fixture directory");
        let tree = temp.path().join("tree");
        build_tree(&tree, size);
        let database = temp.path().join("index.sqlite3");
        let entries = with_index(&database, |index| {
            let root = add_root(index, &tree);
            full_reconcile(index, root, &tree, &Trigger::Requested)
        });

        // One open, many applies. A resuming daemon opens the index once; folding that open (and
        // the WAL checkpoint its close performs) into every batch would measure the harness.
        let index = Index::open(&database, IndexConfig::default()).expect("opening the index");
        let root = index.roots().expect("roots")[0].root;
        for (round, batch_size) in [1usize, 100, 1_000, 1usize].into_iter().enumerate() {
            let inbox = tree.join(format!("inbox{round}"));
            fs::create_dir_all(&inbox).expect("creating inbox");
            let mut mutations = Vec::new();
            for file in 0..batch_size {
                let path = inbox.join(format!("new{file:05}.txt"));
                fs::write(&path, b"new").expect("a new file");
                let metadata = fs::symlink_metadata(&path).expect("stat of a file just written");
                mutations.push(lege_file_index::Mutation::Upsert {
                    root,
                    path: NativePath::from_path(&path),
                    meta: lege_file_index::EntryMeta::from_symlink_metadata(&path, &metadata),
                });
            }
            let batch = lege_file_index::MutationBatch::new(
                lege_file_index::MutationSource::Reconcile,
                mutations,
            );
            let ((), cost) = measure(|| {
                index.apply(batch).expect("applying");
            });
            println!(
                "| {entries} | {batch_size} | {:.1} | {:.1} |",
                cost.wall.as_secs_f64() * 1000.0,
                cost.cpu.as_secs_f64() * 1000.0,
            );
        }
        drop(index);
    }
}
