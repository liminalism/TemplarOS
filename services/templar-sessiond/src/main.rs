//! `templar-sessiond` — the one user-session policy daemon (ADR-0001).
//!
//! Bus name `org.templaros.Session`; one object path per domain; interfaces versioned per domain.
//! `Type=notify`: readiness and the watchdog ping are reported through `sd_notify` once every
//! interface is served and the bus name is claimed.

use anyhow::{Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};
use templar_appd::{
    Registry,
    dbus::{IdentityApi, LauncherApi},
    desktop_entry::{ExecResolver, PathExecResolver},
    launcher::LaunchBackend,
    manifests,
};
use templar_associationd::{
    AssociationEngine,
    dbus::AssociationApi,
    detector::{FallbackDetector, MimeDetector, SharedMimeInfoDetector, XdgMimeDetector},
};
use templar_session_core::{
    history::{HistoryStore, SqliteHistory},
    providers::ProviderRegistry,
    settings::{SettingsSchema, SettingsValues, baseline_schema},
};
use templar_state::Store;
use templar_types::{AppCatalog, ProviderCapability, ProviderRole, dbus::session as names};

mod apis;
mod background;
mod checkpoint;
mod command;
mod reconcile;
mod session_collector;

/// TemplarOS session daemon.
#[derive(Debug, Parser)]
#[command(name = "templar-sessiond", version, about)]
struct Cli {
    /// Additional manifest directory (repeatable). System and user directories are always read.
    #[arg(long = "manifest-dir")]
    manifest_dirs: Vec<PathBuf>,

    /// Override the standard desktop-entry search path (repeatable). Intended for hermetic tests
    /// and image validation; when omitted the normal system and user application directories are
    /// scanned.
    #[arg(long = "application-dir")]
    application_dirs: Vec<PathBuf>,

    /// Do not read system/user manifest defaults. Use with explicit `--manifest-dir` and
    /// `--application-dir` arguments to make integration tests independent of the host install.
    #[arg(long)]
    no_default_discovery: bool,

    /// State database path (default: $XDG_STATE_HOME/templaros/session.db).
    #[arg(long)]
    state_db: Option<PathBuf>,

    /// Explicit isolated nested compositor fixture; never used by the installed session.
    #[arg(long, requires = "no_default_discovery")]
    wallpaper_control_socket: Option<PathBuf>,

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

/// Read the complete XDG preference chain, highest precedence first.
fn mimeapps_read_locations() -> Vec<PathBuf> {
    mimeapps_locations(
        xdg_home("XDG_CONFIG_HOME", ".config"),
        xdg_home("XDG_DATA_HOME", ".local/share"),
        &std::env::var_os("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into()),
        &std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
        &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
    )
}

fn mimeapps_locations(
    config_home: Option<PathBuf>,
    data_home: Option<PathBuf>,
    config_dirs: &std::ffi::OsStr,
    data_dirs: &std::ffi::OsStr,
    desktop: &str,
) -> Vec<PathBuf> {
    let desktops: Vec<_> = desktop
        .split(':')
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(str::to_ascii_lowercase)
        .collect();
    let mut candidates = Vec::new();
    let config_roots = config_home
        .into_iter()
        .chain(std::env::split_paths(config_dirs));
    let data_roots = data_home
        .into_iter()
        .chain(std::env::split_paths(data_dirs));
    for directory in config_roots
        .chain(data_roots.map(|root| root.join("applications")))
        .filter(|directory| directory.is_absolute())
    {
        for name in &desktops {
            candidates.push(directory.join(format!("{name}-mimeapps.list")));
        }
        candidates.push(directory.join("mimeapps.list"));
    }
    candidates
}

fn read_mimeapps_defaults(
    paths: impl IntoIterator<Item = PathBuf>,
) -> std::collections::BTreeMap<String, String> {
    let mut defaults = std::collections::BTreeMap::new();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for (mime, application) in
                templar_associationd::import::parse_default_applications(&text)
            {
                defaults.entry(mime).or_insert(application);
            }
        }
    }
    defaults
}

/// Import an existing `mimeapps.list` once, then never again (`docs/architecture/associations.md`).
fn import_mimeapps_once(store: &Store, catalog: &Arc<dyn AppCatalog>) {
    match templar_associationd::store::mimeapps_already_imported(store) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "failed to check the mimeapps import marker; skipping import");
            return;
        }
    }
    let defaults = read_mimeapps_defaults(mimeapps_read_locations());
    let resolved =
        templar_associationd::import::resolve_against_catalog(&defaults, catalog.as_ref());
    tracing::info!(count = resolved.len(), "importing XDG mimeapps defaults");
    if let Err(error) = store
        .transaction(|tx| templar_associationd::store::mark_mimeapps_imported_within(tx, &resolved))
    {
        tracing::warn!(%error, "failed to record the mimeapps import");
    }
}

