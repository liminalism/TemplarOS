//! `lege-sessiond` — the one user-session policy daemon (ADR-0001).
//!
//! Bus name `org.legeos.Session`; one object path per domain; interfaces versioned per domain.
//! `Type=notify`: readiness and the watchdog ping are reported through `sd_notify` once every
//! interface is served and the bus name is claimed.

use anyhow::{Context, Result};
use clap::Parser;
use lege_appd::{
    Registry,
    dbus::{IdentityApi, LauncherApi},
    desktop_entry::{ExecResolver, PathExecResolver},
    launcher::LaunchBackend,
    manifests,
};
use lege_associationd::{
    AssociationEngine,
    dbus::AssociationApi,
    detector::{FallbackDetector, MimeDetector, SharedMimeInfoDetector, XdgMimeDetector},
};
use lege_session_core::{
    history::{HistoryStore, SqliteHistory},
    providers::ProviderRegistry,
    settings::{SettingsSchema, SettingsValues, baseline_schema},
};
use lege_state::Store;
use lege_types::{AppCatalog, ProviderCapability, ProviderRole, dbus::session as names};
use parking_lot::RwLock;
use std::{collections::BTreeSet, future::pending, path::PathBuf, sync::Arc, time::Duration};

mod apis;
mod background;
mod checkpoint;
mod reconcile;

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

fn xdg_home(env_var: &str, fallback_under_home: &str) -> Option<PathBuf> {
    std::env::var_os(env_var).map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback_under_home))
    })
}

fn mimeapps_export_path() -> PathBuf {
    xdg_home("XDG_DATA_HOME", ".local/share")
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
        .join("applications/mimeapps.list")
}

/// Read locations for an existing `mimeapps.list`, in the order the mimeapps spec prefers.
fn mimeapps_read_locations() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(config_home) = xdg_home("XDG_CONFIG_HOME", ".config") {
        candidates.push(config_home.join("mimeapps.list"));
    }
    if let Some(data_home) = xdg_home("XDG_DATA_HOME", ".local/share") {
        candidates.push(data_home.join("applications/mimeapps.list"));
    }
    candidates.push(PathBuf::from("/etc/xdg/mimeapps.list"));
    candidates
}

/// Import an existing `mimeapps.list` once, then never again (`docs/architecture/associations.md`).
fn import_mimeapps_once(store: &Store, catalog: &Arc<dyn AppCatalog>) {
    match lege_associationd::store::mimeapps_already_imported(store) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "failed to check the mimeapps import marker; skipping import");
            return;
        }
    }
    let resolved = mimeapps_read_locations()
        .into_iter()
        .find_map(|path| std::fs::read_to_string(&path).ok().map(|text| (path, text)))
        .map(|(path, text)| {
            let defaults = lege_associationd::import::parse_default_applications(&text);
            let resolved = lege_associationd::import::resolve_against_catalog(&defaults, catalog.as_ref());
            tracing::info!(path = %path.display(), count = resolved.len(), "importing mimeapps.list");
            resolved
        })
        .unwrap_or_default();
    if let Err(error) = store
        .transaction(|tx| lege_associationd::store::mark_mimeapps_imported_within(tx, &resolved))
    {
        tracing::warn!(%error, "failed to record the mimeapps import");
    }
}

/// sessiond declares itself a `notification-server` candidate at distribution priority so a
/// fresh install has a working notification server with no user action, while a third-party
/// daemon can still outrank or replace it.
fn ensure_sessiond_provider_declaration(store: &Store, providers: &Arc<RwLock<ProviderRegistry>>) {
    let app_id = background::notification_provider::sessiond_app_id();
    let already = providers.read().declarations.iter().any(|declaration| {
        declaration.app_id == app_id
            && declaration.capability.role == ProviderRole::NotificationServer
    });
    if already {
        return;
    }
    let capability = ProviderCapability {
        role: ProviderRole::NotificationServer,
        priority: 100,
    };
    if let Err(error) = store
        .transaction(|tx| lege_session_core::providers::declare_within(tx, &app_id, &capability))
    {
        tracing::warn!(%error, "failed to persist sessiond's notification-server declaration");
        return;
    }
    providers.write().declare(app_id, capability);
}

/// Everything the daemon needs, loaded and cross-linked once. Kept as one struct so `main`
/// reads as a short sequence of named steps instead of two dozen `let` bindings.
struct Services {
    store: Store,
    registry: Arc<Registry>,
    native_count: usize,
    imported_count: usize,
    resolver: Arc<dyn ExecResolver>,
    catalog: Arc<dyn AppCatalog>,
    engine: Arc<RwLock<AssociationEngine>>,
    detector: Arc<dyn MimeDetector>,
    providers: Arc<RwLock<ProviderRegistry>>,
    schema: Arc<RwLock<SettingsSchema>>,
    settings_values: Arc<RwLock<SettingsValues>>,
    history: Arc<SqliteHistory>,
    open_projects: Arc<RwLock<BTreeSet<String>>>,
    checkpoint_transport: Arc<dyn checkpoint::CheckpointTransport>,
}

