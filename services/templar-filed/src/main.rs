//! `templar-filed` — the file service daemon (`org.templaros.Files`).
//!
//! Starts by resuming whatever the journal shows unfinished: that is the difference between a
//! file manager and a file *service*. A copy that was running when the machine lost power picks
//! up where it stopped, rather than leaving the user to work out what happened to it.

use anyhow::{Context, Result};
use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use templar_filed::{
    ExecutorConfig, SqliteJournal,
    dbus::{
        ActionsApi, BrowseApi, FileService, OperationsApi, PreviewsApi, RecentsApi, SearchApi,
        TrashApi, VolumesApi,
    },
    index_host::{Index1Signals, IndexApi},
    lastdir::LastDirectoryStore,
    observer,
    recents::Recents,
    search::Index,
    storage,
};
use templar_types::dbus::files as names;
use zbus::connection;

/// TemplarOS file service.
#[derive(Debug, Parser)]
#[command(name = "templar-filed", version, about)]
struct Cli {
    /// Root-volume reserve as a percentage of its size. Zero disables the reserve entirely,
    /// which is only sensible in a test.
    #[arg(long, default_value_t = 5)]
    reserve_percent: u64,

    /// State database path (default: `$XDG_STATE_HOME/templaros/session.db`).
    #[arg(long)]
    state_db: Option<PathBuf>,

    /// Directory to index at start (repeatable). Defaults to the user's home directory.
    #[arg(long = "index-root")]
    index_roots: Vec<PathBuf>,

    /// The shared index's cache database (default: `$XDG_CACHE_HOME/templaros/file-index-v1.sqlite3`).
    #[arg(long)]
    index_db: Option<PathBuf>,

    /// Entry budget for a scan; unset means uncapped. A budget trip commits `complete = false`
    /// and leaves the root `Stale` rather than truncating silently.
    #[arg(long)]
    index_max_entries: Option<u64>,

    /// Optional staged-byte budget. A trip is reported incomplete; there is no hidden default cap.
    #[arg(long)]
    index_budget_bytes: Option<u64>,

    /// Print volumes and the pending journal, then exit without claiming the bus name.
    #[arg(long)]
    dry_run: bool,
}

