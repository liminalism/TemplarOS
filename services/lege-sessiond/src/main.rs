//! `lege-sessiond` — the one user-session policy daemon (ADR-0001).
//!
//! Bus name `org.legeos.Session`; one object path per domain; interfaces versioned per domain.

use anyhow::{Context, Result};
use clap::Parser;
use lege_appd::{
    Registry,
    dbus::{IdentityApi, LauncherApi},
    manifests,
};
use lege_associationd::{
    AssociationDb, AssociationEngine,
    dbus::AssociationApi,
    detector::{MimeDetector, XdgMimeDetector},
};
use lege_types::{AppCatalog, dbus::session as names};
use parking_lot::RwLock;
use std::{future::pending, path::PathBuf, sync::Arc};
use zbus::connection;

mod domains;

/// LegeOS session daemon.
#[derive(Debug, Parser)]
#[command(name = "lege-sessiond", version, about)]
struct Cli {
    /// Additional manifest directory (repeatable). System and user directories are always read.
    #[arg(long = "manifest-dir")]
    manifest_dirs: Vec<PathBuf>,

    /// State database path (default: $XDG_STATE_HOME/legeos/session.db).
    #[arg(long)]
    state_db: Option<PathBuf>,

    /// Do not claim the bus name; load everything, print a summary and exit.
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

    // --- state -------------------------------------------------------------------------------
    let db_path = cli
        .state_db
        .clone()
        .unwrap_or_else(lege_state::default_path);
    let _state = lege_state::open(&db_path)
        .with_context(|| format!("opening state db {}", db_path.display()))?;
    tracing::info!(path = %db_path.display(), "state store ready");

    // --- appd --------------------------------------------------------------------------------
    let registry = Arc::new(Registry::default());
    let mut dirs = vec![
        PathBuf::from(manifests::SYSTEM_DIR),
        PathBuf::from(manifests::ETC_DIR),
    ];
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    {
        dirs.push(data_home.join("legeos/apps"));
    }
    dirs.extend(cli.manifest_dirs.iter().cloned());
    for dir in &dirs {
        for manifest in manifests::load_dir(dir)
            .with_context(|| format!("loading manifests from {}", dir.display()))?
        {
            tracing::debug!(app = %manifest.id, dir = %dir.display(), "manifest loaded");
            registry.insert_manifest(manifest);
        }
    }
    tracing::info!(
        apps = registry.manifests().len(),
        "application registry ready"
    );

    // --- associationd (in-process: the registry *is* the catalog) ----------------------------
    let catalog: Arc<dyn AppCatalog> = registry.clone();
    let engine = Arc::new(RwLock::new(AssociationEngine::new(
        AssociationDb::default(),
        catalog,
    )));
    let detector: Arc<dyn MimeDetector> = Arc::new(XdgMimeDetector);

    // --- session-core domains ---------------------------------------------------------------
    let domains = domains::Domains::new();

    if cli.dry_run {
        println!("manifests: {}", registry.manifests().len());
        println!("providers: {} roles", lege_types::ProviderRole::ALL.len());
        println!("settings:  {} keys", domains.settings.read().specs.len());
        return Ok(());
    }

    let _connection = connection::Builder::session()?
        .name(names::BUS_NAME)?
        .serve_at(names::APP_IDENTITY_PATH, IdentityApi::new(registry.clone()))?
        .serve_at(names::LAUNCHER_PATH, LauncherApi::new(registry.clone()))?
        .serve_at(
            names::ASSOCIATIONS_PATH,
            AssociationApi::new(engine, detector),
        )?
        .serve_at(
            names::PROVIDERS_PATH,
            domains::ProvidersApi::new(domains.providers.clone()),
        )?
        .serve_at(
            names::HISTORY_PATH,
            domains::HistoryApi::new(domains.history.clone()),
        )?
        .serve_at(
            names::SETTINGS_PATH,
            domains::SettingsApi::new(domains.settings.clone()),
        )?
        .build()
        .await
        .context("claiming org.legeos.Session")?;

    tracing::info!(name = names::BUS_NAME, "lege-sessiond ready");
    pending::<()>().await;
    Ok(())
}