/// Rewrite the PCManFM-Qt profile keys that cannot work on this machine — a terminal that is not
/// on `PATH`, a fallback icon theme that is not installed — to TemplarOS's defaults, leaving every
/// valid choice and every other byte alone, and give libfm-qt the `templar-terminal` group of its
/// per-user terminal list (`templar_session_core::pcmanfm`). Best-effort: a file manager with
/// blank icons is never a reason for the session not to start.
fn repair_pcmanfm_settings() {
    use templar_session_core::pcmanfm;
    if let Some(data_home) = xdg_home("XDG_DATA_HOME", ".local/share") {
        let list = pcmanfm::terminals_list_path(&data_home);
        match pcmanfm::ensure_terminal_entry(&list) {
            Ok(true) => {
                tracing::info!(path = %list.display(), "added templar-terminal to libfm-qt's terminal list")
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), path = %list.display(), "could not add templar-terminal to libfm-qt's terminal list")
            }
        }
    }
    let Some(config_home) = xdg_home("XDG_CONFIG_HOME", ".config") else {
        return;
    };
    let path = pcmanfm::settings_path(&config_home);
    match pcmanfm::repair_file(&path, &pcmanfm::Host::from_env(), None) {
        Ok(pcmanfm::Outcome::Repaired { repairs }) => {
            for repair in repairs {
                tracing::info!(
                    path = %path.display(),
                    key = repair.key,
                    before = ?repair.before,
                    after = %repair.after,
                    "repaired a PCManFM-Qt setting that could not work here"
                );
            }
        }
        Ok(pcmanfm::Outcome::NoProfile | pcmanfm::Outcome::Unchanged) => {}
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), path = %path.display(), "could not repair PCManFM-Qt settings");
        }
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
        .transaction(|tx| templar_session_core::providers::declare_within(tx, &app_id, &capability))
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
    /// The `.desktop` directories the startup import scanned. Kept so the desktop watcher can
    /// re-scan exactly the same chain — including `--application-dir` overrides — for the rest
    /// of the session.
    application_dirs: Vec<PathBuf>,
    catalog: Arc<dyn AppCatalog>,
    engine: Arc<RwLock<AssociationEngine>>,
    detector: Arc<dyn MimeDetector>,
    providers: Arc<RwLock<ProviderRegistry>>,
    schema: Arc<RwLock<SettingsSchema>>,
    /// Anonymous exact-shuffle spools share the already-writable state directory.
    wallpaper_spool_directory: PathBuf,
    settings_values: Arc<RwLock<SettingsValues>>,
    history: Arc<SqliteHistory>,
    open_projects: Arc<RwLock<BTreeSet<String>>>,
    checkpoint_transport: Arc<dyn checkpoint::CheckpointTransport>,
    /// Session-time edges — start, lock, unlock, sleep — as they happen, in this process.
    ///
    /// The bus signals on `Lifecycle1` are a *view* of this (`lifecycle::spawn_signal_relay`);
    /// the channel is the record. Anything in this daemon that has to count session time rather
    /// than wall-clock time subscribes here and pays no round trip for it.
    session_edges: apis::lifecycle::SessionEdges,
    /// One typed successful launch record, including actual broker working directory.
    launches: tokio::sync::broadcast::Sender<templar_appd::dbus::LaunchEvent>,
}

/// The `.desktop` directories the import scans: the operator's `--application-dir`
/// overrides, or the default discovery chain. Factored out of [`load_services`] so that
/// function stays within its line budget; the watcher reuses the stored answer, not this.
fn resolve_application_dirs(cli: &Cli) -> Vec<PathBuf> {
    if cli.application_dirs.is_empty() && !cli.no_default_discovery {
        background::desktop_import::application_dirs()
    } else {
        cli.application_dirs.clone()
    }
}

