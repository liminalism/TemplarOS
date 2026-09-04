//! `cargo xtask index-bench` — measure the shared file index's runtime cost before anyone
//! optimises it (`docs/measurements/`, Lane A of the indexing campaign).
//!
//! This cannot be a `benches/` target: `lege_file_index::watch::guard` panics on any watch root
//! outside the temp directory whenever the running executable's path has a `deps` component, and
//! every `cargo bench`/integration-test binary lives in `target/<profile>/deps/`. An `xtask`
//! subcommand's binary does not, so it is the one place a real directory tree can be measured.
//!
//! Every subcommand prints a human table row and, with `--json`, a machine-readable record. There
//! is no hidden default: an unmeasured workload is absent from the report, never estimated.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use lege_file_index::{
    Index, IndexConfig, MutationBatch, MutationSource, NativePath, Query, RootPolicy, ScanOptions,
    coalesce::{Clock, SystemClock},
    model::EntryKind,
    snapshot::SnapshotRequest,
    watch::{InotifySource, WatchBudget, WatchLimits, WatchOwner},
};
use serde::Serialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, Args)]
pub struct IndexBenchArgs {
    #[command(subcommand)]
    pub command: IndexBenchCommand,
}

#[derive(Debug, Subcommand)]
pub enum IndexBenchCommand {
    /// Static facts: the post-skip directory count under a root, computed from the index's own
    /// `RootPolicy` (never from `find`), plus kernel watch limits, `WatchBudget`, and the
    /// required-to-available watch ratio.
    Dirs {
        /// Root to count. Defaults to `$HOME`. Read-only: nothing under this root is written to.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// The timed-workload suite against a generated fixture tree: initial index, downtime
    /// reconcile, watched touch/rename/delete, a build-shaped create burst, a clean-shaped mass
    /// delete, warm search p50/p95, snapshot bytes, and DB size on disk.
    Workloads {
        /// Entries in the generated fixture tree (files + directories).
        #[arg(long, default_value_t = 100_000)]
        entries: u64,
        /// Entries touched/renamed/deleted in each mutation workload.
        #[arg(long, default_value_t = 10_000)]
        mutate: u64,
        /// Repeated warm queries sampled for the search p50/p95.
        #[arg(long, default_value_t = 200)]
        search_queries: u64,
        /// Also generate a 2,000,000-entry fixture and time initial indexing only. Slow (minutes)
        /// and refuses to run against a debug build.
        #[arg(long)]
        allow_two_million: bool,
        /// Write the JSON record here as well as printing it.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Idle-cost sampling of a real `lege-filed`, isolated on its own D-Bus session via
    /// `dbus-run-session` so it never touches the user's live session bus. Reports
    /// `/proc/<pid>/schedstat` timeslices for the `lege-index-watcher` thread, voluntary context
    /// switches, and background read I/O, over a settled window.
    Idle {
        /// Settled sampling window, in seconds.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Entries in the fixture the daemon indexes while idle-sampled.
        #[arg(long, default_value_t = 5_000)]
        entries: u64,
        /// Use the release binary (`target/release/lege-filed`) instead of debug.
        #[arg(long)]
        release: bool,
        /// Sample this `lege-filed` binary instead of the one under this workspace's own
        /// `target/`. Lets a before/after pair be built from two different commits (e.g. two
        /// `git worktree` checkouts) rather than only two points in this tree's own history.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// A label recorded in the JSON output (e.g. "before", "after").
        #[arg(long)]
        label: Option<String>,
        /// Write the JSON record here as well as printing it.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the delta between two `idle` JSON records — the before/after comparison the
    /// campaign's headline number depends on.
    IdleCompare { before: PathBuf, after: PathBuf },
}

pub fn run(_root: &Path, args: &IndexBenchArgs) -> Result<()> {
    match &args.command {
        IndexBenchCommand::Dirs { root } => dirs(root.clone()),
        IndexBenchCommand::Workloads {
            entries,
            mutate,
            search_queries,
            allow_two_million,
            out,
        } => workloads(
            *entries,
            *mutate,
            *search_queries,
            *allow_two_million,
            out.as_deref(),
        ),
        IndexBenchCommand::Idle {
            seconds,
            entries,
            release,
            binary,
            label,
            out,
        } => idle(
            *seconds,
            *entries,
            *release,
            binary.clone(),
            label.clone(),
            out.as_deref(),
        ),
        IndexBenchCommand::IdleCompare { before, after } => idle_compare(before, after),
    }
}

// -------------------------------------------------------------------------------------------
// Static facts: directory count, watch limits.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DirFacts {
    root: PathBuf,
    directories_raw: u64,
    directories_post_skip: u64,
    max_user_watches: Option<u64>,
    max_queued_events: Option<u64>,
    max_user_instances: Option<u64>,
    fanotify_max_user_marks: Option<u64>,
    watch_budget: u64,
    required_to_budget_ratio: Option<f64>,
    required_to_kernel_ratio: Option<f64>,
}

fn dirs(root: Option<PathBuf>) -> Result<()> {
    let root = root
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .context("no --root given and $HOME is unset")?;
    let policy = RootPolicy::default_home();
    let raw_policy = RootPolicy {
        skip_names: Vec::new(),
        skip_trash: false,
        ..policy.clone()
    };
    let directories_raw = count_directories(&root, &raw_policy);
    let directories_post_skip = count_directories(&root, &policy);

    let limits = WatchLimits::read();
    let budget = WatchBudget::from_limits(limits);
    let fanotify_marks = read_proc_sys_u64("/proc/sys/fs/fanotify/max_user_marks");

    let facts = DirFacts {
        root,
        directories_raw,
        directories_post_skip,
        max_user_watches: limits.max_user_watches,
        max_queued_events: limits.max_queued_events,
        max_user_instances: limits.max_user_instances,
        fanotify_max_user_marks: fanotify_marks,
        watch_budget: budget.max_watches,
        required_to_budget_ratio: ratio(directories_post_skip, budget.max_watches),
        required_to_kernel_ratio: ratio(
            directories_post_skip,
            limits.max_user_watches.unwrap_or(0),
        ),
    };

    let ratios = format!(
        "{}  {}",
        opt_ratio(facts.required_to_budget_ratio),
        opt_ratio(facts.required_to_kernel_ratio)
    );
    println!(
        "{:<28} {}\n{:<28} {}\n{:<28} {}\n{:<28} {}\n{:<28} {}\n{:<28} {}\n{:<28} {}\n{:<28} {}\n{:<28} {}",
        "root:",
        facts.root.display(),
        "directories (raw):",
        facts.directories_raw,
        "directories (post-skip):",
        facts.directories_post_skip,
        "max_user_watches:",
        opt(facts.max_user_watches),
        "max_queued_events:",
        opt(facts.max_queued_events),
        "max_user_instances:",
        opt(facts.max_user_instances),
        "fanotify max_user_marks:",
        opt(facts.fanotify_max_user_marks),
        "WatchBudget::max_watches:",
        facts.watch_budget,
        "required/budget, required/kernel:",
        ratios,
    );
    println!("{}", serde_json::to_string_pretty(&facts)?);
    Ok(())
}

fn opt(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn opt_ratio(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.2}x"))
}

fn ratio(required: u64, available: u64) -> Option<f64> {
    if available == 0 {
        None
    } else {
        Some(required as f64 / available as f64)
    }
}

fn read_proc_sys_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The post-skip directory count under `root`, computed exactly the way
/// [`lege_file_index::scan::walk_into`] and [`lege_file_index::watch::WatchOwner::arm_tree`] walk
/// it: the root itself always counts, a child is skipped by `policy.is_skipped` before it is ever
/// looked at (so a skipped subtree is never descended into), and `cross_mount` stops the walk at a
/// mount boundary below the root. This is deliberately the same predicate the index runs, not a
/// `find`-equivalent reimplementation — that is what settles the 157,283-vs-189,089 discrepancy.
fn count_directories(root: &Path, policy: &RootPolicy) -> u64 {
    let root_device = fs::symlink_metadata(root).ok().map(|meta| device_of(&meta));
    let mut count = 0u64;
    walk_count(root, policy, 0, root_device, &mut count);
    count
}

fn walk_count(
    path: &Path,
    policy: &RootPolicy,
    depth: u32,
    root_device: Option<u64>,
    count: &mut u64,
) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    *count += 1;
    if depth > 0
        && !policy.cross_mount
        && let Some(root_device) = root_device
        && root_device != device_of(&metadata)
    {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        let native = NativePath::from_path(&child);
        if policy.is_skipped(&native, EntryKind::Other) {
            continue;
        }
        // Only a real (non-symlink) directory is worth descending into — a symlinked directory
        // is never armed as its own watch (the scanner treats it as `EntryKind::Symlink`, not
        // `Directory`), so recursing into it would overcount.
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            walk_count(&child, policy, depth + 1, root_device, count);
        }
    }
}

