//! `cargo xtask index-bench churn|memory|layout` — the shared index measured while files change
//! and while several clients pull on it, not only warm and idle.
//!
//! Every subcommand runs against an owned private session bus and an owned `templar-filed`, or
//! in process; nothing touches the live bus or the person's index. Reports go to stdout, to an
//! optional JSON path, and as a Markdown table to `docs/measurements/index-<subcommand>-<date>.md`.

use crate::index_bench::{
    OwnedChild, build_fixture, idle_binary, percentile, read_to_string, which,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use templar_client::{
    file_index::{self, IndexRoots, Observation},
    proxies::{FilesBrowseProxy, FilesIndexProxy},
};
use templar_file_index::{
    CallContext, EntryKind, EntryMeta, Index, IndexConfig, Mutation, MutationBatch, MutationSource,
    NativePath, Query, RootPolicy, ScanOptions,
    snapshot::{SnapshotReader, SnapshotRequest},
};

/// Needles that are broad on the generated fixtures: one character, two, a trigram and a
/// selective name.
const CHURN_NEEDLES: &[&str] = &["d", "f1", "txt", "f12.txt"];

// ---- the owned daemon -----------------------------------------------------------------------

struct PrivateDaemon {
    bus: OwnedChild,
    filed: OwnedChild,
    connection: zbus::Connection,
    proxy: FilesIndexProxy<'static>,
    _observation: Observation,
}

impl PrivateDaemon {
    async fn start(
        binary: &Path,
        scratch: &Path,
        roots: &[PathBuf],
        timeout: Duration,
    ) -> Result<(Self, IndexRoots)> {
        use std::{
            os::unix::fs::PermissionsExt as _,
            process::{Command, Stdio},
        };
        for name in ["config", "data", "state", "cache", "runtime", "home"] {
            fs::create_dir_all(scratch.join(name))?;
        }
        let runtime = scratch.join("runtime");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        let mut bus = OwnedChild(
            Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .arg(format!("--address=unix:tmpdir={}", runtime.display()))
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("spawning the owned private bus")?,
        );
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().context("private bus address pipe")?)
            .read_line(&mut address)?;
        if !address.starts_with("unix:") {
            bail!("private bus did not publish a Unix address");
        }
        let address = address.trim().to_owned();
        let mut command = Command::new(binary);
        for root in roots {
            command.arg("--index-root").arg(root);
        }
        let filed = OwnedChild(
            command
                .arg("--index-db")
                .arg(scratch.join("cache/file-index-v1.sqlite3"))
                .arg("--state-db")
                .arg(scratch.join("state/session.db"))
                .args(["--reserve-percent", "0"])
                .env("HOME", scratch.join("home"))
                .env("XDG_CONFIG_HOME", scratch.join("config"))
                .env("XDG_DATA_HOME", scratch.join("data"))
                .env("XDG_STATE_HOME", scratch.join("state"))
                .env("XDG_CACHE_HOME", scratch.join("cache"))
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("DBUS_SESSION_BUS_ADDRESS", &address)
                .env("DBUS_SYSTEM_BUS_ADDRESS", &address)
                .env("TEMPLAR_INDEX_FIXTURE_GUARD", "1")
                .env("RUST_LOG", "warn")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .context("spawning the owned native filed")?,
        );
        let mut daemon_filed = filed;
        let connection = zbus::connection::Builder::address(address.as_str())?
            .build()
            .await?;
        let proxy = FilesIndexProxy::new(&connection).await?;
        let deadline = Instant::now() + timeout;
        let mut observation = None;
        let roots = loop {
            if let Some(status) = daemon_filed.0.try_wait()? {
                bail!("owned filed exited during startup: {status}");
            }
            if Instant::now() >= deadline {
                bail!("owned filed did not bring every root current within {timeout:?}");
            }
            if observation.is_none()
                && let Ok(Ok(acquired)) =
                    tokio::time::timeout(Duration::from_secs(2), Observation::acquire(&connection))
                        .await
            {
                observation = Some(acquired);
            }
            if observation.is_some()
                && let Ok(Ok(state)) =
                    tokio::time::timeout(Duration::from_secs(2), file_index::roots(&proxy)).await
                && state.roots.len() >= roots.len()
                && state.roots.iter().all(|root| root.current())
            {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        Ok((
            Self {
                bus,
                filed: daemon_filed,
                connection,
                proxy,
                _observation: observation.expect("an observation was acquired"),
            },
            roots,
        ))
    }

    fn pid(&self) -> u32 {
        self.filed.0.id()
    }

    fn stop(mut self) -> Result<()> {
        self.filed.stop()?;
        self.bus.stop()?;
        Ok(())
    }
}

fn daemon_binary() -> Result<PathBuf> {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = idle_binary(None, profile)?;
    if !binary.exists() {
        bail!(
            "{} does not exist — build it first: cargo build{} -p templar-filed",
            binary.display(),
            if cfg!(debug_assertions) {
                ""
            } else {
                " --release"
            }
        );
    }
    if which("dbus-daemon").is_none() {
        bail!("dbus-daemon is not on PATH — install dbus-user-session to isolate this");
    }
    Ok(binary.canonicalize()?)
}

fn root_id(roots: &IndexRoots, path: &Path) -> Result<i64> {
    roots
        .roots
        .iter()
        .find(|root| root.native_path().is_ok_and(|native| native == path))
        .map(|root| root.root)
        .with_context(|| format!("root {} is not registered", path.display()))
}

// ---- fixtures -------------------------------------------------------------------------------

/// A tree shaped like a person's files rather than a balanced generator: depth 1 to 24, names up
/// to 255 bytes, 3 % non-UTF-8 names, 0.5 % names containing a newline, one directory with 50,000
/// children and many directories of one to five entries. Deterministic.
pub(crate) fn build_realistic(root: &Path, target_entries: u64) -> Result<u64> {
    use std::os::unix::ffi::OsStringExt as _;
    fs::create_dir_all(root)?;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut created = 0u64;
    let wide = root.join("wide");
    fs::create_dir(&wide)?;
    created += 1;
    let wide_children = target_entries.min(50_000).saturating_sub(1);
    for number in 0..wide_children {
        fs::write(wide.join(format!("f{number}.txt")), b"w")?;
        created += 1;
    }
    let mut directories = vec![root.to_path_buf()];
    let mut depth = vec![0u32];
    while created < target_entries {
        let roll = next();
        let parent_index = (roll as usize) % directories.len();
        let parent = directories[parent_index].clone();
        let parent_depth = depth[parent_index];
        let mut name: Vec<u8> = match next() % 1000 {
            0..=29 => vec![b'n', 0xff, 0xfe, b'x'],
            30..=34 => b"line\nbreak".to_vec(),
            35..=44 => {
                let length = 120 + (next() % 135) as usize;
                vec![b'l'; length]
            }
            _ => format!("f{}", next() % 10_000).into_bytes(),
        };
        name.extend_from_slice(format!("-{created}").as_bytes());
        let path = parent.join(std::ffi::OsString::from_vec(name));
        // Directories are kept small (1-5 entries) by picking new parents often, and capped at
        // depth 24.
        if next() % 4 == 0 && parent_depth < 24 {
            fs::create_dir(&path)?;
            directories.push(path);
            depth.push(parent_depth + 1);
        } else {
            fs::write(&path, b"r")?;
        }
        created += 1;
    }
    Ok(created)
}

// ---- churn ----------------------------------------------------------------------------------

#[derive(Debug, Default, Serialize)]
struct Latency {
    samples: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

impl Latency {
    fn of(mut samples: Vec<f64>) -> Self {
        samples.sort_by(f64::total_cmp);
        Self {
            samples: samples.len(),
            p50_ms: percentile(&samples, 0.50),
            p95_ms: percentile(&samples, 0.95),
            p99_ms: percentile(&samples, 0.99),
            max_ms: samples.last().copied().unwrap_or(0.0),
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct ScopeReport {
    scope: String,
    search: Latency,
    first_fetch: Latency,
    /// How many answers each plan produced: the arrays hit rate is `arrays / total`.
    strategies: BTreeMap<String, u64>,
    arrays_hit_rate: f64,
    queue_ms: Latency,
    errors: BTreeMap<String, u64>,
}

#[derive(Debug, Default, Serialize)]
struct ChurnReport {
    label: Option<String>,
    seconds: u64,
    quiet_entries: u64,
    build_entries: u64,
    shape: String,
    build_bursts: u64,
    build_files_created: u64,
    download_appends: u64,
    edits: u64,
    edit_scope: String,
    scopes: Vec<ScopeReport>,
    daemon_threads_max: u64,
    browse: Latency,
    browse_errors: u64,
    final_roots: Option<IndexRoots>,
    array_stats: Option<templar_file_index::ArrayStats>,
    overlay_peak_bytes: u64,
}

#[allow(clippy::too_many_lines)]
pub fn churn(
    seconds: u64,
    quiet_entries: u64,
    realistic: bool,
    edit_build: bool,
    binary: Option<&Path>,
    label: Option<String>,
    out: Option<&Path>,
) -> Result<()> {
    let binary = binary
        .map(|p| p.canonicalize().map_err(anyhow::Error::from))
        .unwrap_or_else(daemon_binary)?;
    let scratch = tempfile::tempdir().context("creating the churn scratch directory")?;
    let quiet = scratch.path().join("quiet");
    let build = scratch.path().join("build");
    let download = scratch.path().join("download");
    println!("generating the quiet tree ({quiet_entries} entries)");
    let quiet_made = if realistic {
        build_realistic(&quiet, quiet_entries)?
    } else {
        build_fixture(&quiet, quiet_entries)?.entries
    };
    let build_made = build_fixture(&build, 20_000)?.entries;
    fs::create_dir_all(&download)?;
    let edit_sample = walk_files(if edit_build { &build } else { &quiet }, 2_000);
    let runtime = tokio::runtime::Runtime::new()?;
    let mut report = ChurnReport {
        label,
        seconds,
        quiet_entries: quiet_made,
        build_entries: build_made,
        edit_scope: if edit_build { "build" } else { "quiet" }.into(),
        shape: if realistic { "realistic" } else { "synthetic" }.to_owned(),
        ..ChurnReport::default()
    };
    runtime.block_on(async {
        let (daemon, roots) = PrivateDaemon::start(
            &binary,
            scratch.path(),
            &[quiet.clone(), build.clone(), download.clone()],
            Duration::from_secs(600),
        )
        .await?;
        let quiet_id = root_id(&roots, &quiet)?;
        let build_id = root_id(&roots, &build)?;
        let stop = Arc::new(AtomicBool::new(false));
        let browse_driver = {
            let connection = daemon.connection.clone();
            let stop = Arc::clone(&stop);
            let directory = download.clone();
            tokio::spawn(async move {
                let proxy = FilesBrowseProxy::new(&connection).await?;
                let mut samples = Vec::new();
                let mut errors = 0;
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    match tokio::time::timeout(
                        Duration::from_millis(100),
                        proxy.list_directory(&directory.to_string_lossy(), false, 10),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        _ => errors += 1,
                    }
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                anyhow::Ok((samples, errors))
            })
        };
        let bursts = Arc::new(AtomicU64::new(0));
        let burst_files = Arc::new(AtomicU64::new(0));
        let appends = Arc::new(AtomicU64::new(0));
        let edits = Arc::new(AtomicU64::new(0));

        // build-churn: a 5,000-file burst every two seconds, the previous burst removed, and 50
        // directories renamed per burst.
        let build_driver = {
            let (stop, bursts, burst_files, build) = (
                Arc::clone(&stop),
                Arc::clone(&bursts),
                Arc::clone(&burst_files),
                build.clone(),
            );
            std::thread::spawn(move || -> Result<()> {
                let mut number = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let burst = build.join(format!("gen-{number}"));
                    for group in 0..50 {
                        let directory = burst.join(format!("unit-{group}"));
                        fs::create_dir_all(&directory)?;
                        for file in 0..100 {
                            fs::write(directory.join(format!("obj-{file}.o")), b"o")?;
                        }
                    }
                    burst_files.fetch_add(5_000, Ordering::Relaxed);
                    for group in 0..50 {
                        fs::rename(
                            burst.join(format!("unit-{group}")),
                            burst.join(format!("done-{group}")),
                        )?;
                    }
                    if number > 0 {
                        fs::remove_dir_all(build.join(format!("gen-{}", number - 1)))?;
                    }
                    bursts.fetch_add(1, Ordering::Relaxed);
                    number += 1;
                    std::thread::sleep(Duration::from_secs(2).saturating_sub(started.elapsed()));
                }
                Ok(())
            })
        };
        // download: one growing file every 100 ms, with a `.part` sibling created and removed.
        let download_driver = {
            let (stop, appends, download) =
                (Arc::clone(&stop), Arc::clone(&appends), download.clone());
            std::thread::spawn(move || -> Result<()> {
                use std::io::Write as _;
                let mut target = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(download.join("big.iso"))?;
                let mut tick = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    target.write_all(&[0u8; 4096])?;
                    target.flush()?;
                    let part = download.join(format!("chunk-{}.part", tick % 4));
                    if part.exists() {
                        fs::remove_file(&part)?;
                    } else {
                        fs::write(&part, b"p")?;
                    }
                    appends.fetch_add(1, Ordering::Relaxed);
                    tick += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(())
            })
        };
        // Ten files/second in the explicitly recorded edit scope.
        let edit_driver = {
            let (stop, edits) = (Arc::clone(&stop), Arc::clone(&edits));
            let sample = edit_sample;
            std::thread::spawn(move || -> Result<()> {
                let mut index = 0usize;
                while !stop.load(Ordering::Relaxed) && !sample.is_empty() {
                    for _ in 0..10 {
                        let path = &sample[index % sample.len()];
                        fs::write(path, format!("edit {index}\n"))?;
                        index += 1;
                        edits.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                Ok(())
            })
        };

        let scopes: [(&str, Vec<i64>); 3] = [
            ("quiet", vec![quiet_id]),
            ("build", vec![build_id]),
            ("all", Vec::new()),
        ];
        let mut samples: Vec<ScopeSamples> =
            scopes.iter().map(|_| ScopeSamples::default()).collect();
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut round = 0usize;
        let mut threads_max = 0u64;
        while Instant::now() < deadline {
            for (slot, (_, root_ids)) in scopes.iter().enumerate() {
                let needle = CHURN_NEEDLES[round % CHURN_NEEDLES.len()];
                one_query(&daemon.proxy, needle, root_ids, &mut samples[slot]).await;
            }
            threads_max = threads_max.max(thread_count(daemon.pid()));
            if round % 32 == 0 {
                if let Some(stats) = sample_array_stats(&daemon.proxy).await {
                    report.overlay_peak_bytes = report
                        .overlay_peak_bytes
                        .max(stats.roots.iter().map(|root| root.overlay_bytes).sum());
                    report.array_stats = Some(stats);
                }
            }
            round += 1;
        }
        stop.store(true, Ordering::Relaxed);
        for driver in [build_driver, download_driver, edit_driver] {
            driver
                .join()
                .map_err(|_| anyhow::anyhow!("a churn driver panicked"))??;
        }
        report.build_bursts = bursts.load(Ordering::Relaxed);
        report.build_files_created = burst_files.load(Ordering::Relaxed);
        report.download_appends = appends.load(Ordering::Relaxed);
        report.edits = edits.load(Ordering::Relaxed);
        report.daemon_threads_max = threads_max;
        let (browse, errors) = browse_driver.await??;
        report.browse = Latency::of(browse);
        report.browse_errors = errors;
        report.scopes = scopes
            .iter()
            .zip(samples)
            .map(|((name, _), samples)| samples.report(name))
            .collect();
        report.final_roots = file_index::roots(&daemon.proxy).await.ok();
        if let Some(stats) = sample_array_stats(&daemon.proxy).await {
            report.overlay_peak_bytes = report
                .overlay_peak_bytes
                .max(stats.roots.iter().map(|root| root.overlay_bytes).sum());
            report.array_stats = Some(stats);
        }
        daemon.stop()
    })?;
    print_and_write("churn", &report, &churn_markdown(&report), out)
}

#[derive(Default)]
struct ScopeSamples {
    search: Vec<f64>,
    fetch: Vec<f64>,
    queue: Vec<f64>,
    strategies: BTreeMap<String, u64>,
    errors: BTreeMap<String, u64>,
}

impl ScopeSamples {
    fn report(self, name: &str) -> ScopeReport {
        let answered: u64 = self.strategies.values().sum();
        let arrays = self.strategies.get("arrays").copied().unwrap_or(0)
            + self.strategies.get("arrays+overlay").copied().unwrap_or(0);
        ScopeReport {
            scope: name.to_owned(),
            search: Latency::of(self.search),
            first_fetch: Latency::of(self.fetch),
            arrays_hit_rate: if answered == 0 {
                0.0
            } else {
                arrays as f64 / answered as f64
            },
            strategies: self.strategies,
            queue_ms: Latency::of(self.queue),
            errors: self.errors,
        }
    }
}

async fn sample_array_stats(proxy: &FilesIndexProxy<'_>) -> Option<templar_file_index::ArrayStats> {
    let roots: serde_json::Value = serde_json::from_str(&proxy.roots().await.ok()?).ok()?;
    serde_json::from_value(roots["diagnostics"]["arrays"].clone()).ok()
}

async fn one_query(
    proxy: &FilesIndexProxy<'_>,
    needle: &str,
    roots: &[i64],
    samples: &mut ScopeSamples,
) {
    let mut query = Query::new(needle).with_page_size(100);
    query.roots = roots
        .iter()
        .map(|id| templar_file_index::RootId(*id))
        .collect();
    let json = serde_json::to_string(&query).expect("a query serialises");
    let started = Instant::now();
    match proxy.search(&json).await {
        Ok((cursor, _, page)) => {
            samples
                .search
                .push(started.elapsed().as_secs_f64() * 1000.0);
            let value: serde_json::Value = serde_json::from_str(&page).unwrap_or_default();
            let strategy = value["strategy"].as_str().unwrap_or("?").to_owned();
            *samples.strategies.entry(strategy).or_insert(0) += 1;
            if let Some(queue) = value["queue_ms"].as_f64() {
                samples.queue.push(queue);
            }
            if !cursor.is_empty() {
                let fetched = Instant::now();
                if proxy.fetch_page(&cursor, 100, 100).await.is_ok() {
                    samples.fetch.push(fetched.elapsed().as_secs_f64() * 1000.0);
                }
                let _ = proxy.close_cursor(&cursor).await;
            }
        }
        Err(error) => {
            let name = match &error {
                zbus::Error::MethodError(name, _, _) => name.to_string(),
                other => other.to_string(),
            };
            *samples.errors.entry(name).or_insert(0) += 1;
        }
    }
}

fn thread_count(pid: u32) -> u64 {
    fs::read_dir(format!("/proc/{pid}/task")).map_or(0, |tasks| tasks.count() as u64)
}

fn walk_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(directory) = queue.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => queue.push(path),
                Ok(kind) if kind.is_file() && files.len() < limit => files.push(path),
                _ => {}
            }
        }
        if files.len() >= limit {
            break;
        }
    }
    files
}

fn churn_markdown(report: &ChurnReport) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# Index churn — {}\n", today());
    let _ = writeln!(
        text,
        "Owned private bus and `templar-filed`; quiet tree {} entries ({}), build tree {} entries; \
         {} s of churn: {} build bursts ({} files), {} download appends, {} edits in the {} root. Peak daemon \
         threads {}.{}\n",
        report.quiet_entries,
        report.shape,
        report.build_entries,
        report.seconds,
        report.build_bursts,
        report.build_files_created,
        report.download_appends,
        report.edits,
        report.edit_scope,
        report.daemon_threads_max,
        report
            .label
            .as_ref()
            .map(|label| format!(" Label: `{label}`."))
            .unwrap_or_default(),
    );
    let _ = writeln!(
        text,
        "Browse1 sampled concurrently: p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms; {} errors/timeouts at the 100 ms bound.\n",
        report.browse.p50_ms, report.browse.p95_ms, report.browse.p99_ms, report.browse_errors
    );
    let _ = writeln!(
        text,
        "| Scope | Search p50 ms | p95 | p99 | Next-page fetch p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |"
    );
    let _ = writeln!(text, "|---|---|---|---|---|---|---|---|---|");
    for scope in &report.scopes {
        let _ = writeln!(
            text,
            "| {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} % | {:.2} | {} | {} |",
            scope.scope,
            scope.search.p50_ms,
            scope.search.p95_ms,
            scope.search.p99_ms,
            scope.first_fetch.p95_ms,
            scope.arrays_hit_rate * 100.0,
            scope.queue_ms.p95_ms,
            format_counts(&scope.strategies),
            format_counts(&scope.errors),
        );
    }
    text
}

fn format_counts(counts: &BTreeMap<String, u64>) -> String {
    if counts.is_empty() {
        return "—".to_owned();
    }
    counts
        .iter()
        .map(|(name, count)| format!("{name}: {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- memory ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct MemorySample {
    vm_hwm_kib: u64,
    vm_rss_kib: u64,
    pss_kib: u64,
    private_kib: u64,
    threads: u64,
}

impl MemorySample {
    fn read(pid: u32) -> Self {
        let mut sample = Self::default();
        if let Some(status) = read_to_string(&format!("/proc/{pid}/status")) {
            for line in status.lines() {
                let value = |prefix: &str| {
                    line.strip_prefix(prefix)
                        .and_then(|rest| rest.split_whitespace().next())
                        .and_then(|number| number.parse().ok())
                };
                if let Some(kib) = value("VmHWM:") {
                    sample.vm_hwm_kib = kib;
                }
                if let Some(kib) = value("VmRSS:") {
                    sample.vm_rss_kib = kib;
                }
                if let Some(count) = value("Threads:") {
                    sample.threads = count;
                }
            }
        }
        if let Some(rollup) = read_to_string(&format!("/proc/{pid}/smaps_rollup")) {
            for line in rollup.lines() {
                let value = |prefix: &str| {
                    line.strip_prefix(prefix)
                        .and_then(|rest| rest.split_whitespace().next())
                        .and_then(|number| number.parse::<u64>().ok())
                };
                if let Some(kib) = value("Pss:") {
                    sample.pss_kib = kib;
                }
                if let Some(kib) = value("Private_Clean:") {
                    sample.private_kib += kib;
                }
                if let Some(kib) = value("Private_Dirty:") {
                    sample.private_kib += kib;
                }
            }
        }
        sample
    }
}

/// Samples a process every 100 ms until stopped, keeping the peak of every field.
struct Sampler {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<MemorySample>,
}

impl Sampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut peak = MemorySample::default();
            loop {
                let sample = MemorySample::read(pid);
                peak.vm_hwm_kib = peak.vm_hwm_kib.max(sample.vm_hwm_kib);
                peak.vm_rss_kib = peak.vm_rss_kib.max(sample.vm_rss_kib);
                peak.pss_kib = peak.pss_kib.max(sample.pss_kib);
                peak.private_kib = peak.private_kib.max(sample.private_kib);
                peak.threads = peak.threads.max(sample.threads);
                if flag.load(Ordering::Relaxed) {
                    return peak;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self { stop, thread }
    }

    fn finish(self) -> MemorySample {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap_or_default()
    }
}

#[derive(Debug, Default, Serialize)]
struct Phase {
    name: String,
    before: MemorySample,
    peak: MemorySample,
    after: MemorySample,
    elapsed_ms: f64,
    detail: String,
}

#[derive(Debug, Default, Serialize)]
struct SnapshotScale {
    entries: u64,
    payload_bytes: u64,
    writer_peak_delta_kib: u64,
    reader_peak_delta_kib: u64,
    client_peak_delta_kib: u64,
    decoded_rows: u64,
    decoded_storage_bytes: u64,
    elapsed_ms: f64,
}

#[derive(Debug, Default, Serialize)]
struct MemoryReport {
    label: Option<String>,
    daemon_entries: u64,
    phases: Vec<Phase>,
    snapshot_scales: Vec<SnapshotScale>,
}

#[allow(clippy::too_many_lines)]
pub fn memory(
    entries: u64,
    scales: &[u64],
    label: Option<String>,
    out: Option<&Path>,
) -> Result<()> {
    let binary = daemon_binary()?;
    let scratch = tempfile::tempdir().context("creating the memory scratch directory")?;
    let quiet = scratch.path().join("quiet");
    println!("generating a {entries}-entry tree");
    let made = build_fixture(&quiet, entries)?.entries;
    let runtime = tokio::runtime::Runtime::new()?;
    let mut report = MemoryReport {
        label,
        daemon_entries: made,
        ..MemoryReport::default()
    };
    runtime.block_on(async {
        // (a) cold index: from spawn to every root current.
        let started = Instant::now();
        let (daemon, roots) = PrivateDaemon::start(
            &binary,
            scratch.path(),
            std::slice::from_ref(&quiet),
            Duration::from_secs(600),
        )
        .await?;
        let pid = daemon.pid();
        let cold = MemorySample::read(pid);
        report.phases.push(Phase {
            name: "a: cold index".to_owned(),
            before: MemorySample::default(),
            peak: cold,
            after: cold,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            detail: format!(
                "{} entries indexed",
                roots.roots.iter().map(|root| root.entry_count).sum::<u64>()
            ),
        });

        // (b) arrays build: searches until the arrays answer.
        let phase = PhaseClock::start("b: arrays build", pid);
        let mut strategy = String::new();
        for _ in 0..600 {
            let (_, _, page) = daemon
                .proxy
                .search(&serde_json::to_string(
                    &Query::new("txt").with_page_size(10),
                )?)
                .await?;
            let value: serde_json::Value = serde_json::from_str(&page)?;
            if let Some(cursor) = value["cursor"].as_str() {
                let _ = daemon.proxy.close_cursor(cursor).await;
            }
            strategy = value["strategy"].as_str().unwrap_or("").to_owned();
            if strategy == "arrays" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        report
            .phases
            .push(phase.finish(format!("answered by {strategy}")));

        // (c) two search clients, one materialized snapshot of the whole tree and a forced arrays
        // rebuild, all at once.
        let phase = PhaseClock::start("c: 2 clients + snapshot + rebuild", pid);
        fs::write(quiet.join("invalidate-arrays.txt"), b"x")?;
        let clients: Vec<_> = (0..2)
            .map(|number| {
                let connection = daemon.connection.clone();
                tokio::spawn(async move {
                    let proxy = FilesIndexProxy::new(&connection).await?;
                    for round in 0..40 {
                        let needle = CHURN_NEEDLES[(number + round) % CHURN_NEEDLES.len()];
                        let (cursor, _, _) = proxy
                            .search(&serde_json::to_string(&Query::new(needle))?)
                            .await?;
                        let _ = proxy.close_cursor(&cursor).await;
                    }
                    anyhow::Ok(())
                })
            })
            .collect();
        let (descriptor, manifest) = daemon.proxy.open_snapshot("{}").await?;
        let manifest: templar_file_index::snapshot::SnapshotManifest =
            serde_json::from_str(&manifest)?;
        drop(descriptor);
        for client in clients {
            client.await??;
        }
        report.phases.push(phase.finish(format!(
            "snapshot {} entries, memfd {} bytes",
            manifest.entry_count,
            manifest.payload_len + 64
        )));
        daemon.stop()
    })?;

    // (d) the snapshot writer and reader at several sizes, in process on synthetic rows, so the
    // heap a frame costs is measured without a million files on disk.
    for &scale in scales {
        report
            .snapshot_scales
            .push(snapshot_scale(scratch.path(), scale)?);
    }
    print_and_write("memory", &report, &memory_markdown(&report), out)
}

struct PhaseClock {
    name: &'static str,
    pid: u32,
    before: MemorySample,
    sampler: Sampler,
    started: Instant,
}

impl PhaseClock {
    fn start(name: &'static str, pid: u32) -> Self {
        Self {
            name,
            pid,
            before: MemorySample::read(pid),
            sampler: Sampler::start(pid),
            started: Instant::now(),
        }
    }

    fn finish(self, detail: String) -> Phase {
        let elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        let peak = self.sampler.finish();
        Phase {
            name: self.name.to_owned(),
            before: self.before,
            peak,
            after: MemorySample::read(self.pid),
            elapsed_ms,
            detail,
        }
    }
}

/// Resets this process's peak RSS so the next `VmHWM` read covers only what follows.
fn reset_peak() {
    let _ = fs::write("/proc/self/clear_refs", "5");
}

fn own_peak_kib() -> u64 {
    MemorySample::read(std::process::id()).vm_hwm_kib
}

fn snapshot_scale(scratch: &Path, entries: u64) -> Result<SnapshotScale> {
    let directory = scratch.join(format!("synthetic-{entries}"));
    let tree = directory.join("tree");
    fs::create_dir_all(&tree)?;
    let index = Index::open(&directory.join("index.sqlite3"), IndexConfig::default())?;
    let root = index
        .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())?
        .root;
    index.scan_root(root, ScanOptions::default())?;
    println!("inserting {entries} synthetic rows");
    let mut batch = Vec::with_capacity(10_000);
    for number in 0..entries {
        let path = tree.join(format!("d{}/f{number}.txt", number / 1_000));
        let kind = EntryKind::Regular;
        batch.push(Mutation::Upsert {
            root,
            path: NativePath::from_path(&path),
            meta: EntryMeta {
                kind,
                size_bytes: number % 4096,
                mtime_ns: number as i64,
                ..EntryMeta::default()
            },
        });
        if batch.len() == 10_000 || number + 1 == entries {
            index.apply(MutationBatch::new(
                MutationSource::Reconcile,
                std::mem::take(&mut batch),
            ))?;
        }
    }
    let descriptor = rustix::fs::memfd_create(
        "index-memory-frame",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )?;
    let mut sink = fs::File::from(descriptor);
    let baseline = MemorySample::read(std::process::id()).vm_rss_kib;
    reset_peak();
    let started = Instant::now();
    let mut manifest = write_frame(&index, &mut sink)?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let writer_peak = own_peak_kib().saturating_sub(baseline);
    rustix::fs::fcntl_add_seals(
        &sink,
        rustix::fs::SealFlags::WRITE
            | rustix::fs::SealFlags::GROW
            | rustix::fs::SealFlags::SHRINK
            | rustix::fs::SealFlags::SEAL,
    )?;
    manifest.sealed = true;
    drop(index);
    let baseline = MemorySample::read(std::process::id()).vm_rss_kib;
    reset_peak();
    let frame = templar_client::tree_source::SnapshotMap::new(sink.into(), &manifest)?;
    let decoded = decode_frame(frame.as_bytes())?;
    let reader_peak = own_peak_kib().saturating_sub(baseline);
    if decoded != manifest.entry_count {
        bail!(
            "decoded {decoded} entries from a frame of {}",
            manifest.entry_count
        );
    }
    // Keep both the mapping and the one final RawEntry vector live while measuring. No payload
    // Vec or intermediate owned SnapshotEntry vector is constructed by this production decoder.
    let load =
        templar_client::tree_source::decode_snapshot_tree(frame.as_bytes(), &manifest, &tree)?;
    let client_peak = own_peak_kib().saturating_sub(baseline);
    let row_bytes = load.entries.first().map_or(0, std::mem::size_of_val);
    let decoded_storage_bytes = (load.entries.capacity() * row_bytes) as u64
        + load
            .entries
            .iter()
            .map(|entry| entry.path.as_os_str().len() as u64)
            .sum::<u64>();
    let decoded_rows = load.entries.len() as u64;
    drop(load);
    drop(frame);
    fs::remove_dir_all(&directory)?;
    Ok(SnapshotScale {
        entries: manifest.entry_count,
        payload_bytes: manifest.payload_len,
        writer_peak_delta_kib: writer_peak,
        reader_peak_delta_kib: reader_peak,
        client_peak_delta_kib: client_peak,
        decoded_rows,
        decoded_storage_bytes,
        elapsed_ms,
    })
}

fn write_frame(
    index: &Index,
    sink: &mut fs::File,
) -> Result<templar_file_index::snapshot::SnapshotManifest> {
    let request = SnapshotRequest {
        fields: templar_file_index::query::FieldMask::ALL
            .with(templar_file_index::query::FieldMask::COVERAGE)
            .with(templar_file_index::query::FieldMask::AGGREGATE),
        ..SnapshotRequest::default()
    };
    Ok(index.write_snapshot(
        sink,
        &request,
        &templar_file_index::snapshot::SnapshotLimits::default(),
        &templar_file_index::CallContext::internal(),
    )?)
}

fn decode_frame(frame: &[u8]) -> Result<u64> {
    let reader = SnapshotReader::new(frame).map_err(|error| anyhow::anyhow!(error))?;
    reader
        .entries_iter()
        .map_err(|error| anyhow::anyhow!(error))?
        .try_fold(0u64, |count, entry| {
            entry
                .map(|_| count + 1)
                .map_err(|error| anyhow::anyhow!(error))
        })
}

fn memory_markdown(report: &MemoryReport) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# Index memory — {}\n", today());
    let _ = writeln!(
        text,
        "Owned private bus and `templar-filed` over a {}-entry tree.{}\n",
        report.daemon_entries,
        report
            .label
            .as_ref()
            .map(|label| format!(" Label: `{label}`."))
            .unwrap_or_default()
    );
    let _ = writeln!(
        text,
        "| Phase | RSS before MiB | Peak RSS MiB | Peak PSS MiB | Peak private MiB | Peak threads | ms | Detail |"
    );
    let _ = writeln!(text, "|---|---|---|---|---|---|---|---|");
    for phase in &report.phases {
        let _ = writeln!(
            text,
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {:.0} | {} |",
            phase.name,
            mib(phase.before.vm_rss_kib),
            mib(phase.peak.vm_rss_kib),
            mib(phase.peak.pss_kib),
            mib(phase.peak.private_kib),
            phase.peak.threads,
            phase.elapsed_ms,
            phase.detail
        );
    }
    let _ = writeln!(
        text,
        "\nSnapshot writer and reader, in process on synthetic rows:\n"
    );
    let _ = writeln!(
        text,
        "| Entries | Sealed payload MiB | Writer peak RSS delta MiB | Borrowed reader RSS delta MiB | Mapped client + RawEntry RSS delta MiB | Decoded storage MiB | Write ms |"
    );
    let _ = writeln!(text, "|---|---|---|---|---|---|---|");
    for scale in &report.snapshot_scales {
        let _ = writeln!(
            text,
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} |",
            scale.entries,
            scale.payload_bytes as f64 / 1_048_576.0,
            mib(scale.writer_peak_delta_kib),
            mib(scale.reader_peak_delta_kib),
            mib(scale.client_peak_delta_kib),
            scale.decoded_storage_bytes as f64 / 1_048_576.0,
            scale.elapsed_ms
        );
    }
    let _ = writeln!(
        text,
        "\nRSS deltas use `/proc/self/clear_refs=5` and VmHWM, not an allocation profiler. Synthetic scales reuse one process and allocator; retained pages can reduce a later delta. Reader includes mapped pages; client includes that same mapping plus the final owned paths/vector. Decoded storage counts vector capacity and path lengths, excluding allocator overhead. Unlike the before run, frames include COVERAGE/AGGREGATE for the production tree decoder.\n"
    );
    text
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

// ---- layout ---------------------------------------------------------------------------------

#[derive(Debug, Default, Serialize)]
struct LayoutReport {
    label: Option<String>,
    entries: u64,
    shape: String,
    rows: u64,
    build_ms: f64,
    build_peak_delta_kib: u64,
    total_bytes: u64,
    capacity_bytes: u64,
    bytes_per_row: f64,
    columns: Vec<templar_file_index::ArrayColumn>,
    queries: Vec<(String, Latency)>,
}

pub fn layout(
    entries: u64,
    realistic: bool,
    label: Option<String>,
    out: Option<&Path>,
) -> Result<()> {
    let scratch = tempfile::tempdir().context("creating the layout scratch directory")?;
    let tree = scratch.path().join("tree");
    println!("generating a {entries}-entry tree");
    let made = if realistic {
        build_realistic(&tree, entries)?
    } else {
        build_fixture(&tree, entries)?.entries
    };
    let index = Index::open(
        &scratch.path().join("index.sqlite3"),
        IndexConfig {
            search_arrays: true,
            ..IndexConfig::default()
        },
    )?;
    let root = index
        .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())?
        .root;
    index.scan_root(root, ScanOptions::default())?;
    let baseline = MemorySample::read(std::process::id()).vm_rss_kib;
    reset_peak();
    let started = Instant::now();
    let rows = index.prepare_search_arrays()?;
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let build_peak = own_peak_kib().saturating_sub(baseline);
    let stats = index
        .search_array_stats()
        .context("the arrays were just built")?;
    let mut queries = Vec::new();
    for needle in CHURN_NEEDLES {
        let query = Query::new(*needle).with_page_size(100);
        let mut timings = Vec::new();
        for _ in 0..50 {
            let started = Instant::now();
            let result = index.search(&query, &CallContext::internal())?;
            timings.push(started.elapsed().as_secs_f64() * 1000.0);
            if result.strategy != "arrays" {
                bail!("the arrays did not answer `{needle}`: {}", result.strategy);
            }
            index.close_cursor(&result.cursor, &CallContext::internal());
        }
        queries.push(((*needle).to_owned(), Latency::of(timings)));
    }
    let report = LayoutReport {
        label,
        entries: made,
        shape: if realistic { "realistic" } else { "synthetic" }.to_owned(),
        rows: rows as u64,
        build_ms,
        build_peak_delta_kib: build_peak,
        total_bytes: stats.bytes,
        capacity_bytes: stats.capacity_bytes,
        bytes_per_row: stats.bytes as f64 / rows.max(1) as f64,
        columns: stats.columns,
        queries,
    };
    print_and_write("layout", &report, &layout_markdown(&report), out)
}

fn layout_markdown(report: &LayoutReport) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# Search-array layout — {}\n", today());
    let _ = writeln!(
        text,
        "{} rows from a {} {}-entry tree; built in {:.1} ms with a peak RSS rise of {:.1} MiB. \
        Columns hold {:.1} MiB in use ({:.1} B/row) and {:.1} MiB of capacity.{}\n",
        report.rows,
        report.shape,
        report.entries,
        report.build_ms,
        mib(report.build_peak_delta_kib),
        report.total_bytes as f64 / 1_048_576.0,
        report.bytes_per_row,
        report.capacity_bytes as f64 / 1_048_576.0,
        report
            .label
            .as_ref()
            .map(|label| format!(" Label: `{label}`."))
            .unwrap_or_default()
    );
    let _ = writeln!(text, "| Column | Bytes in use | Capacity bytes |");
    let _ = writeln!(text, "|---|---|---|");
    for column in &report.columns {
        let _ = writeln!(
            text,
            "| {} | {} | {} |",
            column.name, column.bytes, column.capacity_bytes
        );
    }
    let _ = writeln!(text, "\n| Needle | p50 ms | p95 ms |");
    let _ = writeln!(text, "|---|---|---|");
    for (needle, latency) in &report.queries {
        let _ = writeln!(
            text,
            "| `{needle}` | {:.2} | {:.2} |",
            latency.p50_ms, latency.p95_ms
        );
    }
    text
}

// ---- reports --------------------------------------------------------------------------------

fn print_and_write(
    subcommand: &str,
    report: &impl Serialize,
    markdown: &str,
    out: Option<&Path>,
) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    println!("{json}");
    println!("{markdown}");
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out, &json).with_context(|| format!("writing {}", out.display()))?;
        let markdown_path = out.with_extension("md");
        fs::write(&markdown_path, markdown)
            .with_context(|| format!("writing {}", markdown_path.display()))?;
        println!("wrote {} and {}", out.display(), markdown_path.display());
    } else {
        println!(
            "(pass --out docs/measurements/index-{subcommand}-{}.json to keep this report)",
            today()
        );
    }
    Ok(())
}

/// Today's UTC date as `YYYY-MM-DD`, from the civil-from-days algorithm.
fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400) as i64;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