/// `$XDG_CACHE_HOME/templaros/file-index-v1.sqlite3` — the shared index's derived, disposable cache.
/// A deleted cache costs a rescan; it never costs user data (unlike `templar_state::default_path`,
/// which is authoritative).
fn default_index_db_path() -> PathBuf {
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    cache_home.join("templaros").join("file-index-v1.sqlite3")
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();

    let db_path = cli
        .state_db
        .clone()
        .unwrap_or_else(templar_state::default_path);
    let store = templar_state::Store::open(&db_path)
        .with_context(|| format!("opening state db {}", db_path.display()))?;
    tracing::info!(path = %db_path.display(), "state store ready");

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is required; refusing to index the filesystem root as a fallback")?;
    let index_db_path = cli.index_db.clone().unwrap_or_else(default_index_db_path);
    let index = Index::with_index_db(
        store.clone(),
        &index_db_path,
        templar_file_index::IndexConfig {
            // Broad type-ahead searches over a whole home are answered from memory once a search
            // has had the arrays built; until then, and after every commit, SQL answers.
            search_arrays: true,
            scan_budget: templar_file_index::ScanBudget {
                max_entries: cli.index_max_entries,
                max_bytes: cli.index_budget_bytes,
                ..templar_file_index::ScanBudget::unlimited()
            },
            ..templar_file_index::IndexConfig::default()
        },
    )
    .with_context(|| format!("opening file index {}", index_db_path.display()))?;
    let slot = index.slot().clone();
    match slot.current() {
        Some(_) => tracing::info!(path = %index_db_path.display(), "file index ready"),
        None => {
            tracing::warn!(path = %index_db_path.display(), availability = ?slot.availability(),
            "file index unavailable; browsing, operations, trash and previews still serve");
        }
    }

    let lanes = templar_filed::index_lanes::Lanes::start().context("starting index lanes")?;
    slot.set_maintenance_executor(lanes.clone());
    let service = FileService::new(
        Arc::new(SqliteJournal::new(store.clone())),
        ExecutorConfig {
            reserve_percent: cli.reserve_percent,
            index_sink: Some(templar_filed::watcher::OperationSink::over_slot(
                slot.clone(),
            )),
            ..ExecutorConfig::default()
        },
        Recents::new(store.clone()),
        index.clone(),
        lanes.clone(),
        LastDirectoryStore::new(store.clone(), home.clone()),
        cli.reserve_percent,
    );

    if cli.dry_run {
        let volumes = storage::volumes(cli.reserve_percent)?;
        println!("volumes:   {}", volumes.len());
        for volume in &volumes {
            println!(
                "  {:<28} {:>10} free of {:>10}  reserve {:>8}  {}{}",
                volume.mount_point.display(),
                human(volume.available_bytes),
                human(volume.total_bytes),
                human(volume.reserved_bytes),
                volume.fs_type,
                if volume.read_only { " (read-only)" } else { "" },
            );
        }
        let pending = service.journal.unfinished()?;
        println!("pending:   {} unfinished operation(s)", pending.len());
        for operation in &pending {
            println!(
                "  {} {} — step {} of {}",
                operation.id,
                operation.kind.summary(),
                operation.cursor,
                operation.steps.len()
            );
        }
        println!("recents:   {}", service.recents.list(None, 1000)?.len());
        match service.index.roots() {
            Ok(roots) => println!("index:     {} root(s)", roots.len()),
            Err(error) => println!("index:     unavailable ({error})"),
        }
        return Ok(());
    }

    let resumed = service.resume_unfinished()?;
    if resumed > 0 {
        tracing::info!(resumed, "resumed journalled operations");
    }

    // Say which build is running, and which generation of the state schema it speaks — the
    // second is what lets a deployment refuse a set the previous one could not read back.
    // Diagnostic only: a stamp that cannot be written must never keep templar-filed from starting.
    if let Err(error) = templar_build::write_stamp_with(
        "templar-filed",
        [("session".to_owned(), templar_state::schema_version())]
            .into_iter()
            .collect(),
    ) {
        tracing::warn!(%error, "could not write the build stamp");
    }
    let connection = connection::Builder::session()?
        .serve_at(
            names::ROOT_PATH,
            VolumesApi {
                reserve_percent: cli.reserve_percent,
            },
        )?
        .serve_at(
            names::ROOT_PATH,
            BrowseApi {
                reserve_percent: cli.reserve_percent,
                bookmarks: templar_filed::bookmarks::Bookmarks(store.clone()),
            },
        )?
        .serve_at(names::ROOT_PATH, PreviewsApi)?
        .serve_at(names::ROOT_PATH, OperationsApi(service.clone()))?
        .serve_at(names::ROOT_PATH, RecentsApi(service.clone()))?
        .serve_at(names::ROOT_PATH, SearchApi(service.clone()))?
        .serve_at(names::ROOT_PATH, IndexApi(service.clone()))?
        .serve_at(names::ROOT_PATH, TrashApi::new(service.clone()))?
        .serve_at(names::ROOT_PATH, ActionsApi)?
        .build()
        .await
        .context("connecting and hosting the file service interfaces")?;
    service.attach(connection.clone());

    // Make the change signals real. `Index1Signals` has existed since F1 and was wired into
    // nothing, so `GenerationAdvanced` and `RootStateChanged` fired nowhere: every "notify the
    // consumer" step downstream was a no-op. It must be wired *before* the watcher starts, or the
    // watcher's first committed generation goes unannounced — and it must carry this runtime's
    // handle, because `ChangeSink` is synchronous and the watcher calls it from a plain thread
    // where `tokio::spawn` would panic.
    let emitter = connection
        .object_server()
        .interface::<_, IndexApi>(names::ROOT_PATH)
        .await
        .context("looking up the hosted Index1 object")?
        .signal_emitter()
        .clone();
    let watcher_slot: Arc<std::sync::Mutex<Option<templar_filed::watcher::WatcherHandle>>> =
        Arc::default();
    let bring_up = {
        let index = index.clone();
        let index_roots = cli.index_roots.clone();
        let home = home.clone();
        let watcher_slot = Arc::clone(&watcher_slot);
        let runtime = tokio::runtime::Handle::current();
        let checkpoint = db_path.with_file_name("delta-checkpoint.json");
        move |core: &templar_file_index::Index| -> Result<()> {
            // Persisted choices survive cache deletion and an intentionally empty root list
            // survives restart. Registration is cheap; only the watcher walks.
            index.restore_configured_roots(std::slice::from_ref(&home))?;
            for path in &index_roots {
                let native = templar_file_index::NativePath::from_path(path);
                let existing = index.configured_policy(&native)?;
                let expected = existing
                    .as_ref()
                    .map_or_else(|| "absent".to_owned(), templar_file_index::RootPolicy::hash);
                index.configure_root(&native, existing.unwrap_or_default(), &expected)?;
            }
            let signals = Index1Signals::new(emitter.clone(), runtime.clone());
            core.set_change_sink(Arc::new(signals.clone()));
            // Arming watches, walking a never-scanned root and repairing a stale one all run on
            // the watcher's own thread: no client ever waits on a scan, and a root that already
            // has a committed generation is served from it immediately (the warm-start policy).
            let watcher_plan = classify_roots(core)?;
            // Default downtime acceleration on supported btrfs roots with a privileged checkpoint
            // authority. The pointer belongs beside authoritative session state.
            let delta = Some(templar_filed::watcher::DeltaSetup {
                handle: runtime.clone(),
                state_path: checkpoint.clone(),
            });
            let handle = templar_filed::watcher::spawn(core.clone(), watcher_plan, delta)
                .context("starting the index watcher")?;
            observer::register(handle.control());
            *watcher_slot.lock().expect("the watcher handle lock") = Some(handle);
            // Consumers that saw an unavailable cache resubscribe on this.
            templar_file_index::ChangeSink::generation_advanced(&signals, core.generation()?, &[]);
            Ok(())
        }
    };
    match slot.current() {
        Some(core) => bring_up(&core)?,
        None => slot.on_revival(move |core| {
            if let Err(error) = bring_up(core) {
                tracing::error!(%error, "the file index reopened but could not be brought up");
            }
        }),
    }
    let departures = watch_for_departing_consumers(&connection, Arc::clone(&slot), lanes).await?;
    connection
        .request_name(names::BUS_NAME)
        .await
        .context("claiming org.templaros.Files after observation readiness")?;

    tracing::info!(name = names::BUS_NAME, "templar-filed ready");
    departures
        .await
        .context("joining the index consumer-loss bridge")?;
    anyhow::bail!("index consumer-loss subscription ended; refusing unobservable lifetime pins");
}