fn device_of(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

// -------------------------------------------------------------------------------------------
// Fixture generation.
// -------------------------------------------------------------------------------------------

/// Builds a tree of roughly `target_entries` files and directories (branching factor keeps depth
/// sane for hundreds of thousands of entries), returns the actual entry count and the paths of a
/// sample of leaf files usable for search/mutation workloads.
const FIXTURE_FILES_PER_DIR: u64 = 32;
const FIXTURE_DIRS_PER_DIR: u64 = 8;

fn build_fixture(root: &Path, target_entries: u64) -> Result<FixtureTree> {
    fs::create_dir_all(root)?;
    let mut created = 0u64;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    let mut cursor = 0usize;
    while created < target_entries {
        if cursor >= queue.len() {
            // Ran out of directories before hitting the target (a very small target): add one
            // more under the last directory created.
            let parent = queue.last().cloned().unwrap_or_else(|| root.to_path_buf());
            let dir = parent.join(format!("d{created}"));
            fs::create_dir_all(&dir)?;
            queue.push(dir);
            created += 1;
            continue;
        }
        let dir = queue[cursor].clone();
        cursor += 1;
        for file_index in 0..FIXTURE_FILES_PER_DIR {
            if created >= target_entries {
                break;
            }
            let file = dir.join(format!("f{file_index}.txt"));
            fs::write(&file, b"legebench fixture content\n")?;
            created += 1;
            if files.len() < 2_000 {
                files.push(file);
            }
        }
        for dir_index in 0..FIXTURE_DIRS_PER_DIR {
            if created >= target_entries {
                break;
            }
            let child = dir.join(format!("d{dir_index}"));
            fs::create_dir_all(&child)?;
            queue.push(child);
            created += 1;
        }
    }
    Ok(FixtureTree {
        entries: created,
        sample_files: files,
    })
}

struct FixtureTree {
    entries: u64,
    sample_files: Vec<PathBuf>,
}

// -------------------------------------------------------------------------------------------
// Timed workloads.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct TimedResult {
    entries: u64,
    elapsed_ms: f64,
    entries_per_sec: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl TimedResult {
    fn new(entries: u64, elapsed: Duration, detail: Option<String>) -> Self {
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let entries_per_sec = if elapsed.as_secs_f64() > 0.0 {
            entries as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        Self {
            entries,
            elapsed_ms,
            entries_per_sec,
            detail,
        }
    }
}

#[derive(Debug, Serialize)]
struct SearchLatency {
    samples: u64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Serialize)]
struct SnapshotResult {
    payload_bytes: u64,
    entry_count: u64,
    elapsed_ms: f64,
}

#[derive(Debug, Default, Serialize)]
struct WorkloadReport {
    fixture_entries: u64,
    initial_index: Option<TimedResult>,
    initial_index_2m: Option<TimedResult>,
    downtime_reconcile: Option<TimedResult>,
    watch_arm: Option<TimedResult>,
    watch_arm_refused: Option<u64>,
    touch: Option<TimedResult>,
    rename: Option<TimedResult>,
    delete: Option<TimedResult>,
    build_burst: Option<TimedResult>,
    clean_burst: Option<TimedResult>,
    search: Option<SearchLatency>,
    snapshot: Option<SnapshotResult>,
    db_bytes: Option<u64>,
}

#[allow(clippy::too_many_lines)]
fn workloads(
    entries: u64,
    mutate: u64,
    search_queries: u64,
    allow_two_million: bool,
    out: Option<&Path>,
) -> Result<()> {
    let scratch = tempfile::tempdir().context("creating a fixture scratch directory")?;
    let home = scratch.path().join("home");
    let db_path = scratch.path().join("index.sqlite3");

    println!(
        "generating a {entries}-entry fixture tree at {}",
        home.display()
    );
    let build_start = Instant::now();
    let fixture = build_fixture(&home, entries)?;
    println!(
        "fixture ready: {} entries in {:.2}s",
        fixture.entries,
        build_start.elapsed().as_secs_f64()
    );

    let mut report = WorkloadReport {
        fixture_entries: fixture.entries,
        ..WorkloadReport::default()
    };

    let index = Index::open(&db_path, IndexConfig::default()).context("opening the index")?;
    let policy = RootPolicy::default_home();
    let native_home = NativePath::from_path(&home);
    let root_id = index.add_root(&native_home, policy.clone())?.root;

    // 1. Initial index.
    let start = Instant::now();
    let outcome = index.scan_root(root_id, ScanOptions::default())?;
    let elapsed = start.elapsed();
    println!(
        "initial index: {} entries in {:.2}s ({:.0}/s), complete={}",
        outcome.entries,
        elapsed.as_secs_f64(),
        outcome.entries as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
        outcome.complete
    );
    report.initial_index = Some(TimedResult::new(
        outcome.entries,
        elapsed,
        Some(format!("complete={}", outcome.complete)),
    ));
    report.db_bytes = Some(db_bytes(&db_path));

    // 2. Downtime reconcile: mutate the tree with nothing watching, then reconcile.
    let downtime_ops = mutate.min(fixture.sample_files.len() as u64);
    if downtime_ops > 0 {
        for file in fixture.sample_files.iter().take(downtime_ops as usize) {
            let _ = fs::remove_file(file);
        }
        for index_no in 0..downtime_ops {
            let new_file = home.join(format!("downtime-{index_no}.txt"));
            fs::write(&new_file, b"legebench downtime content\n")?;
        }
        let start = Instant::now();
        let outcome = index.reconcile_root(root_id, ScanOptions::reconcile(policy.clone()))?;
        let elapsed = start.elapsed();
        println!(
            "downtime reconcile ({downtime_ops} removed + {downtime_ops} created, no watcher): \
             {} entries revisited in {:.2}s",
            outcome.entries,
            elapsed.as_secs_f64()
        );
        report.downtime_reconcile = Some(TimedResult::new(
            2 * downtime_ops,
            elapsed,
            Some(format!(
                "walked {} entries, complete={}",
                outcome.entries, outcome.complete
            )),
        ));
    }

    // 3. Arm a live watcher, then measure watched touch/rename/delete/build/clean latency.
    let mut owner = match InotifySource::open() {
        Ok(source) => WatchOwner::new(source, WatchBudget::default()),
        Err(error) => {
            println!("could not open inotify for the watched workloads: {error} (skipping)");
            write_report(&report, out)?;
            return Ok(());
        }
    };
    owner.track_root(root_id, &home, policy.clone());
    let arm_start = Instant::now();
    let mut arm_ok = true;
    if let Err(error) = owner.arm_tree(root_id, &home, &mut |_entry| Ok(())) {
        println!("arming watches failed: {error}");
        arm_ok = false;
    }
    let arm_elapsed = arm_start.elapsed();
    println!(
        "watch arm: {} directories armed, {} refused, in {:.2}s",
        owner.watches_armed(),
        owner.status(0).watches_refused,
        arm_elapsed.as_secs_f64()
    );
    report.watch_arm = Some(TimedResult::new(owner.watches_armed(), arm_elapsed, None));
    report.watch_arm_refused = Some(owner.status(0).watches_refused);

    if arm_ok {
        let clock = SystemClock::new();

        // Reindex what the downtime mutation above already changed, so touch/rename/delete start
        // from a settled, fully-indexed tree.
        index.reconcile_root(root_id, ScanOptions::reconcile(policy.clone()))?;
        drain_watcher(&mut owner, &index, &clock, Duration::from_millis(100), 6);

        let live_files: Vec<PathBuf> = walk_existing_files(&home, mutate.min(200_000) as usize);
        let ops = mutate.min(live_files.len() as u64) as usize;

        if ops > 0 {
            let start = Instant::now();
            touch_files(&live_files[..ops])?;
            let applied = drain_watcher(&mut owner, &index, &clock, Duration::from_millis(100), 6);
            let elapsed = start.elapsed();
            println!(
                "touch {ops}: {applied} mutation(s) applied in {:.2}s",
                elapsed.as_secs_f64()
            );
            report.touch = Some(TimedResult::new(
                ops as u64,
                elapsed,
                Some(format!("{applied} mutations applied")),
            ));

            let start = Instant::now();
            let mut renamed = Vec::with_capacity(ops);
            for file in &live_files[..ops] {
                let target = file.with_extension("renamed");
                if fs::rename(file, &target).is_ok() {
                    renamed.push(target);
                }
            }
            let applied = drain_watcher(&mut owner, &index, &clock, Duration::from_millis(100), 6);
            let elapsed = start.elapsed();
            println!(
                "rename {}: {applied} mutation(s) applied in {:.2}s",
                renamed.len(),
                elapsed.as_secs_f64()
            );
            report.rename = Some(TimedResult::new(
                renamed.len() as u64,
                elapsed,
                Some(format!("{applied} mutations applied")),
            ));

            let start = Instant::now();
            let mut deleted = 0u64;
            for file in &renamed {
                if fs::remove_file(file).is_ok() {
                    deleted += 1;
                }
            }
            let applied = drain_watcher(&mut owner, &index, &clock, Duration::from_millis(100), 6);
            let elapsed = start.elapsed();
            println!(
                "delete {deleted}: {applied} mutation(s) applied in {:.2}s",
                elapsed.as_secs_f64()
            );
            report.delete = Some(TimedResult::new(
                deleted,
                elapsed,
                Some(format!("{applied} mutations applied")),
            ));
        }

        // 4. A `cargo build`-shaped burst: many small files created fast in one directory.
        let burst_dir = home.join("build-burst");
        fs::create_dir_all(&burst_dir)?;
        owner.arm_directory(root_id, &burst_dir);
        let burst_count = mutate.min(20_000);
        let start = Instant::now();
        for index_no in 0..burst_count {
            fs::write(
                burst_dir.join(format!("obj{index_no}.o")),
                b"legebench object\n",
            )?;
        }
        let applied = drain_watcher(&mut owner, &index, &clock, Duration::from_millis(100), 6);
        let elapsed = start.elapsed();
        println!(
            "build-shaped burst ({burst_count} creates): {applied} mutation(s) applied in {:.2}s",
            elapsed.as_secs_f64()
        );
        report.build_burst = Some(TimedResult::new(
            burst_count,
            elapsed,
            Some(format!("{applied} mutations applied")),
        ));

        // 5. A `cargo clean`-shaped mass delete: the burst directory, removed whole.
        let start = Instant::now();
        fs::remove_dir_all(&burst_dir)?;
        let applied = drain_watcher(&mut owner, &index, &clock, Duration::from_millis(100), 6);
        let elapsed = start.elapsed();
        println!(
            "clean-shaped mass delete ({burst_count} entries): {applied} mutation(s) applied in {:.2}s",
            elapsed.as_secs_f64()
        );
        report.clean_burst = Some(TimedResult::new(
            burst_count,
            elapsed,
            Some(format!("{applied} mutations applied")),
        ));
    }

    // 6. Warm search p50/p95 for a 100-result query.
    let needle_dir = home.join("searchable");
    fs::create_dir_all(&needle_dir)?;
    for index_no in 0..150 {
        fs::write(
            needle_dir.join(format!("legebenchneedle{index_no}.txt")),
            b"x",
        )?;
    }
    index.reconcile_root(root_id, ScanOptions::reconcile(policy.clone()))?;
    let query = Query::new("legebenchneedle").with_page_size(100);
    let mut samples = Vec::with_capacity(search_queries as usize);
    for _ in 0..search_queries {
        let start = Instant::now();
        let result = index.search(&query)?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(result.hits.len());
    }
    if !samples.is_empty() {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = percentile(&samples, 0.50);
        let p95 = percentile(&samples, 0.95);
        let max = *samples.last().unwrap();
        println!(
            "search p50={p50:.3}ms p95={p95:.3}ms max={max:.3}ms over {} samples",
            samples.len()
        );
        report.search = Some(SearchLatency {
            samples: samples.len() as u64,
            p50_ms: p50,
            p95_ms: p95,
            max_ms: max,
        });
    }

    // 7. Snapshot bytes and duration.
    let start = Instant::now();
    let mut sink = Vec::new();
    let manifest = index.write_snapshot(&mut sink, &SnapshotRequest::default())?;
    let elapsed = start.elapsed();
    println!(
        "snapshot: {} bytes, {} entries, {:.2}ms",
        sink.len(),
        manifest.entry_count,
        elapsed.as_secs_f64() * 1000.0
    );
    report.snapshot = Some(SnapshotResult {
        payload_bytes: sink.len() as u64,
        entry_count: manifest.entry_count,
        elapsed_ms: elapsed.as_secs_f64() * 1000.0,
    });
    report.db_bytes = Some(db_bytes(&db_path));
    println!("db size on disk: {} bytes", report.db_bytes.unwrap_or(0));

    // 8. Optional 2,000,000-entry initial-index case.
    if allow_two_million {
        if cfg!(debug_assertions) {
            bail!(
                "--allow-two-million refuses to run against a debug build; rerun with \
                 `cargo run -q -p xtask --release -- index-bench workloads --allow-two-million`"
            );
        }
        let big_scratch = tempfile::tempdir().context("creating the 2M-entry scratch directory")?;
        let big_home = big_scratch.path().join("home");
        println!("generating a 2,000,000-entry fixture — this takes minutes");
        let build_start = Instant::now();
        let big_fixture = build_fixture(&big_home, 2_000_000)?;
        println!(
            "2M fixture ready: {} entries in {:.1}s",
            big_fixture.entries,
            build_start.elapsed().as_secs_f64()
        );
        let big_index = Index::open(
            &big_scratch.path().join("index.sqlite3"),
            IndexConfig::default(),
        )?;
        let big_root = big_index
            .add_root(
                &NativePath::from_path(&big_home),
                RootPolicy::default_home(),
            )?
            .root;
        let start = Instant::now();
        let outcome = big_index.scan_root(big_root, ScanOptions::default())?;
        let elapsed = start.elapsed();
        println!(
            "2M initial index: {} entries in {:.1}s ({:.0}/s), complete={}",
            outcome.entries,
            elapsed.as_secs_f64(),
            outcome.entries as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
            outcome.complete
        );
        report.initial_index_2m = Some(TimedResult::new(
            outcome.entries,
            elapsed,
            Some(format!("complete={}", outcome.complete)),
        ));
    } else {
        println!(
            "2,000,000-entry case not run (pass --allow-two-million --release to include it; \
             left empty rather than estimated)"
        );
    }

    write_report(&report, out)?;
    Ok(())
}

fn write_report(report: &WorkloadReport, out: Option<&Path>) -> Result<()> {
    let text = serde_json::to_string_pretty(report)?;
    println!("{text}");
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out, &text).with_context(|| format!("writing {}", out.display()))?;
    }
    Ok(())
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn db_bytes(path: &Path) -> u64 {
    let mut total = fs::metadata(path).map_or(0, |meta| meta.len());
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        total += fs::metadata(PathBuf::from(sidecar)).map_or(0, |meta| meta.len());
    }
    total
}

