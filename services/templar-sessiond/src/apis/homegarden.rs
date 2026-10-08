//! `org.templaros.HomeGarden1`: the activity ledger behind Home's grown places, and the tending
//! moments that bring the field in line with it (`templar_session_core::homegarden`, the planner in
//! `templar_field::garden`; `@templaros.decision.homegarden-activity-grown-places`).
//!
//! The ledger lives in the same store as the Home Field it tends and every field change goes
//! through `homefield::apply_op` as one `Batch`, so it is undoable through `HomeField1.Undo` and
//! visible in `History1` like any other change. The Garden Clock is driven by the session edges
//! `Lifecycle1` observes; launches with a directory argument arrive on the launch channel.
//! Nothing here ticks.

mod process_context;

/// This private display association cannot be returned as an IdentityResolution.
struct PresentationSender {
    instance: InstanceId,
    app: AppId,
    descendant: Option<(process_context::Association, fdo::ConnectionCredentials)>,
}

impl PresentationSender {
    fn ensure_current(&self, registry: &templar_appd::Registry) -> fdo::Result<()> {
        if let Some((association, credentials)) = &self.descendant {
            let live = credentials
                .process_fd()
                .is_some_and(|fd| process_context::require_live(fd).is_ok());
            if !live || !association.current(registry) {
                return Err(fdo::Error::AccessDenied(
                    "terminal observer lifetime or ancestry changed".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Descendant cwd is presentation only, including on subsequent tending passes.
fn record_descendant_directory(
    ledger: &mut Ledger,
    directory: &Path,
    instance: InstanceId,
    probe: &dyn RootProbe,
    config: &GardenConfig,
    desktop: bool,
) {
    ledger
        .working_directories
        .insert(instance.to_string(), directory.to_path_buf());
    ledger.prompt_directories.remove(&instance.to_string());
    let adopted = ledger.adopted_instances.get(&instance.to_string());
    for context in &mut ledger.contexts {
        if !adopted.is_some_and(|roots| roots.contains(&context.root)) {
            context.instances.retain(|known| *known != instance);
        }
    }
    if !desktop {
        ledger.associate_instance(directory, instance, probe, config);
    }
}

fn descendant_peer_pid(credentials: &fdo::ConnectionCredentials, uid: u32) -> fdo::Result<u32> {
    if credentials.unix_user_id() != Some(uid) {
        return Err(fdo::Error::AccessDenied(
            "observer is not the session user".into(),
        ));
    }
    let peer = credentials.process_fd().ok_or_else(|| {
        fdo::Error::AccessDenied(
            "bus cannot attest descendant observer process lifetime (ProcessFD unavailable)".into(),
        )
    })?;
    process_context::require_live(peer)
        .map_err(|error| fdo::Error::AccessDenied(error.to_string()))?;
    credentials
        .process_id()
        .ok_or_else(|| fdo::Error::AccessDenied("missing observer PID".into()))
}

use crate::apis::{
    failed,
    lifecycle::{SessionEdge, SessionEdges},
};
use futures_util::StreamExt as _;
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use templar_field::garden::{ContextView, HomeCorner, garden_tile_id, tend};
use templar_session_core::session_catalogue::{Catalogue, VerifiedTerminal};
use templar_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    homefield,
    homegarden::{self, ActivityKind, GardenConfig, HomeActivity, IndexChange, Ledger, RootProbe},
    providers::ProviderRegistry,
    settings::{SettingValue, SettingsSchema, SettingsValues},
};
use templar_state::Store;
use templar_types::{AppCatalog, AppId, InstanceId, ProviderRole, dbus::session as names};
use zbus::{fdo, object_server::SignalEmitter};

/// Everything the host shares between D-Bus calls and the edge listener.
struct Inner {
    ledger: Ledger,
    sessions: Catalogue,
    recent_files: Vec<RecentFileV1>,
    collection_errors: Vec<String>,
    /// Process-relative monotonic origin: the clock's intervals are never persisted, so the
    /// origin need not be.
    origin: Instant,
    /// Acts observed since the ledger was last handed to the writer.
    unsaved: u32,
    /// `monotonic_ms` when the ledger was last handed to the writer.
    saved_at_ms: u64,
}

impl Inner {
    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The moment now, with the clock brought up to it first. Every path that stamps an activity
    /// or writes the ledger goes through here, so what lands in the store is never behind what
    /// was just recorded.
    fn now(&mut self) -> homegarden::GardenMoment {
        let monotonic_ms = self.monotonic_ms();
        advance(&mut self.ledger.clock, monotonic_ms)
    }
}

/// Observed acts are written in batches rather than one ledger write per act: a prompt hook that
/// fires on every `cd` would otherwise serialize and commit the whole ledger each time. Every
/// other change — an edge, a tending, a verb — is written at once, and so is an act that finds
/// this many unsaved before it or the last write this long ago. Nothing ticks: the check happens
/// when an act arrives, and sessiond's exit flushes what is left.
const UNSAVED_ACTS: u32 = 16;
const UNSAVED_MS: u64 = 5 * 60 * 1000;

/// What the writer thread is asked to do. Saves are coalesced to the latest; a flush answers
/// once everything before it is on disk.
enum Write {
    Save(String),
    Sessions(String),
    Flush(mpsc::Sender<()>),
}

/// The one thread that writes the ledger, so no D-Bus handler waits on SQLite and no older
/// snapshot can land after a newer one (the channel is ordered and only the latest save of a
/// burst is written).
fn spawn_writer(store: Store) -> mpsc::Sender<Write> {
    let (tx, rx) = mpsc::channel::<Write>();
    let spawned = std::thread::Builder::new()
        .name("garden-writer".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut latest = None;
                let mut sessions = None;
                let mut acks = Vec::new();
                for request in std::iter::once(first).chain(rx.try_iter()) {
                    match request {
                        Write::Save(json) => latest = Some(json),
                        Write::Sessions(json) => sessions = Some(json),
                        Write::Flush(ack) => acks.push(ack),
                    }
                }
                if let Some(json) = latest
                    && let Err(error) = homegarden::save_json(&store, &json)
                {
                    tracing::warn!(%error, "failed to persist the Homegarden ledger");
                }
                if let Some(json) = sessions
                    && let Err(error) = store.transaction(|tx| {
                        tx.execute(
                            "INSERT INTO home_garden (key,value_json,updated_ms) VALUES (?1,?2,?3)
                             ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,updated_ms=excluded.updated_ms",
                            ("presentation-v1-sessions", &json, templar_state::now_ms()),
                        )?;
                        Ok(())
                    })
                {
                    tracing::warn!(%error, "failed to persist Home session catalogue");
                }
                for ack in acks {
                    let _ = ack.send(());
                }
            }
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "the Homegarden writer thread did not start; nothing will persist");
    }
    tx
}

/// Fold the open interval into the clock's total and read the moment.
///
/// The daemon's only other way to close an interval is a lock or a sleep edge, and a session
/// that ends without one — a logout, or sessiond stopping — emits no edge at all. Doing this
/// before every write is what makes garden time survive a session boundary.
fn advance(clock: &mut homegarden::GardenClock, monotonic_ms: u64) -> homegarden::GardenMoment {
    clock.checkpoint(monotonic_ms);
    clock.now(monotonic_ms)
}

#[derive(Clone)]
struct DesktopRoot {
    /// Filed's configured lexical name is also the dock's persisted binding.
    path: PathBuf,
    /// Kernel/process cwd evidence uses filesystem identity, including Desktop symlinks.
    identity: PathBuf,
}

impl DesktopRoot {
    fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.path) || path.starts_with(&self.identity)
    }
}

#[derive(Clone)]
pub struct HomeGardenApi {
    inner: Arc<Mutex<Inner>>,
    store: Store,
    history: Arc<SqliteHistory>,
    providers: Arc<RwLock<ProviderRegistry>>,
    schema: Arc<RwLock<SettingsSchema>>,
    settings: Arc<RwLock<SettingsValues>>,
    probe: Arc<DaemonProbe>,
    connection: zbus::Connection,
    writer: mpsc::Sender<Write>,
    apps: Arc<templar_appd::Registry>,
    desktop_root: Arc<RwLock<Option<DesktopRoot>>>,
}

impl std::fmt::Debug for HomeGardenApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HomeGardenApi")
            .finish_non_exhaustive()
    }
}