/// A client that held the index open has gone.
///
/// Explicit observations have no renewal timer. Install the unique-name-loss subscription
/// before claiming the public name, and terminate if it ends rather than keeping immortal pins.
/// The departed caller's cursors go with it, so they stop pinning tombstones at once instead of
/// at their idle expiry.
async fn watch_for_departing_consumers(
    connection: &zbus::Connection,
    slot: Arc<templar_filed::index_slot::IndexSlot>,
    lanes: Arc<templar_filed::index_lanes::Lanes>,
) -> Result<tokio::task::JoinHandle<()>> {
    use futures_util::StreamExt as _;
    let proxy = zbus::fdo::DBusProxy::new(connection)
        .await
        .context("connecting the consumer-loss bridge")?;
    let mut changes = proxy
        .receive_name_owner_changed()
        .await
        .context("subscribing to consumer name loss")?;
    Ok(tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.name().starts_with(':') && args.new_owner().is_none() {
                observer::forget_consumer(args.name());
                lanes.cancel_caller(args.name());
                if let Some(core) = slot.current() {
                    core.release_owner(args.name());
                }
            }
        }
    }))
}

/// What the watcher must serve immediately, scan for the first time, or repair in the
/// background. A root with a committed generation is always `serve` — the watcher itself demotes
/// it to a downtime repair internally (`watcher.rs`); this function never rescans anything
/// synchronously.
fn classify_roots(
    core: &templar_file_index::Index,
) -> Result<templar_filed::watcher::WatcherRoots> {
    let mut plan = templar_filed::watcher::WatcherRoots::default();
    for status in core.roots()? {
        match status.state {
            templar_file_index::RootState::Empty => plan.scan.push(status),
            templar_file_index::RootState::Current | templar_file_index::RootState::Building => {
                plan.serve.push(status);
            }
            _ => plan.reconcile.push(status),
        }
    }
    Ok(plan)
}

fn human(bytes: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_with_a_committed_generation_is_served_and_an_empty_one_is_scheduled_to_scan() {
        let dir = tempfile::tempdir().unwrap();
        let core = templar_file_index::Index::open(
            &dir.path().join("file-index-v1.sqlite3"),
            templar_file_index::IndexConfig::default(),
        )
        .unwrap();

        let scanned = dir.path().join("home");
        std::fs::create_dir_all(&scanned).unwrap();
        let root = core
            .add_root(
                &templar_file_index::NativePath::from_path(&scanned),
                templar_file_index::RootPolicy::default_home(),
            )
            .unwrap()
            .root;
        core.scan_root(root, templar_file_index::ScanOptions::default())
            .unwrap();

        let never_scanned = dir.path().join("volume");
        std::fs::create_dir_all(&never_scanned).unwrap();
        core.add_root(
            &templar_file_index::NativePath::from_path(&never_scanned),
            templar_file_index::RootPolicy::default_home(),
        )
        .unwrap();

        let plan = classify_roots(&core).unwrap();
        assert_eq!(plan.serve.len(), 1, "{plan:?}");
        assert_eq!(plan.serve[0].path.to_path(), scanned);
        assert_eq!(plan.scan.len(), 1, "{plan:?}");
        assert_eq!(plan.scan[0].path.to_path(), never_scanned);
        assert!(plan.reconcile.is_empty(), "{plan:?}");
    }
}