fn load_services(cli: &Cli) -> Result<Services> {
    let db_path = cli
        .state_db
        .clone()
        .unwrap_or_else(templar_state::default_path);
    let store =
        Store::open(&db_path).with_context(|| format!("opening state db {}", db_path.display()))?;
    tracing::info!(path = %db_path.display(), "state store ready");

    // --- appd: native manifests, then desktop-entry import ------------------------------------
    let registry = Arc::new(Registry::default());
    let mut manifest_dirs = Vec::new();
    if !cli.no_default_discovery {
        manifest_dirs.extend([
            PathBuf::from(manifests::SYSTEM_DIR),
            PathBuf::from(manifests::ETC_DIR),
        ]);
        if let Some(data_home) = xdg_home("XDG_DATA_HOME", ".local/share") {
            manifest_dirs.push(data_home.join("templaros/apps"));
        }
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
    let application_dirs = resolve_application_dirs(cli);
    let imported_count =
        background::desktop_import::import(&registry, &store, &application_dirs, resolver.as_ref())
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "desktop-entry import failed");
                0
            });
    let grouped = background::desktop_import::regroup_and_persist(&registry, &store)
        .unwrap_or_else(|error| {
            // Grouping is presentation: losing it costs one Firefox showing as two, never a
            // session.
            tracing::warn!(%error, "grouping applications into logical apps failed");
            0
        });
    tracing::info!(
        native = native_count,
        imported = imported_count,
        variants = grouped,
        "application registry ready"
    );

    // --- associationd (in-process: the registry *is* the catalog) ----------------------------
    let catalog: Arc<dyn AppCatalog> = registry.clone();
    import_mimeapps_once(&store, &catalog);
    let assoc_db =
        templar_associationd::store::load_db(&store).context("loading the association database")?;
    let engine = Arc::new(RwLock::new(AssociationEngine::new(
        assoc_db,
        catalog.clone(),
    )));
    {
        let removed = engine.write().prune_stale();
        if !removed.is_empty()
            && let Err(error) = store
                .transaction(|tx| templar_associationd::store::apply_prune_within(tx, &removed))
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
        templar_session_core::providers::load(&store).context("loading provider registry")?,
    ));
    ensure_sessiond_provider_declaration(&store, &providers);
    providers.write().declare_installed(catalog.as_ref());
    let schema = Arc::new(RwLock::new(baseline_schema()));
    let settings_values = Arc::new(RwLock::new(
        templar_session_core::settings::load_values(&store).context("loading settings values")?,
    ));
    let history = Arc::new(SqliteHistory::new(store.clone()));

    Ok(Services {
        store,
        wallpaper_spool_directory: db_path
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned(),
        registry,
        native_count,
        imported_count,
        resolver,
        application_dirs,
        catalog,
        engine,
        detector,
        providers,
        schema,
        settings_values,
        history,
        open_projects: Arc::new(RwLock::new(BTreeSet::new())),
        checkpoint_transport: Arc::new(checkpoint::NullCheckpointTransport),
        session_edges: tokio::sync::broadcast::channel(apis::lifecycle::EDGE_CAPACITY).0,
        launches: tokio::sync::broadcast::channel(apis::lifecycle::EDGE_CAPACITY).0,
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
    let project_count = templar_session_core::projects::load_all(&services.store)
        .map_or(0, |projects| projects.len());
    println!("projects:     {project_count} stored");
    let history_count = services
        .history
        .recent(usize::MAX)
        .map_or(0, |events| events.len());
    println!("history:      {history_count} events");
}

/// Application-identity/launch/association/provider domains: the ones a launch or an "open
/// this file" round trip touches.
async fn serve_launch_and_association_interfaces(
    connection: &zbus::Connection,
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
    // The observer only republishes: appd stays free of any idea of what a launch *means*, and
    // whoever cares subscribes to `services.launches`.
    let launches = services.launches.clone();
    object_server
        .at(
            names::LAUNCHER_PATH,
            LauncherApi::new(services.registry.clone(), backend, connection.clone()).with_observer(
                Box::new(move |event| {
                    let _ = launches.send(event);
                }),
            ),
        )
        .await
        .context("serving Launcher1")?;
    object_server
        .at(
            "/org/templaros/Session/Workloads",
            templar_appd::workload::WorkloadsApi::new(
                services.registry.clone(),
                connection.clone(),
                connection.clone(),
                services.store.clone(),
            )
            .await?,
        )
        .await
        .context("serving Workloads1")?;
    object_server
        .at(
            apis::network::PATH,
            apis::network::NetworkApi::new(connection.clone()).await,
        )
        .await
        .context("serving Network1")?;
    object_server
        .at(
            apis::bluetooth::PATH,
            apis::bluetooth::BluetoothApi::new(connection.clone()).await,
        )
        .await
        .context("serving Bluetooth1")?;
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
            apis::providers::ProvidersApi::new(
                services.providers.clone(),
                services.store.clone(),
                services.registry.clone(),
                Arc::new(apis::providers::PeerCgroupVariant::new(
                    connection.clone(),
                    services.registry.clone(),
                )),
                services.history.clone(),
            ),
        )
        .await
        .context("serving Providers1")?;
    object_server
        .at(
            apis::background::PATH,
            apis::background::BackgroundApi::new(
                services.store.clone(),
                services.registry.clone(),
                services.providers.clone(),
            ),
        )
        .await
        .context("serving Background1")?;
    Ok(())
}