impl HomeGardenApi {
    pub fn new(
        store: Store,
        history: Arc<SqliteHistory>,
        providers: Arc<RwLock<ProviderRegistry>>,
        schema: Arc<RwLock<SettingsSchema>>,
        settings: Arc<RwLock<SettingsValues>>,
        connection: zbus::Connection,
        apps: Arc<templar_appd::Registry>,
    ) -> Self {
        let probe = Arc::new(DaemonProbe::new(store.clone()));
        let mut ledger = homegarden::load(&store);
        ledger
            .contexts
            .retain(|context| !probe.home().starts_with(&context.root));
        // All pre-versioned cwd entries came from verified prompt hooks.
        if ledger.cwd_sources_version == 0 {
            ledger
                .prompt_directories
                .extend(ledger.working_directories.keys().cloned());
            ledger.cwd_sources_version = 1;
        }
        let mut sessions = {
            let conn = store.lock();
            let json = conn
                .query_row(
                    "SELECT value_json FROM home_garden WHERE key=?1",
                    ["presentation-v1-sessions"],
                    |row| row.get::<_, String>(0),
                )
                .ok();
            json.and_then(|json| match serde_json::from_str::<Catalogue>(&json) {
                Ok(catalogue) => Some(catalogue),
                Err(error) => {
                    tracing::warn!(%error, "stored Home session catalogue is unreadable");
                    None
                }
            })
            .unwrap_or_default()
        };
        sessions.mark_resting();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                ledger,
                sessions,
                recent_files: Vec::new(),
                collection_errors: Vec::new(),
                origin: Instant::now(),
                unsaved: 0,
                saved_at_ms: 0,
            })),
            probe,
            writer: spawn_writer(store.clone()),
            store,
            history,
            providers,
            schema,
            settings,
            connection,
            apps,
            desktop_root: Arc::new(RwLock::new(None)),
        }
    }

    fn config(&self) -> GardenConfig {
        let schema = self.schema.read();
        let values = self.settings.read();
        GardenConfig::from_settings(&schema, &values)
    }

    fn corner(&self) -> HomeCorner {
        let schema = self.schema.read();
        let values = self.settings.read();
        match values.effective(&schema, "shell.home-garden.home-corner") {
            Some(SettingValue::Choice(name)) => HomeCorner::parse(&name).unwrap_or_default(),
            _ => HomeCorner::default(),
        }
    }

    fn persist(&self, inner: &mut Inner) {
        // The clock first: `interval_started` is deliberately not persisted, so an interval that
        // is not folded in here is lost outright when the session ends without a lock edge.
        if let Ok(json) = serde_json::to_string(&inner.sessions) {
            let _ = self.writer.send(Write::Sessions(json));
        }
        let _ = inner.now();
        match serde_json::to_string(&inner.ledger) {
            Ok(json) => {
                let _ = self.writer.send(Write::Save(json));
                inner.unsaved = 0;
                inner.saved_at_ms = inner.monotonic_ms();
            }
            Err(error) => tracing::warn!(%error, "failed to serialize the Homegarden ledger"),
        }
    }

    /// One act was recorded: write it with the batch it belongs to.
    fn persist_observed(&self, inner: &mut Inner) {
        inner.unsaved += 1;
        if inner.unsaved >= UNSAVED_ACTS
            || inner.monotonic_ms().saturating_sub(inner.saved_at_ms) >= UNSAVED_MS
        {
            self.persist(inner);
        }
    }

    /// Write everything now and wait (briefly) until it is on disk: sessiond is exiting, and a
    /// logout emits no lock edge, so this is what carries the session's garden time and its last
    /// acts across.
    pub fn flush(&self) {
        {
            let mut inner = self.inner.lock().expect("garden lock");
            self.persist(&mut inner);
        }
        let (ack, done) = mpsc::channel();
        if self.writer.send(Write::Flush(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(3));
        }
    }

    /// The role `app_id` holds among the ones a Place learns (Terminal, FileManager, CodeEditor).
    fn role_of(&self, app_id: &AppId) -> Option<ProviderRole> {
        let providers = self.providers.read();
        [
            ProviderRole::Terminal,
            ProviderRole::FileManager,
            ProviderRole::CodeEditor,
        ]
        .into_iter()
        .find(|role| {
            providers
                .candidates(*role)
                .iter()
                .any(|(candidate, _)| candidate == app_id)
        })
    }

    /// Record one act. Shared by the D-Bus method and the in-process sources.
    pub fn observe_activity(
        &self,
        kind: ActivityKind,
        path: PathBuf,
        app_id: Option<AppId>,
        role: Option<ProviderRole>,
        instance_id: Option<InstanceId>,
    ) {
        let lexical_desktop = self.is_desktop_path(&path);
        let path = path.canonicalize().unwrap_or(path);
        let desktop = lexical_desktop || self.is_desktop_path(&path);
        let mut config = self.config();
        if desktop {
            if kind != ActivityKind::TerminalEntered {
                return;
            }
            // A verified cd onto the staging shelf still detaches the previous project.
            // Ledger cwd maintenance precedes its enabled guard; no Desktop Place can grow.
            config.enabled = false;
        }
        if !config.enabled && kind != ActivityKind::TerminalEntered {
            return;
        }
        let role = role.or_else(|| app_id.as_ref().and_then(|app| self.role_of(app)));
        let template = app_id
            .as_ref()
            .and_then(|app| self.apps.manifest(app))
            .map_or(homegarden::Template::Plain, |manifest| {
                homegarden::Template::from_categories(&manifest.categories, role)
            });
        let mut inner = self.inner.lock().expect("garden lock");
        let at = inner.now();
        if kind == ActivityKind::TerminalEntered {
            inner.ledger.working_directories.retain(|id, _| {
                InstanceId::parse(id).is_some_and(|instance| self.apps.contains_instance(instance))
            });
        }
        let activity = HomeActivity {
            path,
            kind,
            app_id,
            role,
            at,
            instance_id,
        };
        let root = inner
            .ledger
            .observe(&activity, self.probe.as_ref(), &config);
        if let Some(root) = &root {
            if let Some(context) = inner.ledger.context_mut(root) {
                context.record_template(template, true, at);
                if matches!(kind, ActivityKind::FileOpened | ActivityKind::FileSaved) {
                    context.record_template(
                        homegarden::Template::from_path(&activity.path),
                        false,
                        at,
                    );
                }
            }
            tracing::info!(kind = kind.as_str(), root = %root.display(), "garden observed");
        }
        if root.is_some() || (kind == ActivityKind::TerminalEntered && instance_id.is_some()) {
            self.persist_observed(&mut inner);
        }
    }

    /// A launch happened: a directory argument is a `LaunchedHere` on that directory, and either
    /// way the instance it produced belongs to whatever place the argument lies under.
    ///
    /// The two associations the doc calls strong are both this one event. A directory argument is
    /// "launched here" and is evidence; a file argument is the brokered open, whose *weight* the
    /// shell has already reported as `file-opened` — so the instance is attached without counting
    /// the act twice. This source knows exactly which instance the broker produced; the
    /// external terminal source separately authenticates its instance from the bus peer.
    pub fn observe_launch(&self, event: &templar_appd::dbus::LaunchEvent) {
        let app_id = &event.app_id;
        let arguments = &event.arguments;
        let instance = event.instance;
        if let Some(directory) = event
            .directory
            .clone()
            .or_else(|| launch_directory(arguments))
        {
            self.observe_activity(
                ActivityKind::LaunchedHere,
                directory,
                Some(app_id.clone()),
                None,
                Some(instance),
            );
        }
        let Some(file) = launch_file(arguments) else {
            return;
        };
        if self.is_desktop_path(&file) {
            return;
        }
        let file = file.canonicalize().unwrap_or(file);
        let config = self.config();
        if !config.enabled {
            return;
        }
        let mut inner = self.inner.lock().expect("garden lock");
        if let Some(root) =
            inner
                .ledger
                .associate_instance(&file, instance, self.probe.as_ref(), &config)
        {
            tracing::info!(root = %root.display(), "garden associated a launched instance");
            self.persist_observed(&mut inner);
        }
    }

    fn on_edge(&self, edge: SessionEdge) {
        {
            let mut inner = self.inner.lock().expect("garden lock");
            let now = inner.monotonic_ms();
            match edge {
                SessionEdge::Started => inner.ledger.clock.start_session(now),
                SessionEdge::Unlocked | SessionEdge::Sleeping(false) => {
                    inner.ledger.clock.resume(now);
                }
                SessionEdge::Locked | SessionEdge::Sleeping(true) => inner.ledger.clock.pause(now),
                SessionEdge::Idle(idle) => inner.ledger.clock.set_idle(idle, now),
            }
            self.persist(&mut inner);
        }
    }

    pub fn desktop_root(&self) -> Option<PathBuf> {
        self.desktop_root
            .read()
            .as_ref()
            .map(|root| root.path.clone())
    }

    fn is_desktop_path(&self, path: &Path) -> bool {
        self.desktop_root
            .read()
            .as_ref()
            .is_some_and(|root| root.contains(path))
    }

    /// Filed alone resolves the staging shelf path. Missing authority leaves existing layout
    /// untouched and reports unavailability; no guessed/new directory is manufactured.
    pub async fn refresh_desktop_root(&self) {
        use std::os::unix::ffi::OsStringExt as _;
        let result = async {
            let proxy = templar_client::proxies::FilesBrowseProxy::new(&self.connection).await?;
            let encoded =
                tokio::time::timeout(Duration::from_secs(2), proxy.desktop_directory_v1())
                    .await??;
            if encoded.is_empty() {
                return Ok::<_, anyhow::Error>(None);
            }
            anyhow::ensure!(
                encoded.len() <= 8192,
                "Desktop directory exceeds native path bound"
            );
            let root = PathBuf::from(std::ffi::OsString::from_vec(
                templar_client::file_index::decode_path(&encoded)?,
            ));
            anyhow::ensure!(
                root.is_absolute()
                    && !self.probe.home().starts_with(&root)
                    && !root
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir)),
                "Desktop directory is not a valid staging root"
            );
            let identity = root.canonicalize()?;
            anyhow::ensure!(
                identity.is_dir() && !self.probe.home().starts_with(&identity),
                "Desktop identity is a Home ancestor"
            );
            Ok(Some(DesktopRoot {
                path: root,
                identity,
            }))
        }
        .await;
        let mut inner = self.inner.lock().expect("garden lock");
        inner
            .collection_errors
            .retain(|error| !error.starts_with("Desktop directory unavailable:"));
        match result {
            Ok(root) => {
                if let Some(root) = &root {
                    inner
                        .ledger
                        .contexts
                        .retain(|context| !root.contains(&context.root));
                }
                *self.desktop_root.write() = root;
                self.persist(&mut inner);
            }
            Err(error) => inner
                .collection_errors
                .push(format!("Desktop directory unavailable: {error}")),
        }
    }

    /// Only a committed explicit resize establishes a size override. Place/Move/Pin do not.
    pub fn record_manual_resize(
        &self,
        op: &templar_field::FieldOp,
        doc: &templar_field::FieldDocument,
    ) {
        fn resized(op: &templar_field::FieldOp, out: &mut Vec<templar_field::TileId>) {
            match op {
                templar_field::FieldOp::Resize { id, .. } => out.push(*id),
                templar_field::FieldOp::Batch { ops } => {
                    for op in ops {
                        resized(op, out);
                    }
                }
                _ => (),
            }
        }
        let mut ids = Vec::new();
        resized(op, &mut ids);
        if ids.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().expect("garden lock");
        for context in &mut inner.ledger.contexts {
            if ids.contains(&garden_tile_id(&context.root))
                && let Some(location) = doc.location(garden_tile_id(&context.root))
            {
                context.record_manual_span(location.placement.span);
            }
        }
        self.persist(&mut inner);
    }

    /// Undo restores geometry; an already explicit size follows its restored span.
    pub fn record_manual_resize_undo(&self, doc: &templar_field::FieldDocument) {
        let mut inner = self.inner.lock().expect("garden lock");
        for context in &mut inner.ledger.contexts {
            if context.manual_size
                && let Some(location) = doc.location(garden_tile_id(&context.root))
            {
                context.user_span = Some(location.placement.span);
            }
        }
        self.persist(&mut inner);
    }

    async fn verified_sender(
        &self,
        header: &zbus::message::Header<'_>,
    ) -> fdo::Result<templar_appd::identity::IdentityResolution> {
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing unique sender".into()))?;
        let bus = fdo::DBusProxy::new(&self.connection).await?;
        let uid = bus.get_connection_unix_user(sender.clone().into()).await?;
        let status = std::fs::read_to_string("/proc/self/status").map_err(failed)?;
        let own_uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|uid| uid.parse::<u32>().ok());
        if own_uid != Some(uid) {
            return Err(fdo::Error::AccessDenied(
                "observer is not the session user".into(),
            ));
        }
        let pid = bus
            .get_connection_unix_process_id(sender.clone().into())
            .await?;
        let identity = self
            .apps
            .resolve_window(pid, &templar_types::WindowClaims::default())
            .map_err(failed)?;
        if !identity.confidence.is_verified()
            || identity.instance_id.is_none()
            || identity.app_id.is_none()
        {
            return Err(fdo::Error::AccessDenied(
                "observer is not a managed Launcher1 instance".into(),
            ));
        }
        Ok(identity)
    }

    /// Only HomeGarden display/session observations may use this association. Canonical
    /// application/provider/permission callers continue to use exact registered cgroups.
    async fn presentation_sender(
        &self,
        header: &zbus::message::Header<'_>,
    ) -> fdo::Result<PresentationSender> {
        if let Ok(identity) = self.verified_sender(header).await {
            return Ok(PresentationSender {
                instance: identity.instance_id.expect("verified"),
                app: identity.app_id.expect("verified"),
                descendant: None,
            });
        }
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing unique sender".into()))?;
        let bus = fdo::DBusProxy::new(&self.connection).await?;
        let credentials = bus
            .get_connection_credentials(sender.clone().into())
            .await?;
        let uid = process_context::session_uid().map_err(failed)?;
        let pid = descendant_peer_pid(&credentials, uid)?;
        let selected = self.providers.read().provider(ProviderRole::Terminal);
        let association = process_context::associate(&self.apps, pid, uid, selected.as_ref())
            .map_err(|error| fdo::Error::AccessDenied(error.to_string()))?;
        if !association.is_descendant() {
            return Err(fdo::Error::AccessDenied(
                "observer is not a terminal descendant".into(),
            ));
        }
        let observer = PresentationSender {
            instance: association.record.id,
            app: association.record.app_id.clone(),
            descendant: Some((association, credentials)),
        };
        observer.ensure_current(&self.apps)?;
        Ok(observer)
    }

    /// Strong kernel evidence at tending/activation. No process-name or title grants.
    async fn collect_sources(&self, activated: Option<InstanceId>) {
        let registry = self.apps.clone();
        let selected = self.providers.read().provider(ProviderRole::Terminal);
        let scan = tokio::task::spawn_blocking(move || {
            let (instances, mut errors) = scan_instances(&registry);
            let contexts = match process_context::session_uid() {
                Ok(uid) => process_context::discover(&registry, &instances, uid, selected.as_ref()),
                Err(error) => {
                    errors.push(format!("terminal context ownership unavailable: {error}"));
                    Vec::new()
                }
            };
            (instances, contexts, errors)
        })
        .await;
        let (instances, contexts, mut errors) = match scan {
            Ok(value) => value,
            Err(error) => (
                Vec::new(),
                Vec::new(),
                vec![format!("process collection unavailable: {error}")],
            ),
        };
        let prompt_directories = {
            let inner = self.inner.lock().expect("garden lock");
            inner
                .ledger
                .working_directories
                .iter()
                .filter(|(id, _)| inner.ledger.prompt_directories.contains(*id))
                .map(|(id, path)| (id.clone(), path.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let live: std::collections::BTreeSet<_> =
            instances.iter().map(|(record, _, _)| record.id).collect();
        for (record, _, directories) in &instances {
            if self.role_of(&record.app_id) == Some(ProviderRole::Terminal)
                && let Some(directory) = prompt_directories.get(&record.id.to_string())
            {
                self.observe_activity(
                    ActivityKind::TerminalEntered,
                    directory.clone(),
                    Some(record.app_id.clone()),
                    None,
                    Some(record.id),
                );
                continue;
            }
            for directory in directories {
                self.observe_activity(
                    ActivityKind::ProjectEntered,
                    directory.clone(),
                    Some(record.app_id.clone()),
                    None,
                    Some(record.id),
                );
            }
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let (roots, process_cwds) = {
            let mut inner = self.inner.lock().expect("garden lock");
            let terminals: Vec<_> = instances
                .iter()
                .filter_map(|(record, _, directories)| {
                    let pids: Vec<_> = contexts
                        .iter()
                        .filter(|context| {
                            context.record.id == record.id && context.current(&self.apps)
                        })
                        .map(process_context::Association::pid)
                        .collect();
                    (!pids.is_empty()).then(|| VerifiedTerminal {
                        instance_id: record.id,
                        pids,
                        cwd: inner
                            .ledger
                            .working_directories
                            .get(&record.id.to_string())
                            .cloned()
                            .or_else(|| directories.first().cloned())
                            .unwrap_or_else(|| self.probe.home().to_path_buf()),
                    })
                })
                .collect();
            if let Some(runtime) = runtime {
                if let Err(error) = inner.sessions.collect(
                    Path::new("/proc"),
                    &runtime,
                    &terminals,
                    templar_state::now_ms().max(0) as u64,
                    |instance, pid| {
                        contexts.iter().any(|context| {
                            context.pid() == pid
                                && context.record.id == instance
                                && context.current(&self.apps)
                        })
                    },
                ) {
                    errors.push(error.to_string());
                }
            } else {
                errors.push("session journals unavailable: XDG_RUNTIME_DIR is absent".into());
            }
            inner
                .ledger
                .working_directories
                .retain(|id, _| InstanceId::parse(id).is_some_and(|id| live.contains(&id)));
            inner
                .ledger
                .prompt_directories
                .retain(|id| InstanceId::parse(id).is_some_and(|id| live.contains(&id)));
            let mut process_cwds = Vec::new();
            inner.sessions.reconcile_instances(&live);
            for row in inner
                .sessions
                .rows()
                .iter()
                .filter(|row| row.kind == "terminal" && row.state != "ended")
            {
                if let Some(id) = row
                    .instance_id
                    .as_ref()
                    .and_then(|id| InstanceId::parse(id))
                {
                    if !inner.ledger.prompt_directories.contains(&id.to_string()) {
                        inner
                            .ledger
                            .working_directories
                            .insert(id.to_string(), row.cwd.clone());
                        let adopted = inner
                            .ledger
                            .adopted_instances
                            .get(&id.to_string())
                            .cloned()
                            .unwrap_or_default();
                        for context in &mut inner.ledger.contexts {
                            if !adopted.contains(&context.root) {
                                context.instances.retain(|known| *known != id);
                            }
                        }
                        if let Some(record) = self.apps.instance(id) {
                            process_cwds.push((id, record.app_id, row.cwd.clone()));
                        }
                    }
                }
            }
            inner.collection_errors.retain(|error| {
                error.starts_with("Desktop directory unavailable:")
                    || error.starts_with("recent files unavailable:")
            });
            inner.collection_errors.extend(errors);
            self.persist(&mut inner);
            let roots = inner
                .ledger
                .contexts
                .iter()
                .filter(|context| {
                    !context.hidden
                        && (context.presented_template() == homegarden::Template::Coding
                            || context.roles.iter().any(|role| {
                                matches!(role, ProviderRole::Terminal | ProviderRole::CodeEditor)
                            }))
                })
                .map(|context| context.root.clone())
                .collect::<Vec<_>>();
            (roots, process_cwds)
        };
        for (instance, _app, directory) in process_cwds {
            let mut inner = self.inner.lock().expect("garden lock");
            // Foreground/session display data may refine existing containment only.
            // Raw verified process directories above are the independent strong source.
            if !self.is_desktop_path(&directory) {
                inner.ledger.associate_instance(
                    &directory,
                    instance,
                    self.probe.as_ref(),
                    &self.config(),
                );
            }
            self.persist(&mut inner);
        }
        // Foreground processes/journals refresh on activation. Public agent CLI inventory is
        // substantially slower and belongs only to a tending moment, never every focus edge.
        if activated.is_some() {
            return;
        }
        let inventories: Vec<_> =
            futures_util::stream::iter(roots.into_iter().map(|root| async move {
                let result = crate::session_collector::collect_claude_agents(&root).await;
                (root, result)
            }))
            .buffer_unordered(4)
            .collect()
            .await;
        // CLI collection can take seconds. A PID snapshot from before that await is not
        // authority: reverify membership before attaching any public-inventory session row.
        let verified_pids: std::collections::BTreeMap<_, _> = instances
            .iter()
            .flat_map(|(record, pids, _)| {
                pids.iter().filter_map(|pid| {
                    self.apps
                        .resolve_window(*pid, &templar_types::WindowClaims::default())
                        .ok()
                        .filter(|identity| {
                            identity.confidence.is_verified()
                                && identity.instance_id == Some(record.id)
                        })
                        .map(|_| (*pid, record.id))
                })
            })
            .chain(
                contexts
                    .iter()
                    .filter(|context| context.current(&self.apps))
                    .map(|context| (context.pid(), context.record.id)),
            )
            .collect();
        let mut inner = self.inner.lock().expect("garden lock");
        for (root, result) in inventories {
            let result = result.and_then(|json| {
                inner.sessions.observe_claude_inventory(
                    &root,
                    &json,
                    &verified_pids,
                    templar_state::now_ms().max(0) as u64,
                )
            });
            if let Err(error) = result {
                inner.collection_errors.push(format!(
                    "{} agent inventory unavailable: {error}",
                    root.display()
                ));
            }
        }
        self.persist(&mut inner);
    }

    async fn read_recents(&self) {
        let result = async {
            let proxy = templar_client::proxies::FilesRecentsProxy::new(&self.connection).await?;
            let json = proxy.list("", 128).await?;
            if json.len() > 512 * 1024 {
                return Err(anyhow::anyhow!("recent-file response exceeds bound"));
            }
            Ok::<Vec<RecentFileV1>, anyhow::Error>(serde_json::from_str::<Vec<RecentFileV1>>(
                &json,
            )?)
        }
        .await;
        let mut inner = self.inner.lock().expect("garden lock");
        inner
            .collection_errors
            .retain(|error| !error.starts_with("recent files unavailable:"));
        match result {
            Ok(files) => {
                inner.recent_files = files
                    .into_iter()
                    .filter(|file| file.path.is_absolute())
                    .take(128)
                    .collect()
            }
            Err(error) => {
                inner.recent_files.clear();
                inner
                    .collection_errors
                    .push(format!("recent files unavailable: {error}"));
            }
        }
    }

    /// One tending moment for `output`: stages, then the field.
    fn tend_output(&self, output: &str) -> anyhow::Result<TendReport> {
        let config = self.config();
        if !config.enabled {
            return Ok(TendReport::default());
        }
        let corner = self.corner();
        let live = self.live_instances();
        let mut inner = self.inner.lock().expect("garden lock");
        let now = inner.now();
        let changes = inner.ledger.tend(now, &config);
        let Some(mut doc) = homefield::load(&self.store, output)? else {
            self.persist(&mut inner);
            return Ok(TendReport::default());
        };
        let dock_changed = if let Some(desktop) = self.desktop_root() {
            homefield::migrate_desktop_dock(&self.store, &mut doc, &desktop)?
        } else {
            false
        };
        // The user's hand since last time: a moved tile updates its landing; a resized tile
        // becomes user-sized and the tender stops resizing it; a tile the tender never placed —
        // a folder the user put on Home by hand, or one that was there before the garden was —
        // is pinned against recession. Placement/pinning never freezes its size.
        for context in &mut inner.ledger.contexts {
            if let Some(located) = doc.location(garden_tile_id(&context.root)) {
                context.landing = Some(located.placement.cell);
                match context.tended_span {
                    None if !context.hidden => {
                        context.pinned = true;
                        // Hand placement establishes position, not a manually chosen size.
                        context.tended_span = Some(located.placement.span);
                    }
                    Some(_) | None => {}
                }
            }
        }
        let views: Vec<ContextView> = inner
            .ledger
            .contexts
            .iter()
            .map(|context| ContextView {
                root: context.root.clone(),
                wanted: context.presentation_span(
                    now,
                    &config,
                    context
                        .instances
                        .iter()
                        .filter(|id| live.contains(*id))
                        .count(),
                ),
                landing: context.landing,
                pinned: context.pinned,
                user_span: context.user_span,
                summary: context.summary(),
            })
            .collect();
        let tended = tend(&doc, &views, corner);
        let mut report = TendReport {
            changed: u32::from(dock_changed),
            revision: doc.revision,
            stages: changes
                .iter()
                .map(|change| format!("{}: {}", change.root.display(), change.reason))
                .collect(),
            ops: Vec::new(),
        };
        if !tended.is_empty() {
            let batch = templar_field::FieldOp::Batch {
                ops: tended.iter().map(|t| t.op.clone()).collect(),
            };
            let next = homefield::apply_op(&self.store, output, &batch)?;
            for context in &mut inner.ledger.contexts {
                if let Some(located) = next.location(garden_tile_id(&context.root)) {
                    context.landing = Some(located.placement.cell);
                    context.tended_span = Some(located.placement.span);
                }
            }
            report.changed = report
                .changed
                .saturating_add(u32::try_from(tended.len()).unwrap_or(u32::MAX));
            report.ops = tended
                .iter()
                .map(|t| (t.op.label().to_owned(), t.reason.clone()))
                .collect();
            report.revision = next.revision;
            let message = format!(
                "Home on {output} tended: {}.",
                tended
                    .iter()
                    .map(|t| t.reason.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            self.record(message);
        }
        self.persist(&mut inner);
        Ok(report)
    }

    fn live_instances(&self) -> std::collections::BTreeSet<InstanceId> {
        self.apps
            .list_instances()
            .into_iter()
            .filter(|record| record.has_managed_identity())
            .filter(|record| {
                let group = Path::new("/sys/fs/cgroup").join(
                    record
                        .control_group
                        .strip_prefix("/")
                        .unwrap_or(&record.control_group),
                );
                std::fs::read_to_string(group.join("cgroup.events"))
                    .is_ok_and(|events| events.lines().any(|line| line == "populated 1"))
            })
            .map(|record| record.id)
            .collect()
    }

    /// Where `root`'s tile is placed, on every output that has it.
    /// Read the shared index's change log once, at this tending moment, and fold it into the
    /// places that already exist.
    ///
    /// One `ChangesSince` call from a persisted cursor, and **no subscription**: the garden
    /// never registers for `GenerationAdvanced`, because a bloom is only ever *drawn*, and
    /// drawing only happens at a tending moment. A resident listener would be a background
    /// wakeup per index generation for news nobody is looking at, which is exactly what
    /// `no-idle-wakeups` and `no-index-growth` exist to prevent.
    ///
    /// Below the retained floor the log cannot prove continuity, so there is no history to show:
    /// the cursor moves to the log's head, every bloom is cleared, and this moment shows
    /// nothing. Never a read from sequence 0 — that would bloom the whole index at once.
    async fn read_index_changes(&self) {
        let Ok(proxy) = templar_client::proxies::FilesIndexProxy::new(&self.connection).await
        else {
            return;
        };
        let cursor = {
            let inner = self.inner.lock().expect("garden lock");
            inner.ledger.change_cursor
        };
        let batch_json = match proxy.changes_since(cursor, CHANGE_PAGE).await {
            Ok(json) => json,
            Err(error) => {
                if is_snapshot_required(&error) {
                    self.rebase_change_cursor(&proxy).await;
                } else {
                    tracing::debug!(%error, "no change log this tending moment");
                }
                return;
            }
        };
        let batch: WireChanges = match serde_json::from_str(&batch_json) {
            Ok(batch) => batch,
            Err(error) => {
                tracing::warn!(%error, "the index change page did not parse");
                return;
            }
        };
        let changes: Vec<_> = garden_changes(&batch)
            .into_iter()
            .filter(|change| match change {
                IndexChange::Arrived { path } | IndexChange::Gone { path } => {
                    !self.is_desktop_path(path)
                }
                IndexChange::Moved { from, to } => {
                    !self.is_desktop_path(from) && !self.is_desktop_path(to)
                }
            })
            .collect();
        let config = self.config();
        let mut inner = self.inner.lock().expect("garden lock");
        let now = inner.ledger.clock.now(inner.monotonic_ms());
        // A cursor of 0 is a garden that has never read the log: take the position and show
        // nothing, so a first tending moment does not announce every file already on the disk
        // as news.
        let arrivals = if cursor == 0 {
            0
        } else {
            inner.ledger.ingest_changes(&changes, now, &config)
        };
        inner.ledger.change_cursor = batch.next_seq.max(cursor);
        self.persist(&mut inner);
        if arrivals > 0 {
            tracing::debug!(arrivals, "the garden noted new arrivals in earned places");
        }
    }

    /// The log was pruned past the cursor: take its head and forget what we can no longer prove.
    async fn rebase_change_cursor(&self, proxy: &templar_client::proxies::FilesIndexProxy<'_>) {
        let head = match proxy.roots().await {
            Ok(json) => serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|value| value["diagnostics"]["change_seq_head"].as_u64()),
            Err(error) => {
                tracing::debug!(%error, "could not rebase the garden's change cursor");
                None
            }
        };
        let Some(head) = head else { return };
        let mut inner = self.inner.lock().expect("garden lock");
        inner.ledger.change_cursor = head;
        for context in &mut inner.ledger.contexts {
            context.bloom = None;
        }
        self.persist(&mut inner);
        tracing::info!(
            head,
            "the index change log was pruned past the garden's cursor"
        );
    }

    fn tile_locations(&self, root: &Path) -> Vec<(String, templar_field::Located)> {
        let id = garden_tile_id(root);
        let outputs = homefield::outputs(&self.store).unwrap_or_default();
        outputs
            .into_iter()
            .filter_map(|output| {
                let doc = homefield::load(&self.store, &output).ok().flatten()?;
                doc.location(id).map(|located| (output, located))
            })
            .collect()
    }

    /// Take `root`'s tile off every Home it is on — the user said hide or remove, and a tile
    /// that stays is a control that did not do what it said. One undoable `HomeField1` change
    /// per output, announced so every view redraws.
    async fn remove_tile_everywhere(&self, root: &Path, verb: &str) {
        let id = garden_tile_id(root);
        for (output, _) in self.tile_locations(root) {
            match homefield::apply_op(&self.store, &output, &templar_field::FieldOp::Remove { id })
            {
                Ok(next) => {
                    self.record(format!(
                        "Home on {output}: {} {verb}.",
                        root.file_name().map_or_else(
                            || root.display().to_string(),
                            |n| n.to_string_lossy().into_owned()
                        )
                    ));
                    self.announce_field_change(&output, next.revision).await;
                }
                Err(error) => {
                    tracing::warn!(%error, root = %root.display(), output, "removing a place's tile");
                }
            }
        }
    }

    fn record(&self, message: String) {
        let event = Event {
            id: EventId::new(),
            at_ms: templar_state::now_ms(),
            kind: EventKind::SettingChange,
            app_id: None,
            instance_id: None,
            message,
            cause: Some("home-garden".into()),
            actions: Vec::new(),
            reversible: false,
        };
        if let Err(error) = self.history.append(event) {
            tracing::warn!(%error, "failed to record a Homegarden history event");
        }
    }

    async fn announce_field_change(&self, output: &str, revision: u64) {
        let Ok(interface) = self
            .connection
            .object_server()
            .interface::<_, crate::apis::homefield::HomeFieldApi>(names::HOME_FIELD_PATH)
            .await
        else {
            return;
        };
        crate::apis::homefield::announce_changed(interface.signal_emitter(), output, revision)
            .await;
    }

    fn describe_context(&self, root: &Path) -> Option<serde_json::Value> {
        let config = self.config();
        let inner = self.inner.lock().expect("garden lock");
        let now = inner.ledger.clock.now(inner.monotonic_ms());
        inner
            .ledger
            .context(root)
            .map(|context| context_json(context, now, &config))
    }
}

/// The first local file target among the broker's arguments. File URI decoding preserves
/// native bytes (including non-UTF-8 names) and rejects remote/malformed URI evidence.
fn launch_file(arguments: &[String]) -> Option<PathBuf> {
    arguments
        .iter()
        .filter_map(|argument| {
            if argument.starts_with('/') {
                Some(PathBuf::from(argument))
            } else {
                templar_types::uri_list::decode_uri(argument)
            }
        })
        .find(|path| path.is_file())
}

fn launch_directory(arguments: &[String]) -> Option<PathBuf> {
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        if matches!(
            argument.as_str(),
            "--directory" | "--working-directory" | "-d"
        ) {
            return iter.next().map(PathBuf::from).filter(|p| p.is_dir());
        }
        if let Some(value) = argument
            .strip_prefix("--directory=")
            .or_else(|| argument.strip_prefix("--working-directory="))
        {
            return Some(PathBuf::from(value)).filter(|p| p.is_dir());
        }
        if argument.starts_with('/') {
            let path = PathBuf::from(argument);
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    None
}

#[derive(Clone, serde::Deserialize)]
struct RecentFileV1 {
    #[serde(with = "templar_types::native_path")]
    path: PathBuf,
    app_id: Option<AppId>,
    at_ms: i64,
    #[serde(default)]
    action: String,
}

#[derive(serde::Serialize)]
struct NativePath<'a>(#[serde(with = "templar_types::native_path")] &'a Path);

struct NativeDirectories<'a>(&'a std::collections::BTreeMap<String, PathBuf>);
impl serde::Serialize for NativeDirectories<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        templar_types::native_path::map::serialize(self.0, serializer)
    }
}

#[derive(serde::Deserialize)]
struct SessionDirectory {
    #[serde(with = "templar_types::native_path")]
    cwd: PathBuf,
}

fn recent_file_presentation(file: &RecentFileV1) -> serde_json::Value {
    serde_json::json!({
        "path":NativePath(&file.path),"name":file.path.file_name().map(|name| name.to_string_lossy()),
        "category":file_category(file),"app_id":file.app_id,"modified_ms":file.at_ms
    })
}

fn file_category(file: &RecentFileV1) -> &'static str {
    let extension = file
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "kdenlive" | "mlt" => "video-project",
        "mp4" | "mkv" | "mov" | "webm"
            if matches!(file.action.as_str(), "saved" | "created" | "modified") =>
        {
            "render"
        }
        "mp4" | "mkv" | "mov" | "webm" => "footage",
        _ => match homegarden::Template::from_path(&file.path) {
            homegarden::Template::Coding => "source",
            homegarden::Template::Images => "image",
            homegarden::Template::Documents => "document",
            _ => "plain",
        },
    }
}

