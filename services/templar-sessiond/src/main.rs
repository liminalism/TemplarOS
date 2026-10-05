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
mod reconcile;

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
    match templar_associationd::store::mimeapps_already_imported(store) {
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
            let defaults = templar_associationd::import::parse_default_applications(&text);
            let resolved = templar_associationd::import::resolve_against_catalog(&defaults, catalog.as_ref());
            tracing::info!(path = %path.display(), count = resolved.len(), "importing mimeapps.list");
            resolved
        })
        .unwrap_or_default();
    if let Err(error) = store
        .transaction(|tx| templar_associationd::store::mark_mimeapps_imported_within(tx, &resolved))
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
        .transaction(|tx| templar_session_core::providers::declare_within(tx, &app_id, &capability))
    {
        tracing::warn!(%error, "failed to persist sessiond's notification-server declaration");
        return;
    }
    providers.write().declare(app_id, capability);
}

/// Declare the roles installed applications fill by what they *are* rather than by claiming them
/// (`templar_session_core::providers::derived_capabilities`).
///
/// **Not persisted, deliberately.** The declarations table exists so a claim an application made
/// — `X-TemplarOS-Provides=`, or `Providers1.Declare` — survives a restart. A derived declaration
/// has nothing to survive: it is read off the installed set, and the installed set is read on
/// every start. Writing it down would only create a way for the two to disagree, and they would:
/// nothing removes a row when the application it names is uninstalled, so a terminal that is no
/// longer on the machine would go on being offered — and, being the distribution default, would
/// go on *winning*.
///
/// Selections are a different matter and are stored: which terminal a person chose is a choice,
/// and this only ever changes what the choice is made from.
fn ensure_derived_provider_declarations(
    catalog: &dyn AppCatalog,
    providers: &Arc<RwLock<ProviderRegistry>>,
) {
    let mut declared = 0usize;
    for manifest in catalog.manifests() {
        for capability in templar_session_core::providers::derived_capabilities(&manifest) {
            providers.write().declare(manifest.id.clone(), capability);
            declared += 1;
        }
    }
    if declared > 0 {
        tracing::info!(count = declared, "derived provider declarations");
    }
}