/// Volume, backlight and battery: what the hardware is doing.
///
/// Three interfaces rather than one, because they are three different machines' worth of facts
/// with three different reasons to be absent, and only one of them is read-only. Each is served
/// unconditionally: a laptop with no `wpctl` and a desktop with no battery both get an interface
/// that answers `present: false`, which a surface can report, rather than a missing one, which it
/// cannot tell apart from a daemon that failed to start.
async fn serve_hardware_status_interfaces(
    connection: &zbus::Connection,
    system_connection: Option<zbus::Connection>,
    store: Store,
) -> Result<()> {
    let object_server = connection.object_server();
    object_server
        .at(
            names::AUDIO_PATH,
            apis::audio::AudioApi::new(Arc::new(apis::audio::WpctlAudio), store),
        )
        .await
        .context("serving Audio1")?;
    // Backlight writes through logind, so it takes the same lazily retried system connection
    // Lifecycle1 does, and for the same reason: registering only when the system bus happened to
    // answer during startup is what makes a control silently dead for a whole session.
    object_server
        .at(
            names::BACKLIGHT_PATH,
            apis::backlight::BacklightApi::new(
                apis::backlight::SysfsBacklight::system(),
                std::sync::Arc::new(apis::backlight::LogindBrightness::new(system_connection)),
            ),
        )
        .await
        .context("serving Backlight1")?;
    object_server
        .at(
            names::BATTERY_PATH,
            apis::battery::BatteryApi::new(apis::battery::SysfsBatteries::system()),
        )
        .await
        .context("serving Battery1")?;
    Ok(())
}

/// `Session.Lifecycle1`: ending, suspending and locking, and the session's own edges.
///
/// Served whether or not the system bus was reachable at startup: `SystemLogind` retries lazily,
/// the way `Location1` does. Making the registration conditional is what made "the shutdown button
/// does nothing" possible — a session that lost that one race had no power controls at all for its
/// whole life and said so only in a startup warning. An interface that answers "the system bus is
/// unreachable" can be reported to the user; an absent one cannot.
async fn serve_lifecycle_interface(
    connection: &zbus::Connection,
    system_connection: Option<zbus::Connection>,
    services: &Services,
    launcher_control: Option<templar_client::compositor_control::CompositorControl>,
) -> Result<()> {
    let mut lifecycle = apis::lifecycle::LifecycleApi::new(
        Arc::new(apis::lifecycle::SystemLogind::new(system_connection)),
        // Logout goes through the *session* bus manager, which is this connection: ending
        // the session target is what lets the display manager see a clean exit.
        Arc::new(apis::lifecycle::UserManagerSession::new(connection.clone())),
        Arc::new(apis::lifecycle::PeerCgroupUnit::new(connection.clone())),
        services.session_edges.clone(),
    );
    if let Some(control) = launcher_control {
        lifecycle = lifecycle.with_launcher_control_fixture(control);
    }
    connection
        .object_server()
        .at(names::LIFECYCLE_PATH, lifecycle)
        .await
        .context("serving Session.Lifecycle1")?;
    Ok(())
}

