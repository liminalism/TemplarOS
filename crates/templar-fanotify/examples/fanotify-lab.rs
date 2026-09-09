//! The measurement harness behind `docs/measurements/fanotify-<date>.md`.
//!
//! An example rather than a `benches/` target or a test, for the reason `watch.rs` documents: the
//! index's fixture guard panics on any watch root outside the temporary directory as soon as the
//! running executable has a `deps` path component, which every test and bench binary does. An
//! example binary lands in `target/<profile>/examples/`, so it can measure a real tree.
//!
//! ```text
//! cargo run --release -p templar-fanotify --example fanotify-lab -- probe [PATH...]
//! cargo run --release -p templar-fanotify --example fanotify-lab -- arm --dirs 100000
//! cargo run --release -p templar-fanotify --example fanotify-lab -- events --files 20000
//! cargo run --release -p templar-fanotify --example fanotify-lab -- burst --files 20000
//! cargo run --release -p templar-fanotify --example fanotify-lab -- stall --files 40000
//! sudo target/release/examples/fanotify-lab filesystem /some/mount   # needs CAP_SYS_ADMIN
//! ```
//!
//! Every workload builds its own fixture under `$TMPDIR` and deletes it again. Nothing here ever
//! touches a real home.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use templar_fanotify::{Config, FanotifySource, Group, MarkScope, sys};
use templar_file_index::watch::{EventSource, InotifySource, RawKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map_or("probe", String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    match command {
        "probe" => probe(rest),
        "arm" => arm(rest),
        "events" => events(rest, false),
        "burst" => events(rest, true),
        "stall" => stall(rest),
        "filesystem" => filesystem(rest),
        other => {
            eprintln!(
                "unknown command {other:?}: probe | arm | events | burst | stall | filesystem"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Arguments and reporting.
// ---------------------------------------------------------------------------------------------

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|at| args.get(at + 1))
        .cloned()
}

/// Every argument that is neither a `--flag` nor a flag's value: the positional paths.
fn positional(args: &[String]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
        } else if arg.starts_with("--") {
            skip = true;
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    paths
}

fn number(args: &[String], name: &str, default: usize) -> usize {
    flag(args, name).map_or(default, |value| value.parse().unwrap_or(default))
}

fn row(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<38} {value}");
}

// ---------------------------------------------------------------------------------------------
// probe: what this kernel and this process are actually allowed to do.
// ---------------------------------------------------------------------------------------------

fn probe(args: &[String]) {
    println!("kernel {}", read_trimmed("/proc/sys/kernel/osrelease"));
    println!("\nlimits");
    for (label, file) in [
        ("fs.inotify.max_user_watches", "inotify/max_user_watches"),
        ("fs.inotify.max_queued_events", "inotify/max_queued_events"),
        (
            "fs.inotify.max_user_instances",
            "inotify/max_user_instances",
        ),
        ("fs.fanotify.max_user_marks", "fanotify/max_user_marks"),
        ("fs.fanotify.max_user_groups", "fanotify/max_user_groups"),
        (
            "fs.fanotify.max_queued_events",
            "fanotify/max_queued_events",
        ),
    ] {
        row(label, read_trimmed(&format!("/proc/sys/fs/{file}")));
    }

    println!("\ngroup flags (this process, {})", uid_line());
    for (label, config) in [
        ("FAN_CLASS_NOTIF + DFID_NAME + FID", Config::default()),
        (
            "  ... + TARGET_FID",
            Config {
                target_fid: true,
                ..Config::default()
            },
        ),
    ] {
        row(label, describe(Group::open(config).map(|_| ())));
    }
    row(
        "FAN_CLASS_NOTIF without any fid flags",
        describe(
            sys::init(
                libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC,
                libc::O_RDONLY as libc::c_uint,
            )
            .map(|_| ()),
        ),
    );

    let mut paths = positional(args);
    if paths.is_empty() {
        paths = vec![
            std::env::temp_dir(),
            std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from),
            PathBuf::from("/"),
        ];
    }

    println!("\nper path");
    for path in &paths {
        println!("  {}", path.display());
        row(
            "  fsid",
            sys::fsid_of(path).map_or_else(|error| format!("{error}"), |id| format!("{id:?}")),
        );
        row(
            "  file handle",
            sys::handle_of(path).map_or_else(
                |error| format!("{error}"),
                |(kind, bytes)| format!("type {kind}, {} bytes", bytes.len()),
            ),
        );
        for scope in [MarkScope::Inode, MarkScope::Mount, MarkScope::Filesystem] {
            let config = Config {
                scope,
                ..Config::default()
            };
            let attempt = Group::open(config).and_then(|group| group.add(path));
            row(
                &format!("  FAN_MARK_{scope:?}").to_uppercase(),
                describe(attempt),
            );
        }
        let inotify = InotifySource::open().and_then(|mut source| source.add_watch(path));
        row("  inotify add_watch", describe(inotify.map(|_| ())));
    }

    // One `EventSource` is one group, and the index wants several roots behind one source. A
    // group in fid mode turns out not to accept marks on a zero-fsid filesystem (fuse) alongside
    // any other, so this is a per-source constraint, not a per-mark one.
    println!("\none group, every path in turn");
    match Group::open(Config::default()) {
        Ok(group) => {
            for path in &paths {
                row(&format!("  {}", path.display()), describe(group.add(path)));
            }
        }
        Err(error) => row("  group", describe_error(&error)),
    }

    println!("\nresolving a handle back to a path (needs CAP_DAC_READ_SEARCH)");
    row(
        "  open_by_handle_at",
        "not attempted: this crate never calls it, by design",
    );
}

fn describe(result: std::io::Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!(
            "{} ({})",
            error
                .raw_os_error()
                .map_or_else(|| "?".to_owned(), errno_name),
            error
        ),
    }
}