/// Updates each file's mtime, batched into as few external `touch(1)` invocations as a safe
/// argument-list size allows.
///
/// This shells out rather than calling `std::fs::File::set_modified` (an fd-based `futimens`)
/// directly: a minimal reproduction (a fresh `inotify` watch armed with `ATTRIB`, one file
/// touched each way) showed `set_modified` produces **no** inotify event at all in this sandbox,
/// while `touch(1)` (a path-based `utimensat`) produces `ATTRIB` immediately for the same file on
/// the same watch. Whatever intercepts it — likely the sandbox this harness runs under, since the
/// underlying kernel and filesystem are otherwise ordinary — the workload needs a mutation this
/// environment's fsnotify hooks actually see, so it uses the one that reliably does.
fn touch_files(files: &[PathBuf]) -> Result<()> {
    const CHUNK: usize = 1000;
    for chunk in files.chunks(CHUNK) {
        let status = std::process::Command::new("touch")
            .args(chunk)
            .status()
            .context("spawning touch(1)")?;
        if !status.success() {
            bail!("touch(1) exited with {status}");
        }
    }
    Ok(())
}

/// Pump-apply loop: keep draining the watcher and applying whatever mutations arrive until
/// `quiet_polls` consecutive polls return nothing, meaning the kernel's event queue — and the
/// index's own view — has caught up with what the filesystem just did. Returns the total number
/// of mutations applied.
///
/// This measures the watcher-pump-apply latency directly, **without** the production coalescing
/// debounce window (`services/lege-filed/src/watcher.rs`, owned by another lane in this
/// campaign): each pumped batch is applied immediately rather than held for quiet time. That is a
/// different, and generally lower, number than end-to-end production latency; it isolates what
/// this crate's own watch-and-apply path costs.
fn drain_watcher(
    owner: &mut WatchOwner<InotifySource>,
    index: &Index,
    clock: &dyn Clock,
    poll_timeout: Duration,
    quiet_polls: u32,
) -> u64 {
    let mut applied = 0u64;
    let mut quiet = 0u32;
    // A generous ceiling so a stuck watcher cannot hang the harness forever.
    for _ in 0..20_000 {
        let pumped = owner.pump(Some(poll_timeout), clock);

        if pumped.mutations.is_empty() {
            quiet += 1;
            if quiet >= quiet_polls {
                break;
            }
            continue;
        }
        quiet = 0;
        let count = pumped.mutations.len() as u64;
        let batch = MutationBatch::new(MutationSource::Watcher, pumped.mutations);
        if index.apply(batch).is_ok() {
            applied += count;
        }
    }
    applied
}