/// The rest of the session-core domains: history/settings/projects/power/location.
async fn serve_policy_interfaces(
    connection: &zbus::Connection,
    system_connection: Option<zbus::Connection>,
    services: &Services,
    backend: LaunchBackend,
    sleep_ready: tokio::sync::watch::Receiver<bool>,
    wallpaper_control: Option<templar_client::compositor_control::CompositorControl>,
) -> Result<apis::homegarden::HomeGardenApi> {
    let object_server = connection.object_server();
    let appearance = apis::appearance::AppearanceEffects::new(
        services.schema.clone(),
        services.settings_values.clone(),
        xdg_home("XDG_CONFIG_HOME", ".config"),
    );
    if let Err(error) = appearance.reconcile(appearance.lock().await).await {
        tracing::warn!(%error, "loaded stored appearance; supported toolkit configuration incomplete");
    }
    let wallpaper = apis::wallpaper::WallpaperHost::new(
        services.store.clone(),
        services.schema.clone(),
        services.settings_values.clone(),
        wallpaper_control.clone(),
        services.wallpaper_spool_directory.clone(),
    );
    object_server
        .at(names::WALLPAPER_PATH, wallpaper.clone())
        .await
        .context("serving Wallpaper1")?;
    wallpaper.spawn_sources(connection.clone(), &services.session_edges);
    // Homegarden: the ledger behind Home's grown places. Built and subscribed to the session
    // edges and the launch channel here, before `Started` is sent (see `main`), so the Garden
    // Clock sees the session begin — and before HomeField1 and Projects1, which hand it the
    // explicit gestures.
    let garden = apis::homegarden::HomeGardenApi::new(
        services.store.clone(),
        services.history.clone(),
        services.providers.clone(),
        services.schema.clone(),
        services.settings_values.clone(),
        connection.clone(),
        services.registry.clone(),
    );
    apis::homegarden::spawn_sources(garden.clone(), &services.session_edges, &services.launches);
    object_server
        .at(
            names::HISTORY_PATH,
            apis::history::HistoryApi::new(
                services.history.clone(),
                services.store.clone(),
                services.engine.clone(),
                services.settings_values.clone(),
                services.providers.clone(),
                mimeapps_export_path(),
                appearance.clone(),
                wallpaper.clone(),
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
                appearance.clone(),
                wallpaper.clone(),
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
                garden.clone(),
            ),
        )
        .await
        .context("serving Projects1")?;
    object_server
        .at(
            names::HOME_FIELD_PATH,
            apis::homefield::HomeFieldApi::new(
                services.store.clone(),
                services.history.clone(),
                garden.clone(),
            ),
        )
        .await
        .context("serving HomeField1")?;
    object_server
        .at(names::HOME_GARDEN_PATH, garden.clone())
        .await
        .context("serving HomeGarden1")?;
    let input = apis::input::InputApi::new(services.store.clone())?;
    services
        .settings_values
        .write()
        .reset("input.touchpad.tap-to-click");
    object_server
        .at(apis::input::PATH, input)
        .await
        .context("serving Input1")?;
    object_server
        .at(
            apis::tab_counts::PATH,
            apis::tab_counts::TabCountsApi::new(connection.clone()),
        )
        .await
        .context("serving TabCounts1")?;
    object_server
        .at(
            apis::displays::PATH,
            apis::displays::DisplaysApi::new(services.store.clone())?,
        )
        .await
        .context("serving Displays1")?;
    apis::input::spawn_watch(connection.clone());
    apis::displays::spawn_watch(connection.clone());
    object_server
        .at(
            names::POWER_PATH,
            apis::power::PowerApi::new(
                services.store.clone(),
                connection.clone(),
                system_connection.clone(),
                services.session_edges.clone(),
                sleep_ready,
            )
            .await,
        )
        .await
        .context("serving Power1")?;
    serve_hardware_status_interfaces(
        connection,
        system_connection.clone(),
        services.store.clone(),
    )
    .await?;
    object_server
        .at(
            names::NOTIFICATIONS_PATH,
            apis::notifications::NotificationsControlApi::new(services.store.clone()),
        )
        .await
        .context("serving Notifications1")?;
    serve_lifecycle_interface(
        connection,
        system_connection.clone(),
        services,
        wallpaper_control,
    )
    .await?;
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
    Ok(garden)
}

/// Serve every `org.templaros.Session` interface, in-process, before the bus name is claimed.
async fn serve_interfaces(
    connection: &zbus::Connection,
    system_connection: Option<zbus::Connection>,
    services: &Services,
    backend: LaunchBackend,
    sleep_ready: tokio::sync::watch::Receiver<bool>,
    wallpaper_control: Option<templar_client::compositor_control::CompositorControl>,
) -> Result<apis::homegarden::HomeGardenApi> {
    serve_launch_and_association_interfaces(connection, services, backend).await?;
    serve_policy_interfaces(
        connection,
        system_connection,
        services,
        backend,
        sleep_ready,
        wallpaper_control,
    )
    .await
}