type ProcessEvidence = (
    templar_appd::identity::InstanceRecord,
    Vec<u32>,
    Vec<PathBuf>,
);

fn scan_instances(registry: &templar_appd::Registry) -> (Vec<ProcessEvidence>, Vec<String>) {
    use std::io::Read as _;
    fn members(group: &Path, depth: usize, pids: &mut Vec<u32>) -> anyhow::Result<()> {
        if depth > 4 || pids.len() >= 256 {
            return Ok(());
        }
        let mut text = String::new();
        std::fs::File::open(group.join("cgroup.procs"))?
            .take(32_768)
            .read_to_string(&mut text)?;
        pids.extend(
            text.lines()
                .filter_map(|pid| pid.parse::<u32>().ok())
                .take(256 - pids.len()),
        );
        for child in std::fs::read_dir(group)?.take(64) {
            let child = child?;
            if child.file_type()?.is_dir() {
                members(&child.path(), depth + 1, pids)?;
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    let mut errors = Vec::new();
    for record in registry.list_instances().into_iter().take(256) {
        if !record.has_managed_identity() {
            continue;
        }
        let group = Path::new("/sys/fs/cgroup").join(
            record
                .control_group
                .strip_prefix("/")
                .unwrap_or(&record.control_group),
        );
        let mut pids = Vec::new();
        if let Err(error) = members(&group, 0, &mut pids) {
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                errors.push(format!(
                    "{} process membership: {error}",
                    record.systemd_unit
                ));
            }
            continue;
        }
        pids.sort_unstable();
        pids.dedup();
        pids.retain(|pid| {
            registry
                .resolve_window(*pid, &templar_types::WindowClaims::default())
                .is_ok_and(|identity| {
                    identity.confidence.is_verified() && identity.instance_id == Some(record.id)
                })
        });
        if pids.is_empty() {
            continue;
        }
        let mut directories = std::collections::BTreeSet::new();
        for pid in &pids {
            let base = PathBuf::from(format!("/proc/{pid}"));
            if let Ok(directory) = std::fs::read_link(base.join("cwd")) {
                let mut candidates = vec![directory.clone()];
                let mut bytes = Vec::new();
                if let Ok(file) = std::fs::File::open(base.join("cmdline"))
                    && file.take(8192).read_to_end(&mut bytes).is_ok()
                {
                    for argument in bytes.split(|byte| *byte == 0).skip(1).take(32) {
                        if let Ok(argument) = std::str::from_utf8(argument)
                            && !argument.is_empty()
                            && !argument.starts_with('-')
                        {
                            let path = directory.join(argument);
                            if path.is_dir() {
                                candidates.push(path);
                            } else if path.is_file()
                                && let Some(parent) = path.parent()
                            {
                                candidates.push(parent.to_path_buf());
                            }
                        }
                    }
                }
                if registry
                    .resolve_window(*pid, &templar_types::WindowClaims::default())
                    .is_ok_and(|identity| {
                        identity.confidence.is_verified() && identity.instance_id == Some(record.id)
                    })
                {
                    for candidate in candidates {
                        if let Ok(path) = candidate.canonicalize() {
                            directories.insert(path);
                        }
                    }
                }
            }
        }
        result.push((record, pids, directories.into_iter().take(32).collect()));
    }
    (result, errors)
}

/// Cwd is established by Launcher1, never represented as a lossy terminal argv flag.
fn terminal_resume_arguments(
    manifest: &templar_types::AppManifest,
    argv: &[String],
) -> anyhow::Result<Vec<String>> {
    let executable = manifest
        .executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let mut arguments = match executable {
        "kitty" | "foot" => Vec::new(),
        "wezterm" => vec!["start".into(), "--always-new-process".into()],
        _ => {
            return Err(anyhow::anyhow!(
                "terminal provider has no declared new-instance resume argument form"
            ));
        }
    };
    if !argv.is_empty() {
        arguments.push("--".into());
        arguments.extend_from_slice(argv);
    }
    Ok(arguments)
}

#[derive(Default, serde::Serialize)]
struct TendReport {
    changed: u32,
    #[serde(default)]
    revision: u64,
    stages: Vec<String>,
    ops: Vec<(String, String)>,
}

/// How many change-log entries one tending moment reads. Bounded on purpose: a tending moment
/// sits on the path to a paint, and a page the garden cannot finish is a page the next tending
/// moment continues from the same cursor.
const CHANGE_PAGE: u32 = 512;

/// `EntryKind::Regular` as `Index1` puts it on the wire (`templar_file_index::model::EntryKind`).
const KIND_REGULAR: i64 = 0;

/// The D-Bus error `Index1` raises when the log has been pruned past the cursor.
const SNAPSHOT_REQUIRED: &str = "org.templaros.Files.Index1.Error.SnapshotRequired";

/// A page of `Files.Index1.ChangesSince`, reduced to the fields the garden reads. Deliberately
/// a local mirror of `templar_filed::index_host::wire_changes_batch` rather than the index crate
/// itself: the one policy daemon does not link the index to read four fields off a JSON page
/// (the same choice, for the same reason, as `cli/templar/src/files.rs`).
#[derive(serde::Deserialize)]
struct WireChanges {
    next_seq: u64,
    #[serde(default)]
    changes: Vec<WireChange>,
}

#[derive(serde::Deserialize)]
struct WireChange {
    op: String,
    path: WirePath,
    #[serde(default)]
    old_path: Option<WirePath>,
    #[serde(default)]
    kind: i64,
}

#[derive(serde::Deserialize)]
struct WirePath {
    display: String,
}

/// The path, or nothing when the index could not describe it in text.
///
/// `Index1` carries the authoritative bytes in `bytes_b64` beside this lossy `display` string,
/// and a file *action* must always use the bytes. The garden takes neither: it never opens a
/// bloomed path — it stores the name, compares it against the path an `Observe` carried, and
/// hands it to the shell as JSON. All three of those are UTF-8 only (`Observe(path: s)` is a
/// D-Bus string; `serde_json` cannot serialise a non-UTF-8 `PathBuf` at all), so a path that did
/// not survive the trip to text could never be matched or drawn anyway.
///
/// ponytail: skipping any name containing U+FFFD costs at most one missed bloom on a path no
/// other part of this feature could have represented. Decode `bytes_b64` if the garden ever has
/// to act on one.
fn faithful(path: &WirePath) -> Option<PathBuf> {
    (!path.display.contains('\u{FFFD}')).then(|| PathBuf::from(&path.display))
}

/// The change log as the ledger reads it: file arrivals, moves, removals. A `metadata` change
/// is not news, and a `rescan` marker names no file — one root was re-enumerated, which the
/// garden has nothing to say about.
fn garden_changes(batch: &WireChanges) -> Vec<IndexChange> {
    batch
        .changes
        .iter()
        .filter_map(|change| {
            let path = faithful(&change.path)?;
            match change.op.as_str() {
                "added" if change.kind == KIND_REGULAR => Some(IndexChange::Arrived { path }),
                "moved" => Some(IndexChange::Moved {
                    from: faithful(change.old_path.as_ref()?)?,
                    to: path,
                }),
                "removed" => Some(IndexChange::Gone { path }),
                _ => None,
            }
        })
        .collect()
}

fn is_snapshot_required(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _) if name.as_str() == SNAPSHOT_REQUIRED)
}