fn walk_existing_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if found.len() >= limit {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            } else if found.len() < limit {
                found.push(path);
            }
        }
    }
    found
}

// -------------------------------------------------------------------------------------------
// Idle-cost sampling of a real `lege-filed`.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Serialize, serde::Deserialize)]
struct IdleSample {
    label: Option<String>,
    seconds: u64,
    fixture_entries: u64,
    watcher_thread_found: bool,
    schedstat_pcount_delta: Option<u64>,
    voluntary_ctxt_switches_delta: Option<u64>,
    read_bytes_delta: Option<u64>,
    syscw_delta: Option<u64>,
    wchar_delta: Option<u64>,
}

fn idle(
    seconds: u64,
    entries: u64,
    release: bool,
    binary: Option<PathBuf>,
    label: Option<String>,
    out: Option<&Path>,
) -> Result<()> {
    let profile = if release { "release" } else { "debug" };
    let binary = match binary {
        Some(binary) => binary,
        None => find_workspace_root()?
            .join("target")
            .join(profile)
            .join("lege-filed"),
    };
    if !binary.exists() {
        bail!(
            "{} does not exist — build it first: cargo build{} -p lege-filed",
            binary.display(),
            if release { " --release" } else { "" }
        );
    }
    if which("dbus-run-session").is_none() {
        bail!("dbus-run-session is not on PATH — install dbus-user-session to isolate this");
    }

    let scratch = tempfile::tempdir().context("creating an idle-sampling scratch directory")?;
    let home = scratch.path().join("home");
    let index_db = scratch.path().join("cache").join("file-index-v1.sqlite3");
    let state_db = scratch.path().join("state").join("session.db");
    println!("generating a {entries}-entry fixture for idle sampling");
    build_fixture(&home, entries)?;

    let mut child = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(&binary)
        .arg("--index-root")
        .arg(&home)
        .arg("--index-db")
        .arg(&index_db)
        .arg("--state-db")
        .arg(&state_db)
        .arg("--reserve-percent")
        .arg("0")
        .env("HOME", scratch.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning an isolated lege-filed")?;

    // `dbus-run-session -- lege-filed` means the child we spawned is dbus-run-session itself,
    // which execve's into a helper before lege-filed runs; wait for the index db to appear as a
    // simple, robust "it is up" signal instead of parsing process trees.
    let up_deadline = Instant::now() + Duration::from_secs(20);
    while !index_db.exists() && Instant::now() < up_deadline {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Let initial indexing and watch-arming settle before the measurement window starts, so the
    // window captures idle cost, not startup cost.
    std::thread::sleep(Duration::from_secs(5));

    let marker = scratch.path().to_string_lossy().into_owned();
    let pid = find_lege_filed_pid(&marker);
    let sample = match pid {
        None => {
            println!("could not find a running lege-filed process to sample (reporting nothing)");
            IdleSample {
                label,
                seconds,
                fixture_entries: entries,
                watcher_thread_found: false,
                schedstat_pcount_delta: None,
                voluntary_ctxt_switches_delta: None,
                read_bytes_delta: None,
                syscw_delta: None,
                wchar_delta: None,
            }
        }
        Some(pid) => {
            let tid = find_thread(pid, "lege-index-watc"); // comm is truncated to 15 bytes
            let before = ProcSample::read(pid, tid);
            println!("sampling pid {pid} (watcher tid {tid:?}) for {seconds}s...");
            std::thread::sleep(Duration::from_secs(seconds));
            let after = ProcSample::read(pid, tid);
            IdleSample {
                label,
                seconds,
                fixture_entries: entries,
                watcher_thread_found: tid.is_some(),
                schedstat_pcount_delta: delta(before.schedstat_pcount, after.schedstat_pcount),
                voluntary_ctxt_switches_delta: delta(
                    before.voluntary_ctxt_switches,
                    after.voluntary_ctxt_switches,
                ),
                read_bytes_delta: delta(before.read_bytes, after.read_bytes),
                syscw_delta: delta(before.syscw, after.syscw),
                wchar_delta: delta(before.wchar, after.wchar),
            }
        }
    };

    if let Some(pid) = pid {
        kill_pid(pid);
    }
    let _ = child.kill();
    let _ = child.wait();

    println!("{}", serde_json::to_string_pretty(&sample)?);
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out, serde_json::to_string_pretty(&sample)?)
            .with_context(|| format!("writing {}", out.display()))?;
    }
    Ok(())
}

