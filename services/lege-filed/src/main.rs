//! `lege-filed` — the file service daemon (`org.legeos.Files`).
//!
//! Starts by resuming whatever the journal shows unfinished: that is the difference between a
//! file manager and a file *service*. A copy that was running when the machine lost power picks
//! up where it stopped, rather than leaving the user to work out what happened to it.

use anyhow::{Context, Result};
use clap::Parser;
use lege_filed::{
    ExecutorConfig, SqliteJournal,
    dbus::{
        ActionsApi, BrowseApi, FileService, OperationsApi, PreviewsApi, RecentsApi, SearchApi,
        TrashApi, VolumesApi,
    },
    index_host::IndexApi,
    lastdir::LastDirectoryStore,
    recents::Recents,
    search::Index,
    storage,
};
use lege_types::dbus::files as names;
use std::{future::pending, path::PathBuf, sync::Arc};
use zbus::connection;

/// LegeOS file service.
#[derive(Debug, Parser)]
#[command(name = "lege-filed", version, about)]
struct Cli {
    /// Root-volume reserve as a percentage of its size. Zero disables the reserve entirely,
    /// which is only sensible in a test.
    #[arg(long, default_value_t = 5)]
    reserve_percent: u64,

    /// State database path (default: `$XDG_STATE_HOME/legeos/session.db`).
    #[arg(long)]
    state_db: Option<PathBuf>,

    /// Directory to index at start (repeatable). Defaults to the user's home directory.
    #[arg(long = "index-root")]
    index_roots: Vec<PathBuf>,

    /// The shared index's cache database (default: `$XDG_CACHE_HOME/legeos/file-index-v1.sqlite3`).
    #[arg(long)]
    index_db: Option<PathBuf>,

    /// Entry budget for a scan; unset means uncapped. A budget trip commits `complete = false`
    /// and leaves the root `Stale` rather than truncating silently.
    #[arg(long)]
    index_max_entries: Option<u64>,

    /// Reserved for a future byte-budgeted scan; accepted now so the CLI surface does not have to
    /// change again when it is wired up.
    #[arg(long)]
    index_budget_bytes: Option<u64>,

    /// Print volumes and the pending journal, then exit without claiming the bus name.
    #[arg(long)]
    dry_run: bool,
}

/// `$XDG_CACHE_HOME/legeos/file-index-v1.sqlite3` — the shared index's derived, disposable cache.
/// A deleted cache costs a rescan; it never costs user data (unlike `lege_state::default_path`,
/// which is authoritative).
fn default_index_db_path() -> PathBuf {
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    cache_home.join("legeos").join("file-index-v1.sqlite3")
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();

    let db_path = cli
        .state_db
        .clone()
        .unwrap_or_else(lege_state::default_path);
    let store = lege_state::Store::open(&db_path)
        .with_context(|| format!("opening state db {}", db_path.display()))?;
    tracing::info!(path = %db_path.display(), "state store ready");

    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let index_db_path = cli.index_db.clone().unwrap_or_else(default_index_db_path);
    let mut index = Index::with_index_db(
        store.clone(),
        &index_db_path,
        lege_file_index::IndexConfig::default(),
    )
    .with_context(|| format!("opening file index {}", index_db_path.display()))?;
    if let Some(max_entries) = cli.index_max_entries {
        index.max_entries = max_entries as usize;
    }
    // `--index-budget-bytes` is accepted but not yet wired to a byte-budgeted scan; recorded so
    // the flag surface does not have to change again once it is.
    if let Some(budget_bytes) = cli.index_budget_bytes {
        tracing::debug!(budget_bytes, "a byte scan budget was requested but is not yet enforced");
    }
    tracing::info!(path = %index_db_path.display(), "file index ready");

    // Registering is cheap (an insert, not a walk); the roots that need indexing are indexed by
    // the watcher after the bus name is claimed, never here.
    let index_roots: Vec<PathBuf> = if cli.index_roots.is_empty() {
        vec![home.clone()]
    } else {
        cli.index_roots.clone()
    };
    let core = index.core().clone();
    for root in &index_roots {
        if let Err(error) =
            core.add_root(&lege_file_index::NativePath::from_path(root), lege_file_index::RootPolicy::default_home())
        {
            tracing::warn!(root = %root.display(), %error, "could not register index root");
        }
    }

    let service = FileService::new(
        Arc::new(SqliteJournal::new(store.clone())),
        ExecutorConfig {
            reserve_percent: cli.reserve_percent,
            index_sink: Some(lege_filed::watcher::OperationSink::shared(core.clone())),
            ..ExecutorConfig::default()
        },
        Recents::new(store.clone()),
        index,
        LastDirectoryStore::new(store, home.clone()),
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
        println!("index:     {} root(s)", service.index.roots()?.len());
        return Ok(());
    }

    let resumed = service.resume_unfinished()?;
    if resumed > 0 {
        tracing::info!(resumed, "resumed journalled operations");
    }

    // Say which build is running, and which generation of the state schema it speaks — the
    // second is what lets a deployment refuse a set the previous one could not read back.
    // Diagnostic only: a stamp that cannot be written must never keep lege-filed from starting.
    if let Err(error) = lege_build::write_stamp_with(
        "lege-filed",
        [("session".to_owned(), lege_state::schema_version())]
            .into_iter()
            .collect(),
    ) {
        tracing::warn!(%error, "could not write the build stamp");
    }
    let connection = connection::Builder::session()?
        .name(names::BUS_NAME)?
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
        .context("claiming org.legeos.Files")?;
    service.attach(connection.clone());

    // Arming watches, walking a never-scanned root and repairing a stale one all happen after the
    // bus name is claimed and all run on the watcher's own thread: no client ever waits on a scan,
    // and a root that already has a committed generation is served from it immediately (the
    // warm-start policy) rather than rescanned synchronously.
    let watcher_plan = classify_roots(&core)?;
    let _watcher_handle = lege_filed::watcher::spawn(core, watcher_plan)
        .context("starting the index watcher")?;

    tracing::info!(name = names::BUS_NAME, "lege-filed ready");
    pending::<()>().await;
    Ok(())
}

/// What the watcher must serve immediately, scan for the first time, or repair in the
/// background. A root with a committed generation is always `serve` — the watcher itself demotes
/// it to a downtime repair internally (`watcher.rs`); this function never rescans anything
/// synchronously.
fn classify_roots(core: &lege_file_index::Index) -> Result<lege_filed::watcher::WatcherRoots> {
    let mut plan = lege_filed::watcher::WatcherRoots::default();
    for status in core.roots()? {
        match status.state {
            lege_file_index::RootState::Empty => plan.scan.push(status),
            lege_file_index::RootState::Current | lege_file_index::RootState::Building => {
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
        let core = lege_file_index::Index::open(
            &dir.path().join("file-index-v1.sqlite3"),
            lege_file_index::IndexConfig::default(),
        )
        .unwrap();

        let scanned = dir.path().join("home");
        std::fs::create_dir_all(&scanned).unwrap();
        let root = core
            .add_root(
                &lege_file_index::NativePath::from_path(&scanned),
                lege_file_index::RootPolicy::default_home(),
            )
            .unwrap()
            .root;
        core.scan_root(root, lege_file_index::ScanOptions::default())
            .unwrap();

        let never_scanned = dir.path().join("volume");
        std::fs::create_dir_all(&never_scanned).unwrap();
        core.add_root(
            &lege_file_index::NativePath::from_path(&never_scanned),
            lege_file_index::RootPolicy::default_home(),
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