/// Tell `templar-hostd` the session came up, so `templar-boot-health` can tell "the session never
/// started" apart from "nobody has ever been able to report it".
///
/// That distinction is the whole point of the marker: a kernel or driver deployment on trial must
/// be able to fail because the session never appeared, and until something calls this the probe
/// can only report `Skipped` and a black-screened trial boot looks identical to a healthy one
/// (ADR-0004).
///
/// `ReportSessionHealthy` covers sessiond's policy/bus readiness. Compositor readiness is a
/// separate fact: a background probe waits until the public Wayland socket accepts a connection
/// and only then sends `ReportCompositorActive`. Keeping the reports separate means neither one
/// implies more than sessiond can observe.
///
/// Entirely best-effort. A desktop that refused to start because a boot-health marker could not
/// be written would be a far worse failure than the one this exists to catch.
const HEALTH_REPORT_TIMEOUT: Duration = Duration::from_secs(5);

async fn report_session_healthy(system_connection: Option<&zbus::Connection>) {
    if tokio::time::timeout(
        HEALTH_REPORT_TIMEOUT,
        report_session_healthy_once(system_connection),
    )
    .await
    .is_err()
    {
        tracing::warn!("templar-hostd session health report timed out; session remains available");
    }
}

async fn report_session_healthy_once(system_connection: Option<&zbus::Connection>) {
    let Some(connection) = system_connection else {
        tracing::warn!("no system bus; session health not reported to templar-hostd");
        return;
    };
    match templar_client::proxies::HostSessionProxy::new(connection).await {
        Ok(proxy) => match proxy.report_session_healthy().await {
            Ok(()) => tracing::info!("reported session health to templar-hostd"),
            // hostd confirms the caller's PID sits in templar-sessiond.service's cgroup before
            // believing this, so running the binary by hand is refused here. That is the check
            // working as designed, not a fault to escalate.
            Err(error) => tracing::warn!(
                %error,
                "templar-hostd would not record the session health report"
            ),
        },
        Err(error) => {
            tracing::warn!(%error, "templar-hostd unreachable; session health not reported");
        }
    }
}

fn resolve_wayland_socket(
    display: &std::ffi::OsStr,
    runtime_dir: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    let display = std::path::PathBuf::from(display);
    if display.is_absolute() {
        return Some(display);
    }
    Some(std::path::PathBuf::from(runtime_dir?).join(display))
}

fn wayland_socket_path() -> Option<std::path::PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    resolve_wayland_socket(&display, runtime.as_deref())
}