fn errno_name(code: i32) -> String {
    match code {
        libc::EPERM => "EPERM".to_owned(),
        libc::EXDEV => "EXDEV".to_owned(),
        libc::ENODEV => "ENODEV".to_owned(),
        libc::EOPNOTSUPP => "EOPNOTSUPP".to_owned(),
        libc::ENOSPC => "ENOSPC".to_owned(),
        libc::EINVAL => "EINVAL".to_owned(),
        other => format!("errno {other}"),
    }
}

/// The `Uid:` line of `/proc/self/status`, so a run under `sudo` is unmistakable in the report.
fn uid_line() -> String {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .unwrap_or("Uid: ?")
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// arm: what it costs to cover a tree of known size.
// ---------------------------------------------------------------------------------------------

fn arm(args: &[String]) {
    let dirs = number(args, "--dirs", 50_000);
    let fixture = Fixture::directories(dirs);
    println!(
        "tree {} ({} directories)",
        fixture.root.display(),
        fixture.dirs.len()
    );

    for backend in ["inotify", "fanotify"] {
        // Walk first, so the dentry cache is warm for both runs and the slab delta is the marks
        // rather than the walk.
        let paths = fixture.dirs.clone();
        let before = Memory::sample();
        let started = Instant::now();
        let mut armed = 0u64;
        let mut refused: BTreeMap<String, u64> = BTreeMap::new();
        let mut source = open_backend(backend);
        for path in &paths {
            match source.add_watch(path) {
                Ok(_) => armed += 1,
                Err(error) => {
                    *refused
                        .entry(
                            error
                                .raw_os_error()
                                .map_or_else(|| "?".to_owned(), errno_name),
                        )
                        .or_default() += 1;
                }
            }
        }
        let elapsed = started.elapsed();
        let after = Memory::sample();
        println!("\n{} ({})", backend, source.backend());
        row("armed", armed);
        row("refused", format!("{refused:?}"));
        row("arm wall time", format!("{:.3} s", elapsed.as_secs_f64()));
        row(
            "per directory",
            format!(
                "{:.1} us",
                elapsed.as_secs_f64() * 1e6 / armed.max(1) as f64
            ),
        );
        row(
            "SUnreclaim delta",
            format!("{} kB", after.sunreclaim - before.sunreclaim),
        );
        row(
            "  per armed directory",
            format!(
                "{:.0} bytes",
                (after.sunreclaim - before.sunreclaim) as f64 * 1024.0 / armed.max(1) as f64
            ),
        );
        row(
            "process VmRSS delta",
            format!("{} kB", after.rss - before.rss),
        );
        row(
            "  per armed directory",
            format!(
                "{:.0} bytes",
                (after.rss - before.rss) as f64 * 1024.0 / armed.max(1) as f64
            ),
        );
        drop(source);
    }
}

// ---------------------------------------------------------------------------------------------
// events: throughput, merging and overflow.
// ---------------------------------------------------------------------------------------------

fn events(args: &[String], burst: bool) {
    let files = number(args, "--files", 20_000);
    let dirs = number(args, "--dirs", 200);
    let fixture = Fixture::directories(dirs);
    println!(
        "tree {} ({} directories), {files} files{}",
        fixture.root.display(),
        fixture.dirs.len(),
        if burst {
            ", written then deleted in one burst"
        } else {
            ""
        }
    );

    for backend in ["inotify", "fanotify"] {
        let mut source = open_backend(backend);
        let mut armed = 0u64;
        for path in &fixture.dirs {
            if source.add_watch(path).is_ok() {
                armed += 1;
            }
        }

        let started = Instant::now();
        let written = write_files(&fixture, files, burst);
        let generated = started.elapsed();

        // Drain until the source goes quiet for a full pairing window.
        let mut raw = 0u64;
        let mut kinds: BTreeMap<&'static str, u64> = BTreeMap::new();
        let mut overflow = 0u64;
        let drain_started = Instant::now();
        loop {
            match source.poll(Some(Duration::from_millis(400))) {
                Ok(batch) if batch.is_empty() => break,
                Ok(batch) => {
                    for event in batch {
                        raw += 1;
                        if event.kind == RawKind::Overflow {
                            overflow += 1;
                        }
                        *kinds.entry(kind_name(event.kind)).or_default() += 1;
                    }
                }
                Err(error) => {
                    println!("  poll failed: {error}");
                    break;
                }
            }
            if drain_started.elapsed() > Duration::from_secs(60) {
                println!("  drain gave up after 60 s");
                break;
            }
        }
        let drained = drain_started.elapsed();

        println!("\n{} ({})", backend, source.backend());
        row("watches armed", armed);
        row("filesystem operations", written);
        row(
            "generating them took",
            format!("{:.3} s", generated.as_secs_f64()),
        );
        row("raw events delivered", raw);
        row(
            "events per operation",
            format!("{:.3}", raw as f64 / written.max(1) as f64),
        );
        row("drain wall time", format!("{:.3} s", drained.as_secs_f64()));
        row(
            "delivery rate",
            format!(
                "{:.0} events/s",
                raw as f64 / drained.as_secs_f64().max(1e-9)
            ),
        );
        row("queue overflows", overflow);
        row("by kind", format!("{kinds:?}"));
        drop(source);
        // Put the tree back the way the next backend expects to find it.
        fixture.reset();
    }
}

fn kind_name(kind: RawKind) -> &'static str {
    match kind {
        RawKind::Created => "created",
        RawKind::Deleted => "deleted",
        RawKind::Written => "written",
        RawKind::Attributes => "attributes",
        RawKind::MovedFrom => "moved_from",
        RawKind::MovedTo => "moved_to",
        RawKind::SelfGone => "self_gone",
        RawKind::Unmounted => "unmounted",
        RawKind::Overflow => "overflow",
        RawKind::WatchDropped => "watch_dropped",
    }
}