fn delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    Some(after?.saturating_sub(before?))
}

#[derive(Default)]
struct ProcSample {
    schedstat_pcount: Option<u64>,
    voluntary_ctxt_switches: Option<u64>,
    read_bytes: Option<u64>,
    syscw: Option<u64>,
    wchar: Option<u64>,
}

impl ProcSample {
    fn read(pid: u32, watcher_tid: Option<u32>) -> Self {
        let mut sample = Self::default();
        if let Some(tid) = watcher_tid {
            sample.schedstat_pcount = read_to_string(&format!("/proc/{pid}/task/{tid}/schedstat"))
                .and_then(|text| {
                    // `cpu_time_ns run_delay_ns pcount` (verified against a live process: e.g.
                    // "9930109 125917 31" — pcount is the third field, index 2, not the second).
                    text.split_whitespace()
                        .nth(2)
                        .and_then(|field| field.parse().ok())
                });
        }
        // `voluntary_ctxt_switches` in `status` is per-*task*, never summed across a process's
        // threads even when the path is the thread-group leader's own `/proc/<pid>/status` — so
        // reading the leader (`pid`) here would silently read the wrong thread's count whenever
        // the activity being measured (as here) happens on a worker thread instead. Read the
        // watcher thread's own status, the same task `schedstat_pcount` above already resolved.
        let status_path = watcher_tid.map_or_else(
            || format!("/proc/{pid}/status"),
            |tid| format!("/proc/{pid}/task/{tid}/status"),
        );
        if let Some(text) = read_to_string(&status_path) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
                    sample.voluntary_ctxt_switches = rest.trim().parse().ok();
                }
            }
        }
        if let Some(text) = read_to_string(&format!("/proc/{pid}/io")) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("read_bytes:") {
                    sample.read_bytes = rest.trim().parse().ok();
                } else if let Some(rest) = line.strip_prefix("syscw:") {
                    sample.syscw = rest.trim().parse().ok();
                } else if let Some(rest) = line.strip_prefix("wchar:") {
                    sample.wchar = rest.trim().parse().ok();
                }
            }
        }
        sample
    }
}