/// Wait for the compositor's public socket to accept a client, then report precisely that fact to
/// hostd. The bounded retry covers the normal race between the independently-started compositor
/// and sessiond services without holding up sessiond's own readiness notification.
fn spawn_compositor_active_report(system_connection: Option<zbus::Connection>) {
    let Some(connection) = system_connection else {
        tracing::warn!("no system bus; compositor readiness will not be reported to templar-hostd");
        return;
    };
    let Some(socket_path) = wayland_socket_path() else {
        tracing::warn!("WAYLAND_DISPLAY/XDG_RUNTIME_DIR absent; compositor readiness not probed");
        return;
    };

    tokio::spawn(async move {
        for attempt in 1..=30 {
            let path = socket_path.clone();
            let accepted = tokio::task::spawn_blocking(move || {
                std::os::unix::net::UnixStream::connect(path).is_ok()
            })
            .await
            .unwrap_or(false);
            if accepted {
                let report = async {
                    match templar_client::proxies::HostSessionProxy::new(&connection).await {
                        Ok(proxy) => match proxy.report_compositor_active().await {
                            Ok(()) => tracing::info!(
                                socket = %socket_path.display(),
                                "reported compositor readiness to templar-hostd"
                            ),
                            Err(error) => tracing::warn!(
                                %error,
                                "templar-hostd would not record compositor readiness"
                            ),
                        },
                        Err(error) => tracing::warn!(
                            %error,
                            "templar-hostd unreachable; compositor readiness not reported"
                        ),
                    }
                };
                if tokio::time::timeout(HEALTH_REPORT_TIMEOUT, report)
                    .await
                    .is_err()
                {
                    tracing::warn!("templar-hostd compositor readiness report timed out");
                }
                return;
            }
            if attempt < 30 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        tracing::warn!(
            socket = %socket_path.display(),
            "compositor did not accept a Wayland connection within 15 seconds"
        );
    });
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
    history_notifier: apis::history::HistoryChangeNotifier,
) {
    tokio::spawn(background::autostart::run(
        services.registry.clone(),
        services.providers.clone(),
        services.store.clone(),
        services.history.clone(),
        history_notifier.clone(),
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
        history_notifier.clone(),
    ));
    tokio::spawn(background::crash_ingest::run(
        services.store.clone(),
        history_notifier.clone(),
        Duration::from_secs(30),
    ));
    tokio::spawn(background::launch_watch::run(
        connection.clone(),
        services.registry.clone(),
        services.history.clone(),
        history_notifier,
        Duration::from_secs(10),
    ));
    tokio::spawn(background::notification_provider::run(
        connection.clone(),
        services.providers.clone(),
        services.store.clone(),
        services.history.clone(),
        services.registry.clone(),
        Duration::from_secs(5),
    ));
    tokio::spawn(background::desktop_watch::run(
        services.registry.clone(),
        services.store.clone(),
        services.resolver.clone(),
        connection.clone(),
        services.application_dirs.clone(),
        services.providers.clone(),
    ));
}

#[tokio::main]
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
    let mut services = load_services(&cli)?;

    if cli.dry_run {
        print_dry_run_summary(&services);
        return Ok(());
    }

    // Before the bus name is claimed, so before `Launcher1` can start PCManFM-Qt and before it
    // could read a profile still naming xterm and oxygen. Hermetic test runs leave the host's
    // configuration alone.
    if !cli.no_default_discovery {
        repair_pcmanfm_settings();
    }

    // Say which build is running, and which generation of the state schema it speaks — the
    // second is what lets a deployment refuse a set the previous one could not read back.
    // Diagnostic only: a stamp that cannot be written must never keep templar-sessiond from starting.
    if let Err(error) = templar_build::write_stamp_with(
        "templar-sessiond",
        [("session".to_owned(), templar_state::schema_version())]
            .into_iter()
            .collect(),
    ) {
        tracing::warn!(%error, "could not write the build stamp");
    }
    let connection = zbus::connection::Builder::session()?
        .build()
        .await
        .context("connecting to the session bus")?;
    // The null transport was only ever a stand-in for `--dry-run`/tests, where this line never
    // runs; the live daemon always relays checkpoints for real.
    services.checkpoint_transport = Arc::new(checkpoint::DbusCheckpointTransport::new(
        connection.clone(),
        services.registry.clone(),
    ));
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
    let host_connection = system_connection.clone();
    // One match rule on logind, so an hour asleep is not counted as an hour of session time.
    let sleep_watch = apis::lifecycle::spawn_sleep_watch(
        system_connection.clone(),
        services.session_edges.clone(),
    );
    let wallpaper_control = cli
        .wallpaper_control_socket
        .as_deref()
        .map(apis::wallpaper::fixture_control)
        .transpose()?;
    let garden = serve_interfaces(
        &connection,
        system_connection,
        &services,
        backend,
        sleep_watch.readiness(),
        wallpaper_control,
    )
    .await?;
    // Subscribed before anything is sent, so no edge — `Started` included — is emitted into an
    // empty channel and lost.
    apis::lifecycle::spawn_signal_relay(connection.clone(), &services.session_edges);

    // Background work (autostart, crash ingestion, the checkpoint relay) needs to emit
    // `History1.Changed` the same way a D-Bus-invoked mutation does; that requires the
    // interface to already be served, hence fetching it only now.
    let history_interface = connection
        .object_server()
        .interface::<_, apis::history::HistoryApi>(names::HISTORY_PATH)
        .await
        .context("fetching the served HistoryApi interface")?;
    let history_notifier = apis::history::HistoryChangeNotifier::new(history_interface);

    connection
        .request_name(names::BUS_NAME)
        .await
        .context("claiming org.templaros.Session")?;
    apis::audio::spawn_watch(connection.clone());
    tracing::info!(name = names::BUS_NAME, "templar-sessiond ready");

    // Readiness and the health report are the same fact told to two different supervisors: the
    // policy is loaded and the bus name is claimed. systemd learns it through sd_notify; hostd
    // learns it so a trial boot can fail on a session that never came up.
    start_systemd_notify();
    // Best-effort host reporting must not gate catalogue watches or session edges.
    let health_connection = host_connection.clone();
    tokio::spawn(async move {
        report_session_healthy(health_connection.as_ref()).await;
    });
    spawn_compositor_active_report(host_connection);
    spawn_background_tasks(&connection, &services, backend, history_notifier);

    // The last thing startup does, so every in-process subscriber — the signal relay and anything
    // `serve_interfaces` or `spawn_background_tasks` wired up — is already listening.
    let _ = services
        .session_edges
        .send(apis::lifecycle::SessionEdge::Started);

    // Run until systemd stops the session. A logout emits no lock edge, so the exit is where
    // the Garden Clock's last interval and any batched acts are written.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    if let Ok(wallpaper) = connection
        .object_server()
        .interface::<_, apis::wallpaper::WallpaperHost>(names::WALLPAPER_PATH)
        .await
    {
        wallpaper.get().await.shutdown().await;
    }
    tracing::info!("stopping: writing the Homegarden ledger");
    let flushing = garden.clone();
    let _ = tokio::task::spawn_blocking(move || flushing.flush()).await;
    Ok(())
}