/// Create `files` files spread over the fixture, optionally deleting each one straight away —
/// which is the shape a `cargo build` has, and the shape that provokes fanotify's event merging.
fn write_files(fixture: &Fixture, files: usize, burst: bool) -> u64 {
    let mut operations = 0u64;
    for index in 0..files {
        let dir = &fixture.dirs[index % fixture.dirs.len()];
        let path = dir.join(format!("f{index}.o"));
        if let Ok(mut file) = fs::File::create(&path) {
            let _ = file.write_all(b"object");
            operations += 2; // create + close-write
        }
        if burst {
            let renamed = dir.join(format!("f{index}.o.tmp"));
            let _ = fs::rename(&path, &renamed);
            let _ = fs::remove_file(&renamed);
            operations += 2; // rename + delete
        }
    }
    operations
}

// ---------------------------------------------------------------------------------------------
// filesystem: variant B, which needs CAP_SYS_ADMIN.
// ---------------------------------------------------------------------------------------------

/// One `FAN_MARK_FILESYSTEM` mark instead of one mark per directory.
///
/// Two questions, and the second is the one that decides the variant. Does the mark go on at all —
/// on Btrfs a subvolume whose fsid differs from the superblock root's answers `EXDEV`. And if it
/// does, can the listener attribute the events it then receives? Every event names its directory
/// by opaque handle, and turning a handle into a path needs `open_by_handle_at(2)`, which needs
/// `CAP_DAC_READ_SEARCH`. So an unprivileged listener holding a filesystem-marked fd still needs a
/// handle-to-path map of its own, one entry per directory: the kernel's per-directory cost is
/// gone, the indexer's is not.
fn filesystem(args: &[String]) {
    let Some(root) = positional(args).into_iter().next() else {
        eprintln!("usage: fanotify-lab filesystem <path-on-the-filesystem-to-mark> [--dirs N]");
        std::process::exit(2);
    };
    let dirs = number(args, "--dirs", 200);

    let mut source = match FanotifySource::open(Config {
        scope: MarkScope::Filesystem,
        ..Config::default()
    }) {
        Ok(source) => source,
        Err(error) => {
            println!(
                "could not open a fanotify group: {}",
                describe_error(&error)
            );
            return;
        }
    };
    let before = Memory::sample();
    if let Err(error) = source.arm_scope(&root) {
        println!(
            "FAN_MARK_FILESYSTEM on {} -> {}",
            root.display(),
            describe_error(&error)
        );
        if error.raw_os_error() == Some(libc::EXDEV) {
            println!(
                "  EXDEV: this path's fsid differs from its superblock root's. On Btrfs that is \n                 \x20 what a subvolume looks like, and it means one filesystem mark cannot cover it."
            );
        }
        return;
    }
    let after = Memory::sample();
    println!("FAN_MARK_FILESYSTEM on {} -> ok", root.display());
    row(
        "SUnreclaim delta for one mark",
        format!("{} kB", after.sunreclaim - before.sunreclaim),
    );

    // A subtree the listener knows about, and one it does not.
    let known = root.join(format!("fanotify-lab-known-{}", std::process::id()));
    let unknown = root.join(format!("fanotify-lab-unknown-{}", std::process::id()));
    for path in [&known, &unknown] {
        let _ = fs::remove_dir_all(path);
        fs::create_dir_all(path).expect("a fixture directory");
    }
    let mut registered = 0u64;
    for index in 0..dirs {
        let path = known.join(format!("d{index}"));
        if fs::create_dir(&path).is_ok() && source.add_watch(&path).is_ok() {
            registered += 1;
        }
    }
    // Drain everything the directory creation itself produced.
    while let Ok(batch) = source.poll(Some(Duration::from_millis(200))) {
        if batch.is_empty() {
            break;
        }
    }

    let mut operations = 0u64;
    for index in 0..dirs {
        for parent in [known.join(format!("d{index}")), unknown.clone()] {
            let file = parent.join(format!("f{index}"));
            if fs::write(&file, b"x").is_ok() {
                operations += 2;
                let _ = fs::remove_file(&file);
                operations += 1;
            }
        }
    }

    let mut delivered = 0u64;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match source.poll(Some(Duration::from_millis(400))) {
            Ok(batch) if batch.is_empty() => break,
            Ok(batch) => delivered += batch.len() as u64,
            Err(_) => break,
        }
    }

    row("directories registered by handle", registered);
    row("filesystem operations", operations);
    row("events attributed to a known directory", delivered);
    row(
        "events the listener had to drop",
        "every operation under the unregistered subtree: the kernel reported them, the handle was \
         not in the map",
    );
    let _ = fs::remove_dir_all(&known);
    let _ = fs::remove_dir_all(&unknown);
}