fn context_json(
    context: &homegarden::HomeContext,
    now: homegarden::GardenMoment,
    config: &GardenConfig,
) -> serde_json::Value {
    serde_json::json!({
        "root": context.root,
        "stage": context.stage.as_str(),
        "form": context.form.as_str(),
        "effective": context.effective(now, config),
        "score": context.score,
        "sessions_seen": context.sessions_seen(),
        "recent_sessions": context.recent_sessions(now),
        "breadth": context.breadth(now),
        "idle_sessions": now.sessions_since(context.last),
        "idle_garden_hours": now.hours_since(context.last),
        "roles": context.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        "tree": context.tree_at(now, config),
        "pinned": context.pinned,
        "kept": context.user_span.is_some(),
        "size_provenance": if context.manual_size { "manual" } else if context.user_span.is_some() { "legacy-unspecified" } else { "automatic" },
        "hidden": context.hidden,
        "landing": context.landing,
        "user_span": context.user_span,
        "material_seed": context.material_seed,
        "summary": context.summary(),
        "why": context.last_reason,
        "bloom_total": context.bloom.as_ref().map_or(0, |bloom| bloom.total),
        "bloomed": context.bloom.as_ref().map_or(&[][..], |bloom| bloom.items.as_slice()),
        "instances": context.instances.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "tile": garden_tile_id(&context.root).0,
    })
}