fn read_to_string(path: &str) -> Option<String> {
    let mut buffer = String::new();
    fs::File::open(path)
        .ok()?
        .read_to_string(&mut buffer)
        .ok()?;
    Some(buffer)
}

fn find_thread(pid: u32, comm_prefix: &str) -> Option<u32> {
    let task_dir = format!("/proc/{pid}/task");
    let entries = fs::read_dir(task_dir).ok()?;
    for entry in entries.flatten() {
        let tid: u32 = entry.file_name().to_string_lossy().parse().ok()?;
        let comm_path = entry.path().join("comm");
        if let Some(comm) = read_to_string(comm_path.to_str()?)
            && comm.trim() == comm_prefix
        {
            return Some(tid);
        }
    }
    None
}

/// The `lege-filed` process whose command line names `marker` (this run's scratch directory) —
/// never just "any `lege-filed` on the machine". A prior run's process can outlive its
/// `dbus-run-session` parent (killing the parent does not reliably kill the grandchild across a
/// D-Bus disconnect), and sampling the wrong one silently reports someone else's idle window.
fn find_lege_filed_pid(marker: &str) -> Option<u32> {
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        let comm = read_to_string(&format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "lege-filed" {
            continue;
        }
        let Some(cmdline) = fs::read(format!("/proc/{pid}/cmdline")).ok() else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&cmdline);
        if cmdline.contains(marker) {
            return Some(pid);
        }
    }
    None
}