/// Declare the roles installed applications claim in their own metadata
/// (`templar_session_core::providers::claimed_capabilities`).
///
/// The half of startup `ensure_derived_provider_declarations` does not cover: derived roles are
/// read off what an entry says for other reasons, while these are applications asking outright —
/// today that is `X-TemplarOS-Provides=` on a desktop entry. This pass was missing entirely: a
/// claim was parsed at import, recorded on the manifest's provenance, and then never read again,
/// so no declaration ever reached the registry and the role resolved to `NoProvider` however
/// correctly the application was installed. (`Providers1.Declare` is the only other path in, and
/// a desktop application never calls it — which is why nm-tray-2 stayed invisible to Settings
/// and left the network chip dimmed through two install attempts.)
///
/// Memory-only, for the same reason derived declarations are: the installed set is re-read on
/// every start, so persisting would only create rows an uninstall leaves behind — and a removed
/// application going on winning a role is exactly the failure the declarations table must not
/// have. The desktop watcher re-runs this after every rescan for the same reason startup runs it
/// after the import.
fn ensure_claimed_provider_declarations(
    catalog: &dyn AppCatalog,
    providers: &Arc<RwLock<ProviderRegistry>>,
) {
    let mut declared = 0usize;
    for manifest in catalog.manifests() {
        for capability in templar_session_core::providers::claimed_capabilities(&manifest) {
            providers.write().declare(manifest.id.clone(), capability);
            declared += 1;
        }
    }
    if declared > 0 {
        tracing::info!(count = declared, "claimed provider declarations");
    }
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
    /// Every successful launch, as `(resolved variant, arguments)`. Fed by `Launcher1`'s observer.
    launches: tokio::sync::broadcast::Sender<(
        templar_types::AppId,
        Vec<String>,
        templar_types::InstanceId,
    )>,
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
    ensure_derived_provider_declarations(catalog.as_ref(), &providers);
    ensure_claimed_provider_declarations(catalog.as_ref(), &providers);
    let schema = Arc::new(RwLock::new(baseline_schema()));
    let settings_values = Arc::new(RwLock::new(
        templar_session_core::settings::load_values(&store).context("loading settings values")?,
    ));
    let history = Arc::new(SqliteHistory::new(store.clone()));

    Ok(Services {
        store,
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
                Box::new(move |app_id, arguments, instance| {
                    let _ = launches.send((app_id.clone(), arguments.to_vec(), instance));
                }),
            ),
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
) -> Result<()> {
    let object_server = connection.object_server();
    object_server
        .at(
            names::AUDIO_PATH,
            apis::audio::AudioApi::new(std::sync::Arc::new(apis::audio::WpctlAudio)),
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
) -> Result<()> {
    connection
        .object_server()
        .at(
            names::LIFECYCLE_PATH,
            apis::lifecycle::LifecycleApi::new(
                Arc::new(apis::lifecycle::SystemLogind::new(system_connection)),
                // Logout goes through the *session* bus manager, which is this connection: ending
                // the session target is what lets the display manager see a clean exit.
                Arc::new(apis::lifecycle::UserManagerSession::new(connection.clone())),
                Arc::new(apis::lifecycle::PeerCgroupUnit::new(connection.clone())),
                services.session_edges.clone(),
            ),
        )
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
) -> Result<apis::homegarden::HomeGardenApi> {
    let object_server = connection.object_server();
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
    object_server
        .at(
            names::POWER_PATH,
            apis::power::PowerApi::new(services.store.clone()),
        )
        .await
        .context("serving Power1")?;
    serve_hardware_status_interfaces(connection, system_connection.clone()).await?;
    object_server
        .at(
            names::NOTIFICATIONS_PATH,
            apis::notifications::NotificationsControlApi::new(services.store.clone()),
        )
        .await
        .context("serving Notifications1")?;
    serve_lifecycle_interface(connection, system_connection.clone(), services).await?;
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
) -> Result<apis::homegarden::HomeGardenApi> {
    serve_launch_and_association_interfaces(connection, services, backend).await?;
    serve_policy_interfaces(connection, system_connection, services, backend).await
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
async fn report_session_healthy(system_connection: Option<&zbus::Connection>) {
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
        history_notifier,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let mut services = load_services(&cli)?;

    if cli.dry_run {
        print_dry_run_summary(&services);
        return Ok(());
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
    services.checkpoint_transport =
        Arc::new(checkpoint::DbusCheckpointTransport::new(connection.clone()));
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
    apis::lifecycle::spawn_sleep_watch(system_connection.clone(), services.session_edges.clone());
    let garden = serve_interfaces(&connection, system_connection, &services, backend).await?;
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
    tracing::info!(name = names::BUS_NAME, "templar-sessiond ready");

    // Readiness and the health report are the same fact told to two different supervisors: the
    // policy is loaded and the bus name is claimed. systemd learns it through sd_notify; hostd
    // learns it so a trial boot can fail on a session that never came up.
    start_systemd_notify();
    report_session_healthy(host_connection.as_ref()).await;
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
    tracing::info!("stopping: writing the Homegarden ledger");
    let flushing = garden.clone();
    let _ = tokio::task::spawn_blocking(move || flushing.flush()).await;
    Ok(())
}

#[cfg(test)]
mod claimed_provider_tests {
    use super::{ProviderRegistry, Registry, ensure_claimed_provider_declarations};
    use parking_lot::RwLock;
    use std::sync::Arc;
    use templar_types::{
        AppAliases, AppManifest, AppOriginKind, AppProvenance, ProviderRole, ResourceClass,
    };

    /// The installed nm-tray-2 entry as the import leaves it: a manifest whose provenance
    /// carries the `X-TemplarOS-Provides=` claim, at the trust its directory implies.
    fn tray(id: &str, origin: AppOriginKind) -> AppManifest {
        let mut provenance = AppProvenance::from_origin(origin);
        provenance
            .provider_claims
            .insert("network-status-ui".to_owned());
        AppManifest {
            id: id.parse().unwrap(),
            display_name: "nm-tray-alt".to_owned(),
            primary_desktop_file_id: "nm-tray2.desktop".to_owned(),
            executable: std::path::PathBuf::from("/usr/local/bin/nm-tray"),
            aliases: AppAliases::default(),
            mime_types: std::collections::BTreeSet::default(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: false,
            icon: None,
            categories: std::collections::BTreeSet::default(),
            keywords: std::collections::BTreeSet::default(),
            no_display: false,
            provenance: Some(provenance),
        }
    }

    #[test]
    fn a_trusted_desktop_claim_resolves_its_role() {
        let registry = Registry::default();
        registry.insert_manifest(tray(
            "desktop-import.nm-tray2.desktop",
            AppOriginKind::DistroPackage,
        ));
        let providers = Arc::new(RwLock::new(ProviderRegistry::default()));
        ensure_claimed_provider_declarations(&registry, &providers);
        assert_eq!(
            providers.read().provider(ProviderRole::NetworkStatusUi),
            Some("desktop-import.nm-tray2.desktop".parse().unwrap())
        );
    }

    #[test]
    fn an_untrusted_claim_leaves_the_role_empty() {
        let registry = Registry::default();
        registry.insert_manifest(tray(
            "desktop-import.nm-tray2.desktop",
            AppOriginKind::UserDesktopEntry,
        ));
        let providers = Arc::new(RwLock::new(ProviderRegistry::default()));
        ensure_claimed_provider_declarations(&registry, &providers);
        assert_eq!(
            providers.read().provider(ProviderRole::NetworkStatusUi),
            None
        );
    }
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
}