fn not_a_place(path: &Path) -> fdo::Error {
    fdo::Error::InvalidArgs(format!("{} is not a place", path.display()))
}

fn parse_path(path: &str) -> fdo::Result<PathBuf> {
    checked_path(PathBuf::from(path))
}

fn checked_path(path: PathBuf) -> fdo::Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(fdo::Error::InvalidArgs(
            "path must be absolute without parent traversal".into(),
        ));
    }
    Ok(path.canonicalize().unwrap_or(path))
}

#[zbus::interface(name = "org.templaros.HomeGarden1")]
impl HomeGardenApi {
    async fn observe(
        &self,
        kind: &str,
        path: &str,
        app_id: &str,
        role: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let kind = ActivityKind::parse(kind)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown activity kind {kind:?}")))?;
        let path = parse_path(path)?;
        let app_id = if app_id.is_empty() {
            None
        } else {
            Some(
                app_id
                    .parse::<AppId>()
                    .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?,
            )
        };
        let template_role = homegarden::Template::parse(role);
        let role = if role.is_empty() || template_role.is_some() {
            None
        } else {
            // Provider roles remain privileges owned by Providers1, not template categories.
            Some(
                ProviderRole::ALL
                    .into_iter()
                    .find(|r| r.as_str() == role)
                    .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown role {role:?}")))?,
            )
        };
        self.refresh_desktop_root().await;
        // Caller strings never establish identity. Cross-cgroup terminal ancestry is a
        // separate display-only path: it cannot record strong activity or grow a Place.
        let verified = if matches!(
            kind,
            ActivityKind::TerminalEntered | ActivityKind::ProjectEntered
        ) {
            Some(self.presentation_sender(&header).await?)
        } else if kind == ActivityKind::LaunchedHere {
            let identity = self.verified_sender(&header).await?;
            Some(PresentationSender {
                instance: identity.instance_id.expect("verified"),
                app: identity.app_id.expect("verified"),
                descendant: None,
            })
        } else {
            None
        };
        if let Some(observer) = &verified {
            observer.ensure_current(&self.apps)?;
        }
        let instance = verified.as_ref().map(|observer| observer.instance);
        let app_id = verified
            .as_ref()
            .map(|observer| observer.app.clone())
            .or(app_id);
        let role = if verified.is_some() { None } else { role };
        if verified
            .as_ref()
            .is_some_and(|observer| observer.descendant.is_some())
        {
            let mut inner = self.inner.lock().expect("garden lock");
            verified
                .as_ref()
                .expect("associated observer")
                .ensure_current(&self.apps)?;
            let instance = instance.expect("associated observer");
            record_descendant_directory(
                &mut inner.ledger,
                &path,
                instance,
                self.probe.as_ref(),
                &self.config(),
                self.is_desktop_path(&path),
            );
            self.persist(&mut inner);
        } else {
            self.observe_activity(kind, path.clone(), app_id, role, instance);
        }
        if let Some(instance) = instance {
            Self::context_changed(
                &emitter,
                instance.to_string(),
                path.to_string_lossy().into_owned(),
            )
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        }
        Ok(())
    }