fn load_services(cli: &Cli) -> Result<Services> {
    let db_path = cli
        .state_db
        .clone()
        .unwrap_or_else(lege_state::default_path);
    let store =
        Store::open(&db_path).with_context(|| format!("opening state db {}", db_path.display()))?;
    tracing::info!(path = %db_path.display(), "state store ready");

    // --- appd: native manifests, then desktop-entry import ------------------------------------
    let registry = Arc::new(Registry::default());
    let mut manifest_dirs = vec![
        PathBuf::from(manifests::SYSTEM_DIR),
        PathBuf::from(manifests::ETC_DIR),
    ];
    if let Some(data_home) = xdg_home("XDG_DATA_HOME", ".local/share") {
        manifest_dirs.push(data_home.join("legeos/apps"));
    }
    manifest_dirs.extend(cli.manifest_dirs.iter().cloned());
    for dir in &manifest_dirs {
        for manifest in manifests::load_dir(dir)
            .with_context(|| format!("loading manifests from {}", dir.display()))?
        {
            tracing::debug!(app = %manifest.id, dir = %dir.display(), "manifest loaded");
            registry.insert_manifest(manifest);
        }
    }
    let native_count = registry.manifests().len();

    let resolver: Arc<dyn ExecResolver> = Arc::new(PathExecResolver::from_env());
    let imported_count = background::desktop_import::import(
        &registry,
        &store,
        &background::desktop_import::application_dirs(),
        resolver.as_ref(),
    )
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "desktop-entry import failed");
        0
    });
    tracing::info!(
        native = native_count,
        imported = imported_count,
        "application registry ready"
    );

    // --- associationd (in-process: the registry *is* the catalog) ----------------------------
    let catalog: Arc<dyn AppCatalog> = registry.clone();
    import_mimeapps_once(&store, &catalog);
    let assoc_db =
        lege_associationd::store::load_db(&store).context("loading the association database")?;
    let engine = Arc::new(RwLock::new(AssociationEngine::new(
        assoc_db,
        catalog.clone(),
    )));
    {
        let removed = engine.write().prune_stale();
        if !removed.is_empty()
            && let Err(error) =
                store.transaction(|tx| lege_associationd::store::apply_prune_within(tx, &removed))
        {
            tracing::warn!(%error, "failed to persist pruned stale association defaults");
        }
    }
    let shared_mime = SharedMimeInfoDetector::load(&PathBuf::from("/usr/share/mime"))
        .context("loading /usr/share/mime/globs2")?;
    let detector: Arc<dyn MimeDetector> =
        Arc::new(FallbackDetector::new(shared_mime, XdgMimeDetector));

    // --- session-core domains ------------------------------------------------------------------
    let providers = Arc::new(RwLock::new(
        lege_session_core::providers::load(&store).context("loading provider registry")?,
    ));
    ensure_sessiond_provider_declaration(&store, &providers);
    let schema = Arc::new(RwLock::new(baseline_schema()));
    let settings_values = Arc::new(RwLock::new(
        lege_session_core::settings::load_values(&store).context("loading settings values")?,
    ));
    let history = Arc::new(SqliteHistory::new(store.clone()));

    Ok(Services {
        store,
        registry,
        native_count,
        imported_count,
        resolver,
        catalog,
        engine,
        detector,
        providers,
        schema,
        settings_values,
        history,
        open_projects: Arc::new(RwLock::new(BTreeSet::new())),
        checkpoint_transport: Arc::new(checkpoint::NullCheckpointTransport),
    })
}

fn print_dry_run_summary(services: &Services) {
    let db = services.engine.read();
    println!(
        "manifests:    {} native, {} imported from .desktop files",
        services.native_count, services.imported_count
    );
    println!("providers:    {} roles", ProviderRole::ALL.len());
    for (role, resolution) in services.providers.read().report() {
        println!("  {role}: {resolution:?}");
    }
    println!("settings:     {} keys", services.schema.read().specs.len());
    println!(
        "associations: {} mime defaults, {} extension rules, {} project scopes",
        db.database().mime_defaults.len(),
        db.database().extension_defaults.len(),
        db.database().project_mime_defaults.len()
    );
    let project_count =
        lege_session_core::projects::load_all(&services.store).map_or(0, |projects| projects.len());
    println!("projects:     {project_count} stored");
    let history_count = services
        .history
        .recent(usize::MAX)
        .map_or(0, |events| events.len());
    println!("history:      {history_count} events");
}