#[cfg(test)]
mod readiness_tests {
    use super::resolve_wayland_socket;
    use std::{ffi::OsStr, path::PathBuf};

    #[test]
    fn relative_wayland_display_resolves_beneath_runtime_dir() {
        assert_eq!(
            resolve_wayland_socket(OsStr::new("wayland-0"), Some(OsStr::new("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/wayland-0"))
        );
    }

    #[test]
    fn absolute_wayland_display_does_not_need_runtime_dir() {
        assert_eq!(
            resolve_wayland_socket(OsStr::new("/tmp/nested-wayland"), None),
            Some(PathBuf::from("/tmp/nested-wayland"))
        );
        assert_eq!(resolve_wayland_socket(OsStr::new("wayland-0"), None), None);
    }

    #[tokio::test]
    async fn unresponsive_host_health_report_has_a_deadline() {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};
        use std::sync::Arc;

        struct OwnedBus(std::process::Child);
        impl Drop for OwnedBus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        struct UnresponsiveHost(Arc<tokio::sync::Notify>);
        #[zbus::interface(name = "org.templaros.Host.Session1")]
        impl UnresponsiveHost {
            async fn report_session_healthy(&self) {
                self.0.notify_one();
                std::future::pending::<()>().await;
            }
        }

        let mut bus = OwnedBus(
            Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .spawn()
                .expect("owned test bus"),
        );
        let mut address = String::new();
        std::io::BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let _host = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .serve_at("/org/templaros/Host", UnresponsiveHost(entered.clone()))
            .unwrap()
            .name("org.templaros.Host")
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let report = super::report_session_healthy(Some(&client));
        tokio::time::timeout(
            super::HEALTH_REPORT_TIMEOUT + std::time::Duration::from_secs(2),
            report,
        )
        .await
        .expect("best-effort health report must return despite an unresponsive host");
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .expect("production proxy actually reached the owned host method");
    }
}

#[cfg(test)]
mod mimeapps_tests {
    use super::{mimeapps_locations, read_mimeapps_defaults};
    use std::{ffi::OsStr, path::PathBuf};

    #[test]
    fn vendor_directories_and_desktop_preferences_follow_xdg_precedence() {
        let paths = mimeapps_locations(
            Some(PathBuf::from("/owned/config")),
            Some(PathBuf::from("/owned/data")),
            OsStr::new("/owned/vendor:/second/vendor"),
            OsStr::new("/owned/share:/usr/share"),
            "TemplarOS:LXQt",
        );
        assert_eq!(
            paths[0],
            PathBuf::from("/owned/config/templaros-mimeapps.list")
        );
        assert_eq!(paths[1], PathBuf::from("/owned/config/lxqt-mimeapps.list"));
        assert_eq!(paths[2], PathBuf::from("/owned/config/mimeapps.list"));
        assert_eq!(paths[5], PathBuf::from("/owned/vendor/mimeapps.list"));
        assert_eq!(
            paths[11],
            PathBuf::from("/owned/data/applications/mimeapps.list")
        );
        assert_eq!(
            paths[17],
            PathBuf::from("/usr/share/applications/mimeapps.list")
        );
        assert!(!paths.contains(&PathBuf::from("/etc/xdg/mimeapps.list")));
    }

    #[test]
    fn user_overrides_preserved_and_missing_defaults_inherit_vendor_policy() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user.list");
        let vendor = root.path().join("vendor.list");
        std::fs::write(
            &user,
            "[Default Applications]\nimage/png=user-viewer.desktop;\n",
        )
        .unwrap();
        std::fs::write(
            &vendor,
            "[Default Applications]\nimage/png=vendor-viewer.desktop;\ninode/directory=pcmanfm-qt.desktop;\n",
        ).unwrap();
        let defaults = read_mimeapps_defaults([user, vendor]);
        assert_eq!(defaults["image/png"], "user-viewer.desktop");
        assert_eq!(defaults["inode/directory"], "pcmanfm-qt.desktop");
    }
}
