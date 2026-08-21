//! `lege-filed` — the file service daemon (`org.legeos.Files`).
//!
//! Starts by resuming whatever the journal shows unfinished: that is the difference between a
//! file manager and a file *service*. A copy that was running when the machine lost power picks
//! up where it stopped, rather than leaving the user to work out what happened to it.

use anyhow::{Context, Result};
use clap::Parser;
use lege_filed::{
    ExecutorConfig, SqliteJournal,
    dbus::{ActionsApi, FileService, OperationsApi, RecentsApi, SearchApi, VolumesApi},
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

    /// Print volumes and the pending journal, then exit without claiming the bus name.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
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
    let service = FileService::new(
        Arc::new(SqliteJournal::new(store.clone())),
        ExecutorConfig {
            reserve_percent: cli.reserve_percent,
            ..ExecutorConfig::default()
        },
        Recents::new(store.clone()),
        Index::new(store.clone()),
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

    let connection = connection::Builder::session()?
        .name(names::BUS_NAME)?
        .serve_at(
            names::ROOT_PATH,
            VolumesApi {
                reserve_percent: cli.reserve_percent,
            },
        )?
        .serve_at(names::ROOT_PATH, OperationsApi(service.clone()))?
        .serve_at(names::ROOT_PATH, RecentsApi(service.clone()))?
        .serve_at(names::ROOT_PATH, SearchApi(service.clone()))?
        .serve_at(names::ROOT_PATH, ActionsApi)?
        .build()
        .await
        .context("claiming org.legeos.Files")?;
    service.attach(connection.clone());

    // Indexing walks the home directory, so it happens after the bus name is claimed: clients
    // must not wait on it.
    let roots = if cli.index_roots.is_empty() {
        vec![home]
    } else {
        cli.index_roots.clone()
    };
    let indexer = service.clone();
    tokio::task::spawn_blocking(move || {
        for root in roots {
            match indexer.index.index_root(&root) {
                Ok(count) => tracing::info!(root = %root.display(), entries = count, "indexed"),
                Err(error) => {
                    tracing::warn!(root = %root.display(), %error, "could not index");
                }
            }
        }
    });

    tracing::info!(name = names::BUS_NAME, "lege-filed ready");
    pending::<()>().await;
    Ok(())
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