    async fn tend(
        &self,
        output: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        if output.is_empty() {
            return Err(fdo::Error::InvalidArgs("output must not be empty".into()));
        }
        // One read of the shared index per tending moment, before the stages are recomputed,
        // so a place that grew this moment can already say what arrived in it.
        self.refresh_desktop_root().await;
        self.read_index_changes().await;
        self.collect_sources(None).await;
        self.read_recents().await;
        let report = self.tend_output(output).map_err(failed)?;
        if report.changed > 0 {
            self.announce_field_change(output, report.revision).await;
            let _ = emitter.tended(output.to_owned(), report.changed).await;
        }
        serde_json::to_string(&report).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn contexts(&self) -> fdo::Result<String> {
        let config = self.config();
        let inner = self.inner.lock().expect("garden lock");
        let now = inner.ledger.clock.now(inner.monotonic_ms());
        let list: Vec<serde_json::Value> = inner
            .ledger
            .contexts
            .iter()
            .map(|context| context_json(context, now, &config))
            .collect();
        serde_json::to_string(&list).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn working_directories(&self) -> fdo::Result<String> {
        let inner = self.inner.lock().expect("garden lock");
        serde_json::to_string(&NativeDirectories(&inner.ledger.working_directories))
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    /// Additive work-card snapshot; existing Contexts signatures and fields remain untouched.
    async fn presentation_v1(&self) -> fdo::Result<String> {
        self.read_recents().await;
        let config = self.config();
        let live = self.live_instances();
        let mut inner = self.inner.lock().expect("garden lock");
        inner.sessions.reconcile_instances(&live);
        let now = inner.ledger.clock.now(inner.monotonic_ms());
        let rows = inner.sessions.rows();
        let projects: Vec<_> = inner.ledger.contexts.iter().filter(|context| !context.hidden).map(|context| {
            let mut sessions: Vec<_> = rows.iter().filter(|row| {
                row.cwd.starts_with(&context.root)
                    && !inner.ledger.contexts.iter().any(|child| child.root != context.root
                        && child.root.starts_with(&context.root) && row.cwd.starts_with(&child.root))
            }).collect();
            let priority = |state: &str| match state { "waiting" => 0, "failed" => 1, "running" => 2, "idle" => 3, _ => 4 };
            sessions.sort_by(|a, b| priority(&a.state).cmp(&priority(&b.state))
                .then(b.updated_ms.cmp(&a.updated_ms)).then(a.id.cmp(&b.id)));
            let action = |row: &templar_session_core::session_catalogue::SessionRowV1| {
                if row.instance_id.as_deref().and_then(InstanceId::parse).is_some_and(|instance| live.contains(&instance)) { "Focus" }
                else if row.resume.is_some() || row.kind == "terminal" { "Resume" }
                else { "Open project" }
            };
            let instances: Vec<_> = context.instances.iter().filter(|instance| live.contains(*instance)).map(ToString::to_string).collect();
            let attention = if sessions.iter().any(|row| row.state == "waiting") { "needs-you" }
                else if sessions.iter().any(|row| row.state == "failed") { "failed" } else { "none" };
            let active = !instances.is_empty() || sessions.iter().any(|row| matches!(row.state.as_str(), "running" | "idle" | "waiting" | "failed"));
            let activity = if active { "active" } else if context.stage <= homegarden::Stage::Trace { "fading" } else { "resting" };
            let state_line = if let Some(row) = sessions.iter().find(|row| row.state == "waiting") {
                format!("{} waiting for you · {}", row.program, action(row))
            } else if let Some(row) = sessions.iter().find(|row| row.state == "failed") {
                format!("{} failed · {}", row.program, action(row))
            } else if let Some(row) = sessions.iter().find(|row| row.state == "running") {
                format!("{} running · {}", row.program, action(row))
            } else if let Some(row) = sessions.iter().find(|row| row.state == "idle") {
                format!("{} ready · {}", row.program, action(row))
            } else if let Some(row) = sessions.first() {
                format!("Resume: {}", row.subject)
            } else if active { "Work open · Focus".into() } else { "Open project to continue".into() };
            let files: Vec<_> = inner.recent_files.iter().filter(|file| file.path.starts_with(&context.root)
                && !inner.ledger.contexts.iter().any(|child| child.root != context.root
                    && child.root.starts_with(&context.root) && file.path.starts_with(&child.root)))
                .take(12).map(recent_file_presentation).collect();
            let app_ids: std::collections::BTreeSet<_> = context.instances.iter().filter_map(|instance| self.apps.instance(*instance).map(|record| record.app_id))
                .chain(context.used_apps.iter().cloned())
                .chain(inner.recent_files.iter().filter(|file| file.path.starts_with(&context.root)
                    && !inner.ledger.contexts.iter().any(|child| child.root != context.root
                        && child.root.starts_with(&context.root) && file.path.starts_with(&child.root)))
                    .filter_map(|file| file.app_id.clone())).collect();
            let apps: Vec<_> = app_ids.iter().filter_map(|app_id| self.apps.manifest(app_id).map(|manifest| serde_json::json!({
                "app_id":app_id,"name":manifest.display_name,
                "template":homegarden::Template::from_categories(&manifest.categories,self.role_of(app_id)).as_str()
            }))).collect();
            serde_json::json!({
                "root":NativePath(&context.root),"name":context.root.file_name().map(|name| name.to_string_lossy()),
                "template":context.presented_template().as_str(),
                "secondary_templates":context.secondary_templates(now,&config).iter().map(|template| template.as_str()).collect::<Vec<_>>(),
                "template_override":context.template_override.map(|template| template.as_str()),
                "activity":activity,"attention":attention,"state_line":state_line,"instances":instances,
                "sessions":sessions,"recent_files":files,"apps":apps
            })
        }).collect();
        Ok(serde_json::json!({"version":1,"projects":projects,"source_errors":inner.collection_errors}).to_string())
    }

    async fn observe_session_v1(
        &self,
        observation_json: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let observer = self.presentation_sender(&header).await?;
        let instance = observer.instance;
        let app = observer.app.clone();
        if observation_json.len() > 8192 {
            return Err(fdo::Error::InvalidArgs(
                "session observation too large".into(),
            ));
        }
        let payload: SessionDirectory = serde_json::from_str(observation_json)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let cwd = checked_path(payload.cwd)?;
        self.refresh_desktop_root().await;
        {
            let mut inner = self.inner.lock().expect("garden lock");
            observer.ensure_current(&self.apps)?;
            inner
                .sessions
                .observe(
                    instance,
                    &app,
                    observation_json,
                    templar_state::now_ms().max(0) as u64,
                )
                .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
            // Display state can support an existing project, but never creates or grows one.
            if observer.descendant.is_some() {
                record_descendant_directory(
                    &mut inner.ledger,
                    &cwd,
                    instance,
                    self.probe.as_ref(),
                    &self.config(),
                    self.is_desktop_path(&cwd),
                );
            } else {
                inner
                    .ledger
                    .working_directories
                    .insert(instance.to_string(), cwd.clone());
                if !self.is_desktop_path(&cwd) {
                    inner.ledger.associate_instance(
                        &cwd,
                        instance,
                        self.probe.as_ref(),
                        &self.config(),
                    );
                }
            }
            self.persist(&mut inner);
        }
        Self::context_changed(
            &emitter,
            instance.to_string(),
            cwd.to_string_lossy().into_owned(),
        )
        .await
        .map_err(failed)?;
        Ok(())
    }

    async fn sessions_v1(&self) -> fdo::Result<String> {
        let live = self.live_instances();
        let mut inner = self.inner.lock().expect("garden lock");
        inner.sessions.reconcile_instances(&live);
        serde_json::to_string(&inner.sessions.rows()).map_err(failed)
    }

    async fn activate_instance_v1(&self, instance_id: &str) -> fdo::Result<()> {
        let instance = InstanceId::parse(instance_id)
            .ok_or_else(|| fdo::Error::InvalidArgs("invalid instance".into()))?;
        if !self.live_instances().contains(&instance) {
            return Err(fdo::Error::InvalidArgs(
                "unknown or inactive managed instance".into(),
            ));
        }
        self.collect_sources(Some(instance)).await;
        Ok(())
    }

    async fn show_as_v1(&self, path: &str, template: &str) -> fdo::Result<()> {
        let path = parse_path(path)?;
        let template = if template.is_empty() {
            None
        } else {
            Some(
                homegarden::Template::parse(template)
                    .ok_or_else(|| fdo::Error::InvalidArgs("unknown template".into()))?,
            )
        };
        let mut inner = self.inner.lock().expect("garden lock");
        inner
            .ledger
            .context_mut(&path)
            .ok_or_else(|| not_a_place(&path))?
            .template_override = template;
        self.persist(&mut inner);
        Ok(())
    }

    async fn resume_session_v1(&self, path: &str, session_id: &str) -> fdo::Result<String> {
        let path = parse_path(path)?;
        let request = {
            let inner = self.inner.lock().expect("garden lock");
            if inner.ledger.context(&path).is_none() {
                return Err(not_a_place(&path));
            }
            let request = inner
                .sessions
                .resume(session_id)
                .ok_or_else(|| fdo::Error::InvalidArgs("unknown or unsupported session".into()))?;
            if inner.ledger.contexts.iter().any(|child| {
                child.root != path
                    && child.root.starts_with(&path)
                    && request.directory.starts_with(&child.root)
            }) {
                return Err(fdo::Error::InvalidArgs(
                    "session belongs to a child project".into(),
                ));
            }
            request
        };
        if !request.directory.starts_with(&path) || !request.directory.is_dir() {
            return Err(fdo::Error::InvalidArgs(
                "session is not resumable in this project".into(),
            ));
        }
        let terminal = self
            .providers
            .read()
            .provider(ProviderRole::Terminal)
            .ok_or_else(|| fdo::Error::Failed("no terminal provider selected".into()))?;
        let manifest = self
            .apps
            .manifest(&terminal)
            .ok_or_else(|| fdo::Error::Failed("terminal provider not installed".into()))?;
        let arguments = terminal_resume_arguments(&manifest, &request.argv).map_err(failed)?;
        let launcher = templar_client::proxies::LauncherProxy::new(&self.connection)
            .await
            .map_err(failed)?;
        launcher
            .launch_in_directory_v1(
                &terminal.to_string(),
                arguments,
                &templar_types::uri_list::encode_path(&request.directory),
                "",
            )
            .await
            .map_err(failed)
    }

    async fn launch_app_in_place_v1(&self, path: &str, app_id: &str) -> fdo::Result<String> {
        let path = parse_path(path)?;
        {
            let inner = self.inner.lock().expect("garden lock");
            if inner.ledger.context(&path).is_none() {
                return Err(not_a_place(&path));
            }
        }
        let app: AppId = app_id.parse().map_err(|error: templar_types::AppIdError| {
            fdo::Error::InvalidArgs(error.to_string())
        })?;
        if self.apps.manifest(&app).is_none() {
            return Err(fdo::Error::FileNotFound(
                "application is not installed".into(),
            ));
        }
        let launcher = templar_client::proxies::LauncherProxy::new(&self.connection)
            .await
            .map_err(failed)?;
        launcher
            .launch_in_directory_v1(
                app.as_str(),
                Vec::new(),
                path.to_str().ok_or_else(|| {
                    fdo::Error::InvalidArgs("project directory is not UTF-8".into())
                })?,
                "",
            )
            .await
            .map_err(failed)
    }

    async fn describe(&self, path: &str) -> fdo::Result<String> {
        let path = parse_path(path)?;
        match self.describe_context(&path) {
            Some(value) => {
                serde_json::to_string(&value).map_err(|error| fdo::Error::Failed(error.to_string()))
            }
            None => Ok(String::new()),
        }
    }

    async fn pin(&self, path: &str, pinned: bool) -> fdo::Result<()> {
        let path = parse_path(path)?;
        let mut inner = self.inner.lock().expect("garden lock");
        if !inner.ledger.set_pinned(&path, pinned) {
            return Err(not_a_place(&path));
        }
        self.persist(&mut inner);
        Ok(())
    }

    async fn keep(&self, path: &str) -> fdo::Result<()> {
        let path = parse_path(path)?;
        // Keep establishes permanence, not a manually chosen geometry.
        let mut inner = self.inner.lock().expect("garden lock");
        let known = inner.ledger.set_pinned(&path, true);
        if !known {
            return Err(not_a_place(&path));
        }
        self.persist(&mut inner);
        Ok(())
    }

    async fn hide(&self, path: &str, hidden: bool) -> fdo::Result<()> {
        let path = parse_path(path)?;
        {
            let mut inner = self.inner.lock().expect("garden lock");
            if !inner.ledger.set_hidden(&path, hidden) {
                return Err(not_a_place(&path));
            }
            self.persist(&mut inner);
        }
        if hidden {
            self.remove_tile_everywhere(&path, "hidden").await;
        }
        Ok(())
    }

    async fn forget(&self, path: &str) -> fdo::Result<()> {
        let path = parse_path(path)?;
        {
            let mut inner = self.inner.lock().expect("garden lock");
            inner.ledger.forget(&path);
            self.persist(&mut inner);
        }
        self.remove_tile_everywhere(&path, "removed").await;
        Ok(())
    }

    async fn adopt_window(&self, path: &str, instance_id: &str) -> fdo::Result<()> {
        let path = parse_path(path)?;
        let Some(instance) = InstanceId::parse(instance_id) else {
            return Err(fdo::Error::InvalidArgs(format!(
                "{instance_id:?} is not an instance id"
            )));
        };
        let mut inner = self.inner.lock().expect("garden lock");
        if !self.apps.contains_instance(instance) {
            return Err(fdo::Error::InvalidArgs(
                "window has no live managed instance".into(),
            ));
        }
        if !inner.ledger.adopt_instance(&path, instance) {
            return Err(not_a_place(&path));
        }
        self.persist(&mut inner);
        tracing::info!(root = %path.display(), "garden adopted a window");
        Ok(())
    }

    async fn forget_all(&self) -> fdo::Result<()> {
        let mut inner = self.inner.lock().expect("garden lock");
        inner.ledger.forget_all();
        self.persist(&mut inner);
        Ok(())
    }

    async fn clock(&self) -> fdo::Result<String> {
        let inner = self.inner.lock().expect("garden lock");
        let now = inner.ledger.clock.now(inner.monotonic_ms());
        Ok(serde_json::json!({
            "session": now.session,
            "active_ms": now.active_ms,
            "active": inner.ledger.clock.is_active(),
        })
        .to_string())
    }

    #[zbus(signal)]
    async fn context_changed(
        signal_emitter: &SignalEmitter<'_>,
        instance_id: String,
        path: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn tended(
        signal_emitter: &SignalEmitter<'_>,
        output: String,
        changed: u32,
    ) -> zbus::Result<()>;
}

/// Subscribe the garden to the session edges (the Garden Clock) and to launches (`LaunchedHere`).
/// Both channels are subscribed before anything is sent on them (see `main.rs`).
pub fn spawn_sources(
    api: HomeGardenApi,
    edges: &SessionEdges,
    launches: &tokio::sync::broadcast::Sender<templar_appd::dbus::LaunchEvent>,
) {
    let mut edges = edges.subscribe();
    let for_edges = api.clone();
    tokio::spawn(async move {
        loop {
            match edges.recv().await {
                Ok(edge) => for_edges.on_edge(edge),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    let mut launches = launches.subscribe();
    tokio::spawn(async move {
        api.refresh_desktop_root().await;
        loop {
            match launches.recv().await {
                Ok(event) => {
                    api.observe_launch(&event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "HomeGarden missed broker launch evidence; next collection can recover live process cwd"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// What the ledger may ask about the filesystem, answered with `stat`-class reads only.
struct DaemonProbe {
    store: Store,
    home: PathBuf,
    xdg_dirs: Vec<PathBuf>,
}

const WORKSPACE_MARKERS: &[&str] = &[
    ".git",
    ".hg",
    ".jj",
    ".svn",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "Makefile",
    "justfile",
];

/// A real repository root: a `.git` directory that has a `HEAD` (an empty `.git` left in a
/// projects folder is not one), a worktree's or submodule's `.git` file, or another VCS's
/// directory.
fn is_repository(path: &Path) -> bool {
    let git = path.join(".git");
    git.join("HEAD").is_file()
        || git.is_file()
        || [".hg", ".jj", ".svn"]
            .iter()
            .any(|marker| path.join(marker).is_dir())
}

impl DaemonProbe {
    fn new(store: Store) -> Self {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let home = home.canonicalize().unwrap_or(home);
        let xdg_dirs = user_dirs(&home);
        Self {
            store,
            home,
            xdg_dirs,
        }
    }
}

fn config_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from)
}

/// `~/.config/user-dirs.dirs`, the file `xdg-user-dirs` writes; the English defaults when absent.
fn user_dirs(home: &Path) -> Vec<PathBuf> {
    user_dirs_from(home, &config_dir(home))
}

fn user_dirs_from(home: &Path, config_dir: &Path) -> Vec<PathBuf> {
    let config = config_dir.join("user-dirs.dirs");
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(config) {
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if !key.starts_with("XDG_") || !key.ends_with("_DIR") {
                continue;
            }
            let value = value.trim().trim_matches('"');
            let path = if let Some(rest) = value.strip_prefix("$HOME/") {
                home.join(rest)
            } else if value == "$HOME" {
                continue;
            } else {
                PathBuf::from(value)
            };
            dirs.push(path);
        }
    }
    if dirs.is_empty() {
        for name in [
            "Desktop",
            "Downloads",
            "Documents",
            "Pictures",
            "Music",
            "Videos",
            "Templates",
            "Public",
        ] {
            dirs.push(home.join(name));
        }
    }
    dirs
}

impl RootProbe for DaemonProbe {
    fn project_root(&self, path: &Path) -> Option<PathBuf> {
        let projects = templar_session_core::projects::load_all(&self.store).ok()?;
        projects
            .into_iter()
            .map(|project| project.root)
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
    }

    fn has_workspace_marker(&self, path: &Path) -> bool {
        WORKSPACE_MARKERS
            .iter()
            .any(|marker| path.join(marker).exists())
    }

    fn has_vcs_marker(&self, path: &Path) -> bool {
        is_repository(path)
    }

    fn is_mount_point(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        let Some(parent) = path.parent() else {
            return true;
        };
        match (std::fs::metadata(path), std::fs::metadata(parent)) {
            (Ok(own), Ok(up)) => own.dev() != up.dev(),
            _ => false,
        }
    }

    fn is_xdg_user_dir(&self, path: &Path) -> bool {
        self.xdg_dirs.iter().any(|dir| dir == path)
    }

    fn home(&self) -> &Path {
        &self.home
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_observer_numeric_pid_without_bus_lifetime_proof_fails_closed() {
        let uid = process_context::session_uid().unwrap();
        let credentials = fdo::ConnectionCredentials::default()
            .set_unix_user_id(uid)
            .set_process_id(std::process::id());
        let error = descendant_peer_pid(&credentials, uid).unwrap_err();
        assert!(matches!(error, fdo::Error::AccessDenied(_)));
    }

    #[test]
    fn descendant_context_is_display_only_even_after_a_prior_verified_prompt() {
        let temp = tempfile::Builder::new()
            .prefix("templaros-descendant-")
            .tempdir()
            .unwrap();
        let project = temp.path().join("project");
        let unrelated = temp.path().join("unrelated");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        let probe = DaemonProbe::new(Store::in_memory().unwrap());
        let config = GardenConfig {
            ignore: Vec::new(),
            ..GardenConfig::default()
        };
        let instance = InstanceId::new();
        let mut ledger = Ledger::default();
        ledger.contexts.push(
            serde_json::from_value(serde_json::json!({
                "root":project,"score":30.0,"last":{"session":1,"active_ms":0},
                "stage":"expanded","material_seed":7
            }))
            .unwrap(),
        );
        ledger.prompt_directories.insert(instance.to_string());
        record_descendant_directory(&mut ledger, &project, instance, &probe, &config, false);
        assert_eq!(ledger.contexts.len(), 1);
        assert_eq!(ledger.contexts[0].score, 30.0);
        assert!(ledger.contexts[0].instances.contains(&instance));
        assert!(!ledger.prompt_directories.contains(&instance.to_string()));
        record_descendant_directory(&mut ledger, &unrelated, instance, &probe, &config, false);
        assert_eq!(ledger.contexts.len(), 1);
        assert_eq!(ledger.contexts[0].score, 30.0);
        assert!(!ledger.contexts[0].instances.contains(&instance));
        assert_eq!(ledger.working_directories[&instance.to_string()], unrelated);
    }

    /// The writer keeps the newest of a burst and a flush answers only once it is stored, so an
    /// exiting daemon neither loses its last acts nor lets an older snapshot win.
    #[test]
    fn the_writer_stores_the_newest_save_before_answering_a_flush() {
        let store = Store::in_memory().unwrap();
        let writer = spawn_writer(store.clone());
        let mut ledger = Ledger::default();
        for session in 1..=5 {
            ledger.clock.session = session;
            writer
                .send(Write::Save(serde_json::to_string(&ledger).unwrap()))
                .unwrap();
        }
        let (ack, done) = mpsc::channel();
        writer.send(Write::Flush(ack)).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(homegarden::load(&store).clock.session, 5);
    }

    #[test]
    fn native_session_and_recent_producers_preserve_identity_and_terminal_resume_argv() {
        use std::os::unix::ffi::OsStringExt;
        let cwd = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/project/native-\xff".to_vec(),
        ));
        let observation = serde_json::json!({
            "schema_version":1,"adapter":"claude-code","event":"stop","session_id":"safe-id",
            "cwd":NativePath(&cwd)
        })
        .to_string();
        let directory: SessionDirectory = serde_json::from_str(&observation).unwrap();
        assert_eq!(checked_path(directory.cwd).unwrap(), cwd);
        let mut catalogue = Catalogue::default();
        catalogue
            .observe(
                InstanceId::new(),
                &"org.example.Terminal".parse().unwrap(),
                &observation,
                42,
            )
            .unwrap();
        let file: RecentFileV1 = serde_json::from_value(serde_json::json!({
            "path":NativePath(&cwd.join("notes.txt")),"app_id":null,"at_ms":42,"action":"opened"
        }))
        .unwrap();
        let snapshot = serde_json::json!({
            "sessions":catalogue.rows(),"recent_files":[recent_file_presentation(&file)]
        });
        assert!(snapshot["sessions"][0]["cwd"]["path_b64"].is_string());
        assert!(snapshot["recent_files"][0]["path"]["path_b64"].is_string());
        assert!(snapshot["recent_files"][0]["app_id"].is_null());
        assert_eq!(snapshot["recent_files"][0]["name"], "notes.txt");
        let request = catalogue.resume(&catalogue.rows()[0].id).unwrap();
        assert_eq!(request.directory, cwd);
        assert_eq!(
            templar_types::uri_list::decode_uri(&templar_types::uri_list::encode_path(
                &request.directory
            )),
            Some(cwd)
        );
        for executable in ["kitty", "foot", "wezterm"] {
            let manifest: templar_types::AppManifest = serde_json::from_value(serde_json::json!({
                "id":"org.example.Terminal","display_name":"Terminal",
                "primary_desktop_file_id":"terminal.desktop","executable":format!("/usr/bin/{executable}"),
                "resource_class":templar_types::ResourceClass::Interactive
            })).unwrap();
            let prefix: &[&str] = if executable == "wezterm" {
                &["start", "--always-new-process"]
            } else {
                &[]
            };
            assert_eq!(terminal_resume_arguments(&manifest, &[]).unwrap(), prefix);
            let mut expected: Vec<String> = prefix
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect();
            expected.push("--".into());
            expected.extend_from_slice(&request.argv);
            assert_eq!(
                terminal_resume_arguments(&manifest, &request.argv).unwrap(),
                expected
            );
        }
    }

    /// An empty `.git` in a projects folder is not a repository; a real one, or a worktree's
    /// `.git` file, is.
    #[test]
    fn only_a_real_repository_is_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let stray = tmp.path().join("projects");
        std::fs::create_dir_all(stray.join(".git")).unwrap();
        assert!(!is_repository(&stray));
        let repo = stray.join("templaros");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(is_repository(&repo));
        let worktree = stray.join("templaros-iso");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../templaros/.git/worktrees/iso\n",
        )
        .unwrap();
        assert!(is_repository(&worktree));
    }

    #[test]
    fn a_launch_directory_is_read_from_the_common_argument_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_string_lossy().into_owned();
        let file = tmp.path().join("f");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            launch_directory(&["--directory".into(), dir.clone()]),
            Some(tmp.path().to_path_buf())
        );
        assert_eq!(
            launch_directory(&[format!("--working-directory={dir}")]),
            Some(tmp.path().to_path_buf())
        );
        assert_eq!(
            launch_directory(std::slice::from_ref(&dir)),
            Some(tmp.path().to_path_buf())
        );
        // A file argument is not a directory launch; the garden hears about opens elsewhere.
        assert_eq!(
            launch_directory(&[file.to_string_lossy().into_owned()]),
            None
        );
        assert_eq!(launch_directory(&["--new-window".into()]), None);
    }

    #[test]
    fn broker_file_uri_evidence_preserves_native_bytes_and_desktop_lexical_origin() {
        use std::os::unix::{ffi::OsStringExt as _, fs::symlink};
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp
            .path()
            .join(std::ffi::OsString::from_vec(b"name-\xff.pdf".to_vec()));
        std::fs::write(&file, "native").unwrap();
        let uri = templar_types::uri_list::encode_path(&file);
        assert_eq!(launch_file(&[uri]), Some(file.clone()));
        assert_eq!(launch_file(&["file://remote/tmp/file.pdf".into()]), None);
        let desktop = tmp.path().join("Desktop");
        std::fs::create_dir(&desktop).unwrap();
        let staged = desktop.join("linked.pdf");
        symlink(&file, &staged).unwrap();
        assert_eq!(
            launch_file(&[templar_types::uri_list::encode_path(&staged)]),
            Some(staged)
        );
    }

    /// The ordering every recording and every write uses: fold the open interval in, *then* read
    /// the moment. Before this, `pause` was reached only from a lock or a sleep edge and a
    /// logout emits neither, so seven real sessions persisted `active_ms: 0` while the running
    /// daemon's own `now()` — which adds the open interval — looked perfectly healthy.
    #[test]
    fn advancing_folds_the_open_interval_in_before_the_moment_is_read() {
        let mut clock = homegarden::GardenClock::default();
        clock.start_session(0);
        let at = advance(&mut clock, 3_600_000);
        assert_eq!(at.active_ms, 3_600_000);
        assert_eq!(
            clock.active_ms, at.active_ms,
            "the stamped moment is exactly what a write would store"
        );

        // The session ends here — no lock, no sleep, no edge. What was folded in survives.
        let json = serde_json::to_string(&clock).unwrap();
        let mut next: homegarden::GardenClock = serde_json::from_str(&json).unwrap();
        assert_eq!(next.active_ms, 3_600_000);
        next.start_session(0);
        assert_eq!(advance(&mut next, 60_000).active_ms, 3_660_000);
    }

    #[test]
    fn desktop_exclusion_keeps_lexical_binding_and_canonical_identity() {
        use std::os::unix::{ffi::OsStringExt as _, fs::symlink};
        let tmp = tempfile::tempdir().unwrap();
        let actual = tmp.path().join("staging");
        std::fs::create_dir(&actual).unwrap();
        let lexical = tmp
            .path()
            .join(std::ffi::OsString::from_vec(b"Desktop-\xff".to_vec()));
        symlink(&actual, &lexical).unwrap();
        let desktop = DesktopRoot {
            path: lexical.clone(),
            identity: lexical.canonicalize().unwrap(),
        };
        assert_eq!(desktop.path, lexical);
        assert!(desktop.contains(&desktop.path.join("a.txt")));
        assert!(desktop.contains(&actual.join("a.txt")));
        assert!(!desktop.contains(&tmp.path().join("staging-other/a.txt")));
        assert!(!desktop.contains(tmp.path()));
    }

    #[test]
    fn user_dirs_are_parsed_from_the_xdg_file_with_english_defaults_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config = home.join(".config");
        assert!(user_dirs_from(home, &config).contains(&home.join("Downloads")));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("user-dirs.dirs"),
            "XDG_DOWNLOAD_DIR=\"$HOME/dl\"\nXDG_DESKTOP_DIR=\"$HOME\"\n# comment\n",
        )
        .unwrap();
        assert_eq!(user_dirs_from(home, &config), vec![home.join("dl")]);
    }

    /// One page of `Index1.ChangesSince` exactly as `wire_changes_batch` writes it.
    fn page() -> &'static str {
        r#"{"generation":9,"next_seq":412,"more":false,"changes":[
            {"seq":409,"generation":9,"root":1,"op":"added","entry_id":7,
             "path":{"bytes_b64":"eA==","display":"/home/dk/Downloads/a.mp4"},
             "old_path":null,"kind":0,"size_bytes":10,"mtime_ns":0,"at_ms":1788270784771},
            {"seq":410,"generation":9,"root":1,"op":"added","entry_id":8,
             "path":{"bytes_b64":"eQ==","display":"/home/dk/Downloads/sub"},
             "old_path":null,"kind":1,"size_bytes":0,"mtime_ns":0,"at_ms":1788270784772},
            {"seq":411,"generation":9,"root":1,"op":"moved","entry_id":9,
             "path":{"bytes_b64":"eg==","display":"/home/dk/src/templar-os"},
             "old_path":{"bytes_b64":"dw==","display":"/home/dk/src/templaros"},
             "kind":1,"size_bytes":0,"mtime_ns":0,"at_ms":1788270784773},
            {"seq":412,"generation":9,"root":1,"op":"metadata","entry_id":10,
             "path":{"bytes_b64":"dg==","display":"/home/dk/Downloads/a.mp4"},
             "old_path":null,"kind":0,"size_bytes":11,"mtime_ns":0,"at_ms":1788270784774}]}"#
    }

    #[test]
    fn the_change_page_becomes_arrivals_moves_and_nothing_else() {
        let batch: WireChanges = serde_json::from_str(page()).expect("the wire page parses");
        assert_eq!(batch.next_seq, 412);
        let changes = garden_changes(&batch);
        assert_eq!(
            changes,
            vec![
                IndexChange::Arrived {
                    path: PathBuf::from("/home/dk/Downloads/a.mp4")
                },
                IndexChange::Moved {
                    from: PathBuf::from("/home/dk/src/templaros"),
                    to: PathBuf::from("/home/dk/src/templar-os"),
                },
            ],
            "a directory arrival is not news, and a metadata change is not either"
        );
    }

    #[test]
    fn a_removal_is_read_and_a_rescan_marker_names_no_file() {
        let batch: WireChanges = serde_json::from_str(
            r#"{"next_seq":3,"changes":[
                {"op":"removed","path":{"display":"/home/dk/Downloads/a.mp4"},"kind":0},
                {"op":"rescan","path":{"display":"/home/dk"},"kind":1}]}"#,
        )
        .unwrap();
        assert_eq!(
            garden_changes(&batch),
            vec![IndexChange::Gone {
                path: PathBuf::from("/home/dk/Downloads/a.mp4")
            }]
        );
    }