fn describe_error(error: &std::io::Error) -> String {
    format!(
        "{} ({error})",
        error
            .raw_os_error()
            .map_or_else(|| "?".to_owned(), errno_name)
    )
}

// ---------------------------------------------------------------------------------------------
// stall: what the kernel queue does when nobody is reading it.
// ---------------------------------------------------------------------------------------------

/// Both backends drain their fd from a dedicated reader thread, which is precisely why neither
/// overflows in the `events` workload: the kernel queue never fills, and the backlog becomes an
/// in-process channel instead. This workload removes the reader thread and asks the other
/// question — how many filesystem operations fit in `max_queued_events` before the kernel starts
/// dropping them, which is where fanotify's event merging either helps or does not.
fn stall(args: &[String]) {
    let files = number(args, "--files", 40_000);
    let dirs = number(args, "--dirs", 200);
    let fixture = Fixture::directories(dirs);
    let queued = read_trimmed("/proc/sys/fs/inotify/max_queued_events");
    println!(
        "tree {} ({} directories), {files} files, nothing reading (max_queued_events {queued})",
        fixture.root.display(),
        fixture.dirs.len()
    );

    // fanotify: mark, generate, then read once.
    {
        let group = Group::open(Config::default()).expect("a fanotify group");
        for dir in &fixture.dirs {
            group.add(dir).expect("a mark");
        }
        let operations = write_files(&fixture, files, true);
        let (events, overflows) = drain_fanotify(&group);
        report_stall("fanotify-inode", operations, events, overflows);
        fixture.reset();
    }

    // inotify: the same, on a raw fd.
    {
        let inotify = inotify::Inotify::init().expect("an inotify instance");
        let mut watches = inotify.watches();
        for dir in &fixture.dirs {
            watches.add(dir, inotify_mask()).expect("a watch");
        }
        let operations = write_files(&fixture, files, true);
        let (events, overflows) = drain_inotify(inotify);
        report_stall("inotify", operations, events, overflows);
        fixture.reset();
    }
}