/// Serve every `org.legeos.Session` interface, in-process, before the bus name is claimed.
async fn serve_interfaces(
    connection: &zbus::Connection,
    system_connection: Option<zbus::Connection>,
    services: &Services,
    backend: LaunchBackend,
) -> Result<()> {
    let object_server = connection.object_server();
    object_server
        .at(
            names::APP_IDENTITY_PATH,
            IdentityApi::new(services.registry.clone()),
        )
        .await
        .context("serving AppIdentity1")?;
    object_server
        .at(
            names::LAUNCHER_PATH,
            LauncherApi::new(services.registry.clone(), backend, connection.clone()),
        )
        .await
        .context("serving Launcher1")?;
    object_server
        .at(
            names::ASSOCIATIONS_PATH,
            AssociationApi::new(
                services.engine.clone(),
                services.detector.clone(),
                services.store.clone(),
                services.catalog.clone(),
                mimeapps_export_path(),
            ),
        )
        .await
        .context("serving Associations1")?;
    object_server
        .at(
            names::PROVIDERS_PATH,
            apis::providers::ProvidersApi::new(services.providers.clone(), services.store.clone()),
        )
        .await
        .context("serving Providers1")?;
    object_server
        .at(
            names::HISTORY_PATH,
            apis::history::HistoryApi::new(
                services.history.clone(),
                services.store.clone(),
                services.engine.clone(),
                services.settings_values.clone(),
                services.providers.clone(),
            ),
        )
        .await
        .context("serving History1")?;
    object_server
        .at(
            names::SETTINGS_PATH,
            apis::settings::SettingsApi::new(
                services.schema.clone(),
                services.settings_values.clone(),
                services.store.clone(),
            ),
        )
        .await
        .context("serving Settings1")?;
    object_server
        .at(
            names::PROJECTS_PATH,
            apis::projects::ProjectsApi::new(
                services.store.clone(),
                services.registry.clone(),
                backend,
                connection.clone(),
                services.checkpoint_transport.clone(),
                services.open_projects.clone(),
                services.history.clone(),
            ),
        )
        .await
        .context("serving Projects1")?;
    object_server
        .at(
            names::POWER_PATH,
            apis::power::PowerApi::new(services.store.clone()),
        )
        .await
        .context("serving Power1")?;
    object_server
        .at(
            names::LOCATION_PATH,
            apis::location::LocationApi::new(
                services.store.clone(),
                system_connection,
                services.history.clone(),
            ),
        )
        .await
        .context("serving Location1")?;
    Ok(())
}

/// Report readiness and, if `WatchdogSec` is set, keep pinging the service manager.
fn start_systemd_notify() {
    if let Err(error) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(%error, "sd_notify READY failed (not running under systemd?)");
    }
    if let Some(watchdog_interval) = sd_notify::watchdog_enabled() {
        tokio::spawn(async move {
            let ping_every = watchdog_interval / 2;
            loop {
                tokio::time::sleep(ping_every).await;
                let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
            }
        });
    }
}

fn spawn_background_tasks(
    connection: &zbus::Connection,
    services: &Services,
    backend: LaunchBackend,
) {
    tokio::spawn(background::autostart::run(
        services.registry.clone(),
        services.providers.clone(),
        services.store.clone(),
        services.history.clone(),
        backend,
        connection.clone(),
        services.resolver.clone(),
        background::autostart::autostart_dirs(),
    ));
    tokio::spawn(background::checkpoint_relay::run(
        connection.clone(),
        services.registry.clone(),
        services.checkpoint_transport.clone(),
        services.history.clone(),
    ));
    tokio::spawn(background::crash_ingest::run(
        services.store.clone(),
        Duration::from_secs(30),
    ));
    tokio::spawn(background::notification_provider::run(
        connection.clone(),
        services.providers.clone(),
        services.store.clone(),
        services.history.clone(),
        services.registry.clone(),
        Duration::from_secs(5),
    ));
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let services = load_services(&cli)?;

    if cli.dry_run {
        print_dry_run_summary(&services);
        return Ok(());
    }

    let connection = zbus::connection::Builder::session()?
        .build()
        .await
        .context("connecting to the session bus")?;
    // Best-effort: `Location1` retries lazily (see `apis::location`), so a system bus that is
    // briefly unreachable at session startup must not take the rest of the daemon down with it.
    let system_connection = match zbus::connection::Builder::system() {
        Ok(builder) => match builder.build().await {
            Ok(connection) => Some(connection),
            Err(error) => {
                tracing::warn!(%error, "could not connect to the system bus yet; Location1 will retry lazily");
                None
            }
        },
        Err(error) => {
            tracing::warn!(%error, "failed to prepare a system bus connection; Location1 will retry lazily");
            None
        }
    };
    let backend = LaunchBackend::detect(Some(&connection)).await;
    tracing::info!(?backend, "launch backend selected");

    reconcile::reconcile_instances(&connection, &services.registry, &services.store).await;
    serve_interfaces(&connection, system_connection, &services, backend).await?;

    connection
        .request_name(names::BUS_NAME)
        .await
        .context("claiming org.legeos.Session")?;
    tracing::info!(name = names::BUS_NAME, "lege-sessiond ready");

    start_systemd_notify();
    spawn_background_tasks(&connection, &services, backend);

    pending::<()>().await;
    Ok(())
}