    #[test]
    fn a_name_that_did_not_survive_the_trip_to_text_is_skipped() {
        let batch: WireChanges = serde_json::from_str(
            r#"{"next_seq":2,"changes":[
                {"op":"added","path":{"display":"/home/dk/Downloads/b\uFFFDd.mp4"},"kind":0},
                {"op":"added","path":{"display":"/home/dk/Downloads/good.mp4"},"kind":0}]}"#,
        )
        .unwrap();
        assert_eq!(
            garden_changes(&batch),
            vec![IndexChange::Arrived {
                path: PathBuf::from("/home/dk/Downloads/good.mp4")
            }]
        );
    }

    #[test]
    fn only_the_pruned_log_error_rebases_the_cursor() {
        let pruned = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(SNAPSHOT_REQUIRED).unwrap(),
            None,
            zbus::message::Message::method_call("/", "X")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert!(is_snapshot_required(&pruned));
        let other = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("org.freedesktop.DBus.Error.Failed").unwrap(),
            None,
            zbus::message::Message::method_call("/", "X")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert!(!is_snapshot_required(&other));
    }

    #[test]
    fn a_context_reports_its_bloom_and_names_the_clock_its_idle_hours_are_on() {
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        ledger.contexts.push(
            serde_json::from_str(
                r#"{"root":"/home/dk/Downloads","score":30.0,
                    "last":{"session":1,"active_ms":0},"stage":"expanded","material_seed":7}"#,
            )
            .expect("an earned place"),
        );
        ledger.ingest_changes(
            &[IndexChange::Arrived {
                path: PathBuf::from("/home/dk/Downloads/a.mp4"),
            }],
            homegarden::GardenMoment::default(),
            &config,
        );
        let json = context_json(
            &ledger.contexts[0],
            homegarden::GardenMoment::default(),
            &config,
        );
        assert_eq!(json["bloom_total"], 1);
        assert_eq!(json["bloomed"][0], "/home/dk/Downloads/a.mp4");
        assert!(
            json.get("idle_hours").is_none(),
            "the wall-clock name is gone"
        );
        assert!(json.get("idle_garden_hours").is_some());
    }
}