fn inotify_mask() -> inotify::WatchMask {
    use inotify::WatchMask as M;
    M::CREATE
        | M::DELETE
        | M::DELETE_SELF
        | M::MOVED_FROM
        | M::MOVED_TO
        | M::MOVE_SELF
        | M::CLOSE_WRITE
        | M::ATTRIB
        | M::EXCL_UNLINK
        | M::ONLYDIR
}

fn report_stall(backend: &str, operations: u64, events: u64, overflows: u64) {
    println!("\n{backend}");
    row("filesystem operations", operations);
    row("events the kernel kept", events);
    row("overflow markers", overflows);
    row(
        "operations lost",
        if overflows == 0 {
            "none".to_owned()
        } else {
            "unknown: an overflow says only that the history is gone".to_owned()
        },
    );
}

fn drain_fanotify(group: &Group) -> (u64, u64) {
    let mut buffer = vec![0u8; 256 * 1024];
    let (mut events, mut overflows) = (0u64, 0u64);
    loop {
        // The group is blocking, so stop as soon as a poll says there is nothing left.
        if !sys::readable(group.as_fd()).unwrap_or(false) {
            break;
        }
        let Ok(read) = sys::read(group.as_fd(), &mut buffer) else {
            break;
        };
        let Ok(batch) = templar_fanotify::event::parse(&buffer[..read]) else {
            break;
        };
        for event in batch {
            events += 1;
            if event.is_overflow() {
                overflows += 1;
            }
        }
    }
    (events, overflows)
}

fn drain_inotify(mut inotify: inotify::Inotify) -> (u64, u64) {
    let mut buffer = vec![0u8; 256 * 1024];
    let (mut events, mut overflows) = (0u64, 0u64);
    loop {
        let Ok(batch) = inotify.read_events(&mut buffer) else {
            break;
        };
        let mut seen = 0u64;
        for event in batch {
            seen += 1;
            events += 1;
            if event.mask.contains(inotify::EventMask::Q_OVERFLOW) {
                overflows += 1;
            }
        }
        if seen == 0 {
            break;
        }
    }
    (events, overflows)
}

// ---------------------------------------------------------------------------------------------
// Fixtures and sampling.
// ---------------------------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    dirs: Vec<PathBuf>,
    _guard: tempfile::TempDir,
}

impl Fixture {
    /// A fan-out tree of `count` directories under `$TMPDIR`, 100 per level so no single
    /// directory holds an unrealistic number of children.
    fn directories(count: usize) -> Self {
        let guard = tempfile::Builder::new()
            .prefix("fanotify-lab-")
            .tempdir()
            .expect("a temporary directory");
        let root = guard.path().to_path_buf();
        let mut dirs = vec![root.clone()];
        let mut index = 0usize;
        while dirs.len() < count {
            let parent = dirs[index % dirs.len().max(1)].clone();
            for child in 0..100 {
                if dirs.len() >= count {
                    break;
                }
                let path = parent.join(format!("d{}_{child}", dirs.len()));
                if fs::create_dir(&path).is_ok() {
                    dirs.push(path);
                }
            }
            index += 1;
        }
        Self {
            root,
            dirs,
            _guard: guard,
        }
    }

    /// Remove every file the workload created, leaving the directories.
    fn reset(&self) {
        for dir in &self.dirs {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

struct Memory {
    sunreclaim: i64,
    rss: i64,
}

impl Memory {
    fn sample() -> Self {
        Self {
            sunreclaim: meminfo_field("/proc/meminfo", "SUnreclaim:"),
            rss: meminfo_field("/proc/self/status", "VmRSS:"),
        }
    }
}

fn meminfo_field(file: &str, key: &str) -> i64 {
    fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with(key))
        .and_then(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

fn read_trimmed(path: &str) -> String {
    fs::read_to_string(path)
        .map_or_else(|error| format!("<{error}>"), |text| text.trim().to_owned())
}

fn open_backend(name: &str) -> Box<dyn EventSource> {
    match name {
        "inotify" => Box::new(InotifySource::open().expect("an inotify instance")),
        "fanotify" => Box::new(FanotifySource::unprivileged().expect("a fanotify group")),
        other => panic!("unknown backend {other}"),
    }
}