/// `SIGTERM`, then `SIGKILL` after a short grace period if it did not exit — best-effort (via the
/// `kill(1)` binary, so this needs no `libc`/`nix` dependency), so a benchmark run never leaves an
/// idle `lege-filed` behind for the next one to accidentally sample.
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
    for _ in 0..20 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn find_workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .context("xtask must live in tools/xtask")
        .map(PathBuf::from)
}

fn idle_compare(before: &Path, after: &Path) -> Result<()> {
    let before: IdleSample = serde_json::from_str(
        &fs::read_to_string(before).with_context(|| format!("reading {}", before.display()))?,
    )?;
    let after: IdleSample = serde_json::from_str(
        &fs::read_to_string(after).with_context(|| format!("reading {}", after.display()))?,
    )?;
    println!(
        "{:<32} {:>14} {:>14}",
        "metric",
        before.label.as_deref().unwrap_or("before"),
        after.label.as_deref().unwrap_or("after")
    );
    let row = |name: &str, before_value: Option<u64>, after_value: Option<u64>| {
        println!(
            "{name:<32} {:>14} {:>14}",
            opt(before_value),
            opt(after_value)
        );
    };
    row(
        "watcher thread schedstat pcount",
        before.schedstat_pcount_delta,
        after.schedstat_pcount_delta,
    );
    row(
        "voluntary_ctxt_switches",
        before.voluntary_ctxt_switches_delta,
        after.voluntary_ctxt_switches_delta,
    );
    row(
        "read_bytes",
        before.read_bytes_delta,
        after.read_bytes_delta,
    );
    row("syscw", before.syscw_delta, after.syscw_delta);
    row("wchar", before.wchar_delta, after.wchar_delta);
    Ok(())
}
