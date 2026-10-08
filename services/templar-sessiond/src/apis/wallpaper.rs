//! Session-owned wallpaper policy. One monotonic deadline, one cancellable worker,
//! latest pending generation; neither D-Bus nor this process carries decoded pixels.
use anyhow::{Context, Result, bail, ensure};
use parking_lot::RwLock;
use rusqlite::OptionalExtension;
use std::{collections::BTreeMap, ffi::OsString, fs::File, io::{Seek, SeekFrom}, os::{fd::{AsFd, AsRawFd}, unix::ffi::{OsStrExt, OsStringExt}}, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use templar_client::compositor_control::{CompositorControl, ActivitySubscription, WallpaperSubscription};
use templar_session_core::settings::{self, SettingsSchema, SettingsValues};
use templar_state::Store;
use templar_types::{compositor_control::{ActivityObservation, ControlRequest, ControlResponse}, dbus::session as names, display::DisplayId, settings::Revision, wallpaper::{RasterDescriptor, WallpaperConfig, WallpaperImage, WallpaperObservation, WallpaperStatus}};
use templar_wallpaper::{raster, selection::{Direction, SourceSelector}};
use tokio::{io::AsyncReadExt, sync::{mpsc, oneshot, watch}, time::Instant};
use zbus::fdo;
use super::lifecycle::{SessionEdge, SessionEdges};

const LAST_SUCCESS: &str = "wallpaper.runtime.last-success.v1";
const WORKER_TIMEOUT: Duration = Duration::from_secs(60);
const METADATA_LIMIT: u64 = 16_384;
const HISTORY_LIMIT: usize = 64;

#[derive(Clone, Debug)]
pub struct WallpaperHost {
    sender: mpsc::Sender<Message>,
    status: watch::Receiver<WallpaperStatus>,
    schema: Arc<RwLock<SettingsSchema>>,
    values: Arc<RwLock<SettingsValues>>,
}
#[derive(Clone, Copy, Debug)]
enum Action { Next, Previous, Pause, Resume, Refresh }
#[derive(Debug)]
enum Message {
    Policy(Result<(bool, WallpaperConfig), String>, bool, oneshot::Sender<()>),
    Action(Action, oneshot::Sender<WallpaperStatus>),
    Edge(SessionEdge),
    Stop(oneshot::Sender<()>),
}
#[derive(Clone, Copy, Debug)]
enum Selection { Restore, Refresh, Next, Previous }
#[derive(Clone, Debug)]
struct Job { revision: Revision, config: WallpaperConfig, previous: Option<Vec<u8>>, historical_previous: Option<Vec<u8>>, selection: Selection, display_revision: Revision, outputs: Vec<DisplayId> }
struct Submitted { job: Job, source: Vec<u8>, observation: WallpaperObservation }
struct Active { selection: Selection, cancel: watch::Sender<bool>, task: tokio::task::JoinHandle<Result<Submitted>> }
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SuccessfulSources { current: Option<Vec<u8>>, previous: Vec<Vec<u8>> }

impl WallpaperHost {
    pub fn new(store: Store, schema: Arc<RwLock<SettingsSchema>>, values: Arc<RwLock<SettingsValues>>, fixture_control: Option<CompositorControl>, spool_directory: PathBuf) -> Self {
        let initial = settings::wallpaper_policy(&schema.read(), &values.read());
        let (enabled, config, error) = match initial {
            Ok((enabled, config)) => (enabled, config, None),
            Err(error) => {
                let schema = schema.read();
                let enabled = matches!(values.read().effective(&schema, settings::WALLPAPER_ENABLED_KEY), Some(settings::SettingValue::Bool(true)));
                (enabled, WallpaperConfig::default(), Some(format!("Stored wallpaper policy: {error:#}")))
            },
        };
        let sources = load_sources(&store).unwrap_or_else(|error| { tracing::warn!(%error, "wallpaper successful-source record unavailable"); SuccessfulSources::default() });
        let status = WallpaperStatus { revision: Revision::new_owner(), enabled, config, paused: false, source: sources.current.clone(), pending: false, applied: None, error };
        let (published, status) = watch::channel(status);
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(Runtime::new(store, published, sources, fixture_control, spool_directory).run(receiver));
        Self { sender, status, schema, values }
    }
    pub fn affects(key: &str) -> bool { matches!(key, settings::WALLPAPER_ENABLED_KEY | settings::WALLPAPER_CONFIG_KEY) }
    pub async fn reconcile(&self) -> Result<()> {
        let (policy, enabled) = {
            let schema = self.schema.read();
            let values = self.values.read();
            let enabled = matches!(values.effective(&schema, settings::WALLPAPER_ENABLED_KEY), Some(settings::SettingValue::Bool(true)));
            (settings::wallpaper_policy(&schema, &values).map_err(|error| format!("{error:#}")), enabled)
        };
        let (sent, received) = oneshot::channel();
        self.sender.send(Message::Policy(policy, enabled, sent)).await.context("wallpaper runtime stopped")?;
        received.await.context("wallpaper reconcile owner stopped")
    }
    pub async fn shutdown(&self) {
        let (sent, received) = oneshot::channel();
        if self.sender.send(Message::Stop(sent)).await.is_ok() { let _ = received.await; }
    }
    async fn action(&self, action: Action) -> fdo::Result<String> {
        let (sent, received) = oneshot::channel();
        self.sender.send(Message::Action(action, sent)).await.map_err(super::failed)?;
        let status = received.await.map_err(super::failed)?;
        serde_json::to_string(&status).map_err(super::failed)
    }
    pub fn spawn_sources(&self, connection: zbus::Connection, edges: &SessionEdges) {
        let mut edges = edges.subscribe();
        let sender = self.sender.clone();
        tokio::spawn(async move {
            loop {
                match edges.recv().await {
                    Ok(edge) => if sender.send(Message::Edge(edge)).await.is_err() { break; },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { let _ = sender.send(Message::Edge(SessionEdge::Sleeping(true))).await; },
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let mut status = self.status.clone();
        tokio::spawn(async move {
            while status.changed().await.is_ok() {
                let json = serde_json::to_string(&*status.borrow_and_update());
                if let Ok(json) = json {
                    let _ = connection.emit_signal(None::<&str>, names::WALLPAPER_PATH, names::WALLPAPER_IFACE, "Changed", &(json,)).await;
                }
            }
        });
    }
}
#[zbus::interface(name = "org.templaros.Wallpaper1")]
impl WallpaperHost {
    async fn inspect(&self) -> fdo::Result<String> { serde_json::to_string(&*self.status.borrow()).map_err(super::failed) }
    async fn next(&self) -> fdo::Result<String> { self.action(Action::Next).await }
    async fn previous(&self) -> fdo::Result<String> { self.action(Action::Previous).await }
    async fn pause(&self) -> fdo::Result<String> { self.action(Action::Pause).await }
    async fn resume(&self) -> fdo::Result<String> { self.action(Action::Resume).await }
    async fn refresh(&self) -> fdo::Result<String> { self.action(Action::Refresh).await }
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>, json: &str) -> zbus::Result<()>;
}

struct Runtime {
    store: Store,
    published: watch::Sender<WallpaperStatus>,
    state: WallpaperStatus,
    policy_valid: bool,
    sources: SuccessfulSources,
    selector: Arc<std::sync::Mutex<SourceSelector>>,
    fixture_control: Option<CompositorControl>,
    control: Option<CompositorControl>,
    activity: Option<ActivitySubscription>,
    wallpaper: Option<WallpaperSubscription>,
    observed_activity: Option<ActivityObservation>,
    sleeping: bool,
    edge_locked: bool,
    deadline: Option<Instant>,
    reconnect: Instant,
    reconnect_delay: Duration,
    active: Option<Active>,
    pending: Option<Selection>,
    submitted: Option<Submitted>,
    recovery_attempt: Option<(Revision, Revision)>,
}
impl Runtime {
    fn new(store: Store, published: watch::Sender<WallpaperStatus>, sources: SuccessfulSources, fixture_control: Option<CompositorControl>, spool_directory: PathBuf) -> Self {
        let state = published.borrow().clone();
        let policy_valid = state.error.is_none();
        Self { store, published, state, policy_valid, sources, selector: Arc::new(std::sync::Mutex::new(SourceSelector::new_in(spool_directory))), control: None, fixture_control, activity: None, wallpaper: None, observed_activity: None, sleeping: false, edge_locked: false, deadline: None, reconnect: Instant::now(), reconnect_delay: Duration::from_secs(2), active: None, pending: Some(Selection::Restore), submitted: None, recovery_attempt: None }
    }
    fn blocked(&self) -> bool {
        !self.policy_valid || self.sleeping || self.edge_locked || self.observed_activity.as_ref().is_none_or(|activity| !activity.session_active || activity.locked || activity.blanked)
    }
    fn publish(&self) {
        self.published.send_if_modified(|status| {
            if *status == self.state { false } else { *status = self.state.clone(); true }
        });
    }
    fn advance(&mut self) { self.state.revision.generation = self.state.revision.generation.saturating_add(1); }
    fn invalidate(&mut self, selection: Selection) {
        self.advance();
        if let Some(active) = &self.active { active.cancel.send_replace(true); }
        self.submitted = None;
        self.deadline = None;
        self.pending = Some(selection);
        self.state.pending = self.state.enabled;
        if self.policy_valid { self.state.error = None; }
        self.recovery_attempt = None;
    }
    fn start_deadline(&mut self) {
        self.deadline = self.state.config.interval_seconds.filter(|_| !self.state.paused).map(|seconds| Instant::now() + Duration::from_secs(seconds));
    }
    async fn connect(&mut self) -> Result<()> {
        let control = match &self.fixture_control { Some(control) => control.clone(), None => CompositorControl::from_runtime()? };
        // Both subscriptions precede effects. Their first messages are authoritative snapshots.
        let activity = control.subscribe_activity().await?;
        let wallpaper = control.subscribe_wallpaper().await?;
        self.control = Some(control); self.activity = Some(activity); self.wallpaper = Some(wallpaper);
        self.reconnect_delay = Duration::from_secs(2);
        Ok(())
    }
    fn disconnect(&mut self, error: impl std::fmt::Display) {
        if let Some(active) = &self.active { active.cancel.send_replace(true); }
        self.control = None; self.activity = None; self.wallpaper = None; self.observed_activity = None;
        self.submitted = None; self.state.applied = None; self.state.error = Some(format!("Compositor wallpaper authority unavailable: {error}"));
        let selection = self.pending.or_else(|| self.active.as_ref().map(|active| active.selection)).unwrap_or(Selection::Restore);
        self.pending = Some(selection); self.state.pending = self.state.enabled;
        self.reconnect = Instant::now() + self.reconnect_delay;
        self.reconnect_delay = (self.reconnect_delay * 2).min(Duration::from_secs(30));
        self.advance(); self.publish();
    }
    async fn run(mut self, mut messages: mpsc::Receiver<Message>) {
        loop {
            if self.state.enabled && !self.state.paused && !self.blocked() && self.deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                self.invalidate(Selection::Next); self.publish();
            }
            if self.active.is_none() && self.pending.is_some() && self.state.enabled && !self.blocked() {
                let selection = self.pending.take().expect("pending checked");
                let activity = self.observed_activity.as_ref().expect("unblocked owner exists");
                let mut job = Job { revision: self.state.revision, config: self.state.config.clone(), previous: self.sources.current.clone(), historical_previous: self.sources.previous.last().cloned(), selection, display_revision: activity.display_revision, outputs: activity.outputs.iter().filter(|output| output.enabled).map(|output| output.id.clone()).collect() };
                // Validate the shared candidate before changing any output. Output-specific
                // sources still decode/import serially, never with a second pending raster.
                job.outputs.sort_by(|left, right| representation_order(&job.config, left).cmp(&representation_order(&job.config, right)));
                if job.outputs.is_empty() { self.fail("No enabled compositor output is available"); }
                else {
                    let (cancel, cancelled) = watch::channel(false);
                    let control = self.control.clone().expect("unblocked control exists");
                    let selector = self.selector.clone();
                    self.active = Some(Active { selection, cancel, task: tokio::spawn(run_job(control, selector, job, cancelled)) });
                    self.state.pending = true; self.publish();
                }
            }
            let deadline = self.deadline.filter(|_| self.state.enabled && !self.state.paused && !self.blocked());
            let reconnect = self.control.is_none().then_some(self.reconnect);
            tokio::select! {
                message = messages.recv() => {
                    match message {
                        Some(Message::Policy(policy, desired_enabled, ack)) => {
                            match policy {
                                Ok((enabled, config)) => {
                                    let changed = !self.policy_valid || enabled != self.state.enabled || config != self.state.config;
                                    self.policy_valid = true;
                                    let sources_changed = config.sources != self.state.config.sources || config.recursive != self.state.config.recursive;
                                    self.state.enabled = enabled; self.state.config = config;
                                    if changed { self.invalidate(if sources_changed { Selection::Next } else { Selection::Restore }); self.deadline = None; }
                                    if !enabled { self.pending = None; self.state.pending = false; }
                                },
                                Err(error) => {
                                    self.policy_valid = false;
                                    self.state.enabled = desired_enabled;
                                    self.advance();
                                    if let Some(active) = &self.active { active.cancel.send_replace(true); }
                                    self.submitted = None; self.pending = None; self.deadline = None;
                                    self.fail(format!("Committed wallpaper policy is invalid: {error}"));
                                },
                            }
                            if !self.state.enabled { self.clear().await; }
                            self.publish(); let _ = ack.send(());
                        },
                        Some(Message::Action(action, ack)) => {
                            match action {
                                Action::Pause => { self.state.paused = true; self.deadline = None; },
                                Action::Resume => { self.state.paused = false; self.start_deadline(); },
                                Action::Next => self.invalidate(Selection::Next),
                                Action::Previous => self.invalidate(Selection::Previous),
                                Action::Refresh => self.invalidate(Selection::Refresh),
                            }
                            if !self.state.enabled && matches!(action, Action::Next | Action::Previous | Action::Refresh) { self.pending = None; self.state.error = Some("Wallpaper is disabled in Settings1".into()); }
                            self.publish(); let _ = ack.send(self.state.clone());
                        },
                        Some(Message::Edge(edge)) => {
                            match edge {
                                SessionEdge::Sleeping(sleeping) => self.sleeping = sleeping,
                                SessionEdge::Locked => self.edge_locked = true,
                                SessionEdge::Unlocked => self.edge_locked = false,
                                SessionEdge::Started | SessionEdge::Idle(_) => {},
                            }
                            if self.blocked() && let Some(active) = &self.active { active.cancel.send_replace(true); self.pending.get_or_insert(active.selection); }
                        },
                        Some(Message::Stop(ack)) => { self.reap_active().await; let _ = ack.send(()); break; },
                        None => { self.reap_active().await; break; },
                    }
                },
                result = finish_active(&mut self.active), if self.active.is_some() => {
                    self.active = None;
                    match result {
                        Ok(Ok(submitted)) if submitted.job.revision == self.state.revision && self.state.enabled => {
                            self.accept_observation(submitted.observation.clone());
                            self.submitted = Some(submitted);
                            self.observe_presentation(); self.publish();
                        },
                        Ok(Ok(_)) => {},
                        Ok(Err(error)) if self.pending.is_none() => self.fail(format!("{error:#}")),
                        Err(error) => self.fail(format!("Wallpaper job failed: {error}")),
                        _ => {},
                    }
                },
                observation = next_activity(&mut self.activity), if self.activity.is_some() => {
                    match observation {
                        Some(Ok(activity)) => {
                            let changed = self.observed_activity.as_ref().is_none_or(|old| old.display_revision != activity.display_revision);
                            let owner_changed = self.observed_activity.as_ref().is_none_or(|old| old.revision.owner_epoch != activity.revision.owner_epoch);
                            self.observed_activity = Some(activity);
                            if changed && self.state.enabled {
                                let selection = self.pending.or_else(|| self.active.as_ref().map(|active| active.selection)).unwrap_or(Selection::Restore);
                                self.invalidate(selection);
                            }
                            if owner_changed && !self.state.enabled { self.clear().await; }
                            if self.blocked() && let Some(active) = &self.active { active.cancel.send_replace(true); self.pending.get_or_insert(active.selection); }
                            if changed || owner_changed { self.publish(); }
                        },
                        Some(Err(error)) => self.disconnect(error),
                        None => self.disconnect("activity subscription ended"),
                    }
                },
                observation = next_wallpaper(&mut self.wallpaper), if self.wallpaper.is_some() => {
                    match observation {
                        Some(Ok(observation)) => {
                            let previous = self.state.applied.clone();
                            self.accept_observation(observation);
                            let recovery = self.state.applied.as_ref().is_some_and(|observation| observation.outputs.iter().any(|output| output.error.as_deref().is_some_and(|error| error.contains("re-decode"))));
                            self.observe_presentation();
                            if recovery && self.state.enabled && self.active.is_none() && self.pending.is_none() && self.submitted.is_none() {
                                if let Some(display_revision) = self.observed_activity.as_ref().map(|activity| activity.display_revision) {
                                    let key = (display_revision, self.state.revision);
                                    if self.recovery_attempt != Some(key) { self.invalidate(Selection::Restore); self.recovery_attempt = Some((display_revision, self.state.revision)); }
                                }
                            }
                            if previous != self.state.applied { self.publish(); }
                        },
                        Some(Err(error)) => self.disconnect(error),
                        None => self.disconnect("wallpaper subscription ended"),
                    }
                },
                _ = until(deadline), if deadline.is_some() => { self.deadline = None; self.invalidate(Selection::Next); self.publish(); },
                _ = until(reconnect), if reconnect.is_some() => { if let Err(error) = self.connect().await { self.disconnect(error); } },
            }
        }
    }
    async fn clear(&mut self) {
        if let Some(control) = &self.control {
            match control.request(ControlRequest::ClearWallpaper { revision: self.state.revision, output: None }).await {
                Ok(ControlResponse::Wallpaper(observation)) => self.accept_observation(observation),
                Ok(_) => self.state.error = Some("Compositor returned incompatible clear observation".into()),
                Err(error) => self.state.error = Some(error.to_string()),
            }
        }
    }
    async fn reap_active(&mut self) {
        if let Some(active) = self.active.take() { active.cancel.send_replace(true); let _ = active.task.await; }
    }
    fn fail(&mut self, error: impl Into<String>) { self.state.pending = false; self.state.error = Some(error.into()); self.publish(); }
    fn accept_observation(&mut self, observation: WallpaperObservation) {
        let old = self.state.applied.as_ref();
        if old.is_none_or(|old| old.revision.owner_epoch != observation.revision.owner_epoch || old.revision.generation <= observation.revision.generation) {
            self.state.applied = Some(observation);
        }
    }
    fn observe_presentation(&mut self) {
        let Some(submitted) = &self.submitted else { return; };
        let Some(observation) = &self.state.applied else { return; };
        let relevant: Vec<_> = submitted.job.outputs.iter().filter_map(|id| observation.outputs.iter().find(|output| &output.output == id)).collect();
        if relevant.len() != submitted.job.outputs.len() { return; }
        if relevant.iter().any(|output| output.error.is_some() && output.accepted == Some(submitted.job.revision)) {
            let errors = relevant.iter().filter_map(|output| output.error.as_deref()).collect::<Vec<_>>().join("; ");
            self.submitted = None; self.fail(errors); return;
        }
        if !relevant.iter().all(|output| output.presented == Some(submitted.job.revision)) { return; }
        let submitted = self.submitted.take().expect("submitted exists");
        if self.sources.current.as_ref() != Some(&submitted.source) {
            if matches!(submitted.job.selection, Selection::Previous) && self.sources.previous.last() == Some(&submitted.source) {
                self.sources.previous.pop();
            } else if let Some(current) = self.sources.current.take() { self.sources.previous.push(current); }
            if self.sources.previous.len() > HISTORY_LIMIT { self.sources.previous.remove(0); }
        }
        self.sources.current = Some(submitted.source.clone());
        self.state.source = Some(submitted.source.clone()); self.state.pending = false; self.state.error = None;
        if let Ok(mut selector) = self.selector.lock() { selector.record_success(&path(&submitted.source)); }
        if let Err(error) = save_sources(&self.store, &self.sources) { self.state.error = Some(format!("Wallpaper presented, but source persistence failed: {error:#}")); }
        self.recovery_attempt = None;
        self.start_deadline();
    }
}
async fn finish_active(active: &mut Option<Active>) -> std::result::Result<Result<Submitted>, tokio::task::JoinError> {
    match active { Some(active) => (&mut active.task).await, None => std::future::pending().await }
}
async fn next_activity(activity: &mut Option<ActivitySubscription>) -> Option<std::result::Result<ActivityObservation, templar_types::compositor_control::ControlError>> {
    match activity { Some(activity) => activity.next().await, None => std::future::pending().await }
}
async fn next_wallpaper(wallpaper: &mut Option<WallpaperSubscription>) -> Option<std::result::Result<WallpaperObservation, templar_types::compositor_control::ControlError>> {
    match wallpaper { Some(wallpaper) => wallpaper.next().await, None => std::future::pending().await }
}
async fn until(deadline: Option<Instant>) { match deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } }
fn path(bytes: &[u8]) -> PathBuf { PathBuf::from(OsString::from_vec(bytes.to_vec())) }
fn representation_order<'a>(config: &'a WallpaperConfig, output: &DisplayId) -> (Option<&'a [u8]>, [u8; 4], (u8, u64)) {
    let override_ = config.outputs.get(&output.0);
    let source = override_.and_then(|override_| override_.source.as_deref());
    let ground = override_.and_then(|override_| override_.ground).unwrap_or(config.ground);
    let texture = override_.and_then(|override_| override_.texture).unwrap_or(config.texture);
    let texture = match texture {
        templar_types::wallpaper::TextureLimit::Native => (0, 0),
        templar_types::wallpaper::TextureLimit::MaxEdge { pixels } => (1, u64::from(pixels)),
        templar_types::wallpaper::TextureLimit::MaxPixels { pixels } => (2, pixels),
    };
    (source, ground, texture)
}
fn load_sources(store: &Store) -> Result<SuccessfulSources> {
    let json: Option<String> = store.lock().query_row("SELECT value_json FROM settings_values WHERE key=?1", [LAST_SUCCESS], |row| row.get(0)).optional()?;
    let mut sources: SuccessfulSources = json.map(|json| serde_json::from_str(&json)).transpose()?.unwrap_or_default();
    ensure!(sources.current.as_ref().is_none_or(|source| !source.is_empty() && source.len() <= 4096 && !source.contains(&0)), "invalid persisted wallpaper source");
    sources.previous.retain(|source| source.len() <= 4096 && !source.contains(&0));
    if sources.previous.len() > HISTORY_LIMIT { sources.previous.drain(..sources.previous.len() - HISTORY_LIMIT); }
    Ok(sources)
}
fn save_sources(store: &Store, sources: &SuccessfulSources) -> Result<()> {
    let json = serde_json::to_string(sources)?;
    store.transaction(|tx| { tx.execute("INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,updated_ms=excluded.updated_ms", rusqlite::params![LAST_SUCCESS, json, templar_state::now_ms()])?; Ok(()) })
}

/// Unmanaged transport is available only through the explicit hermetic CLI option.
/// The real compositor fixture listener still checks peer credentials; no fake responses.
pub fn fixture_control(socket: &std::path::Path) -> Result<CompositorControl> {
    use std::os::unix::fs::MetadataExt;
    let directory = socket.parent().context("fixture socket has no parent directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    let uid = std::fs::metadata("/proc/self")?.uid();
    ensure!(socket.is_absolute() && metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid && metadata.mode() & 0o777 == 0o700 && std::fs::canonicalize(directory)? == directory, "wallpaper fixture socket requires an absolute owned0700 nonsymlink directory");
    Ok(CompositorControl::from_socket(socket))
}

async fn run_job(control: CompositorControl, selector: Arc<std::sync::Mutex<SourceSelector>>, job: Job, mut cancelled: watch::Receiver<bool>) -> Result<Submitted> {
    let first_output = job.outputs.first().context("no enabled wallpaper output")?;
    let first_config = templar_wallpaper::config::effective(&job.config, &first_output.0);
    let only_overrides = job.config.outputs.get(&first_output.0).and_then(|value| value.source.as_ref());
    let fixed = only_overrides.cloned().or_else(|| match job.selection {
        Selection::Restore | Selection::Refresh => job.previous.clone(),
        Selection::Previous => job.historical_previous.clone(),
        Selection::Next => None,
    });
    if matches!(job.selection, Selection::Refresh) {
        let selector = selector.clone();
        tokio::task::spawn_blocking(move || selector.lock().map_err(|_| anyhow::anyhow!("wallpaper selector poisoned")).map(|mut selector| selector.refresh())).await??;
    }
    let mut attempted = Vec::<Vec<u8>>::new();
    let mut previous = job.previous.clone();
    let (source, opened_source, first_raster, first_descriptor) = loop {
        ensure!(!*cancelled.borrow(), "wallpaper generation cancelled");
        let bytes = if let Some(fixed) = &fixed { fixed.clone() } else {
            let selector = selector.clone();
            let config = job.config.clone();
            let previous = previous.as_ref().map(|bytes| path(bytes));
            let direction = if matches!(job.selection, Selection::Previous) { Direction::Previous } else { Direction::Next };
            let cancellation = cancelled.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                let mut selector = selector.lock().map_err(|_| anyhow::anyhow!("wallpaper selector poisoned"))?;
                let selected = selector.select_cancellable(&config, previous.as_deref(), direction, || *cancellation.borrow())?;
                Ok(selected.as_os_str().as_bytes().to_vec())
            }).await.context("wallpaper source traversal")??
        };
        ensure!(!attempted.contains(&bytes), "no usable wallpaper among the bounded candidate attempts");
        attempted.push(bytes.clone());
        let prepared = async {
            let source = open_source(path(&bytes)).await?;
            let output = raster::create_memfd()?;
            let descriptor = decode(&source, &output, &first_config, &mut cancelled).await?;
            raster::seal(&output)?;
            raster::validate(&output, &descriptor)?;
            Ok::<_, anyhow::Error>((source, output, descriptor))
        }.await;
        match prepared {
            Ok((source, raster, descriptor)) => break (bytes, source, raster, descriptor),
            Err(error) if fixed.is_some() || attempted.len() == 8 || *cancelled.borrow() => return Err(error).context("no usable wallpaper in this bounded rotation attempt"),
            Err(_) => previous = Some(bytes),
        }
    };
    let mut opened = BTreeMap::<Vec<u8>, File>::new();
    opened.insert(source.clone(), opened_source);
    let mut raster = Some((first_raster, first_descriptor));
    let mut representation = (source.as_slice(), first_config.texture, first_config.ground);
    let mut observation = None;
    for output in &job.outputs {
        ensure!(!*cancelled.borrow(), "wallpaper generation cancelled");
        let config = templar_wallpaper::config::effective(&job.config, &output.0);
        let source_bytes = job.config.outputs.get(&output.0).and_then(|override_| override_.source.as_ref()).unwrap_or(&source);
        if !opened.contains_key(source_bytes) { opened.insert(source_bytes.clone(), open_source(path(source_bytes)).await?); }
        let next_representation = (source_bytes.as_slice(), config.texture, config.ground);
        if representation != next_representation {
            // Keep only this job's one candidate backing, not a raster cache. Identical
            // consecutive outputs share its sealed FD; drop it before preparing another.
            raster.take();
            let file = opened.get_mut(source_bytes).context("opened wallpaper source missing")?;
            file.seek(SeekFrom::Start(0))?;
            let output = raster::create_memfd()?;
            let descriptor = decode(file, &output, &config, &mut cancelled).await?;
            raster::seal(&output)?;
            raster::validate(&output, &descriptor)?;
            raster = Some((output, descriptor));
            representation = next_representation;
        }
        let (raster_file, descriptor) = raster.as_ref().context("prepared wallpaper candidate missing")?;
        ensure!(!*cancelled.borrow(), "wallpaper generation cancelled after decode");
        let image = WallpaperImage { revision: job.revision, display_revision: job.display_revision, output: output.clone(), raster: descriptor.clone(), sizing: config.sizing, ground: config.ground, transition_budget_bytes: config.transition_budget_bytes };
        let response = control.request_with_fd(ControlRequest::SetWallpaper { image }, raster_file.as_fd()).await?;
        let ControlResponse::Wallpaper(actual) = response else { bail!("compositor returned incompatible wallpaper import response"); };
        ensure!(actual.outputs.iter().any(|actual| &actual.output == output && actual.accepted == Some(job.revision)), "compositor did not accept the wallpaper generation");
        observation = Some(actual);
    }
    Ok(Submitted { job, source, observation: observation.context("no wallpaper output was submitted")? })
}

async fn open_source(path: PathBuf) -> Result<File> {
    tokio::task::spawn_blocking(move || -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        let source = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).context("opening wallpaper source once")?;
        ensure!(source.metadata()?.is_file(), "wallpaper source is not a regular file");
        Ok(source)
    }).await.context("wallpaper source open worker")?
}

fn decoder_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("TEMPLAROS_WALLPAPER_DECODER") { return path.into(); }
    if let Ok(executable) = std::env::current_exe() && let Some(parent) = executable.parent() {
        let adjacent = parent.join("templar-wallpaper-decode");
        if adjacent.is_file() { return adjacent; }
    }
    PathBuf::from("/usr/libexec/templar-wallpaper-decode")
}
#[allow(unsafe_code)]
async fn decode(source: &File, output: &File, config: &WallpaperConfig, cancelled: &mut watch::Receiver<bool>) -> Result<RasterDescriptor> {
    // Duplicate above the fixed destinations first: original fds can themselves be 3 or 4.
    let source_fd = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    ensure!(source_fd >= 0, "duplicating wallpaper source: {}", std::io::Error::last_os_error());
    // SAFETY: fcntl transferred one new owned descriptor on success.
    let source_fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(source_fd) };
    let output_fd = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    ensure!(output_fd >= 0, "duplicating wallpaper raster: {}", std::io::Error::last_os_error());
    // SAFETY: fcntl transferred one new owned descriptor on success.
    let output_fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(output_fd) };
    let source_number = source_fd.as_raw_fd(); let output_number = output_fd.as_raw_fd();
    let options = serde_json::json!({ "texture": config.texture, "ground": config.ground, "worker_budget_bytes": config.worker_budget_bytes, "quantize_colors": config.quantize_colors }).to_string();
    let mut command = tokio::process::Command::new(decoder_executable());
    command.args(["--source-fd", "3", "--output-fd", "4", "--options-json", &options]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    // SAFETY: pre_exec calls only async-signal-safe syscalls, using copied fd numbers and no
    // allocator/locks. The parent keeps both owned descriptors alive until spawn returns.
    unsafe { command.pre_exec(move || {
        if libc::dup2(source_number, 3) < 0 || libc::dup2(output_number, 4) < 0 { return Err(std::io::Error::last_os_error()); }
        let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 { return Err(std::io::Error::last_os_error()); }
        Ok(())
    }); }
    let mut child = command.spawn().context("starting temporary templar-wallpaper-decode")?;
    drop(source_fd); drop(output_fd);
    let stdout = child.stdout.take().context("decoder stdout missing")?;
    let stderr = child.stderr.take().context("decoder stderr missing")?;
    let stdout = tokio::spawn(async move { let mut data = Vec::new(); stdout.take(METADATA_LIMIT + 1).read_to_end(&mut data).await.map(|_| data) });
    let stderr = tokio::spawn(async move { let mut data = Vec::new(); stderr.take(METADATA_LIMIT + 1).read_to_end(&mut data).await.map(|_| data) });
    let mut stopped = false;
    let wait = tokio::select! {
        result = child.wait() => result,
        _ = tokio::time::sleep(WORKER_TIMEOUT) => { stopped = true; let _ = child.start_kill(); child.wait().await },
        _ = cancelled.changed() => { stopped = true; let _ = child.start_kill(); child.wait().await },
    };
    // Reaping precedes every seal/import. Even cancellation consumes both pipe owners.
    let wait = match wait {
        Ok(status) => Ok(status),
        Err(error) => { let _ = child.start_kill(); let _ = child.wait().await; Err(error) },
    };
    let metadata = stdout.await.context("joining decoder metadata reader")??;
    let errors = stderr.await.context("joining decoder error reader")??;
    let status = wait.context("reaping wallpaper decoder")?;
    ensure!(!stopped, "wallpaper decoder cancelled or timed out");
    ensure!(status.success(), "wallpaper decoder failed: {}", String::from_utf8_lossy(&errors));
    ensure!(metadata.len() as u64 <= METADATA_LIMIT, "decoder metadata exceeds its 16 KiB limit");
    Ok(serde_json::from_slice(&metadata).context("parsing finalized decoder raster descriptor")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::wallpaper::WallpaperOutput;

    fn runtime() -> Runtime {
        let status = WallpaperStatus { revision: Revision::new_owner(), enabled: true, config: WallpaperConfig::default(), paused: false, source: None, pending: true, applied: None, error: None };
        let (published, _) = watch::channel(status);
        Runtime::new(Store::in_memory().unwrap(), published, SuccessfulSources::default(), None, PathBuf::from("/var/tmp"))
    }
    fn observation(revision: Revision, presented: bool) -> WallpaperObservation {
        WallpaperObservation { revision: Revision::new_owner(), indexed_supported: true, outputs: vec![WallpaperOutput { output: DisplayId("test-output".into()), accepted: Some(revision), presented: presented.then_some(revision), source_bytes: 16, texture_bytes: 16, context_replicas: 1, error: None }] }
    }
    fn submitted(runtime: &Runtime, source: &[u8], selection: Selection) -> Submitted {
        Submitted { job: Job { revision: runtime.state.revision, config: runtime.state.config.clone(), previous: None, historical_previous: None, selection, display_revision: Revision::new_owner(), outputs: vec![DisplayId("test-output".into())] }, source: source.to_vec(), observation: observation(runtime.state.revision, false) }
    }
    #[test]
    fn import_acknowledgement_never_persists_a_successful_source() {
        let mut runtime = runtime();
        let source = [b'/', 255, b'.', b'p', b'n', b'g'];
        runtime.submitted = Some(submitted(&runtime, &source, Selection::Next));
        runtime.state.applied = Some(observation(runtime.state.revision, false));
        runtime.observe_presentation();
        assert!(runtime.state.source.is_none());
        assert!(load_sources(&runtime.store).unwrap().current.is_none());
        assert!(runtime.state.pending);
        runtime.state.applied = Some(observation(runtime.state.revision, true));
        runtime.observe_presentation();
        assert_eq!(runtime.state.source.as_deref(), Some(source.as_slice()));
        assert_eq!(load_sources(&runtime.store).unwrap().current, runtime.state.source);
        assert!(!runtime.state.pending);
    }
    #[test]
    fn previous_consumes_only_successful_history_without_oscillating() {
        let mut runtime = runtime();
        runtime.sources.current = Some(b"c.png".to_vec());
        runtime.sources.previous = vec![b"a.png".to_vec(), b"b.png".to_vec()];
        runtime.submitted = Some(submitted(&runtime, b"b.png", Selection::Previous));
        runtime.state.applied = Some(observation(runtime.state.revision, true));
        runtime.observe_presentation();
        assert_eq!(runtime.sources.previous, vec![b"a.png".to_vec()]);
        assert_eq!(runtime.sources.current, Some(b"b.png".to_vec()));
    }
    #[test]
    fn superseding_selection_discards_pending_presentation_and_deadline() {
        let mut runtime = runtime();
        runtime.submitted = Some(submitted(&runtime, b"old.png", Selection::Next));
        runtime.deadline = Some(Instant::now());
        let old = runtime.state.revision;
        runtime.invalidate(Selection::Previous);
        assert!(runtime.submitted.is_none());
        assert!(runtime.deadline.is_none());
        assert_ne!(runtime.state.revision, old);
        assert!(matches!(runtime.pending, Some(Selection::Previous)));
    }
    #[test]
    fn late_import_reply_cannot_overwrite_a_newer_presentation() {
        let mut runtime = runtime();
        let presented = observation(runtime.state.revision, true);
        let mut late = presented.clone();
        late.revision.generation = 0;
        late.outputs[0].presented = None;
        let mut presented = presented;
        presented.revision.generation = 1;
        runtime.accept_observation(presented.clone());
        runtime.accept_observation(late);
        assert_eq!(runtime.state.applied, Some(presented));
    }
    #[test]
    fn idle_activity_does_not_publish_identical_wallpaper_state() {
        let runtime = runtime();
        let receiver = runtime.published.subscribe();
        runtime.publish();
        assert!(!receiver.has_changed().unwrap());
    }
    #[test]
    fn fixture_socket_rejects_shared_directories_and_relative_paths() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(fixture_control(&directory.path().join("control.sock")).is_err());
        assert!(fixture_control(std::path::Path::new("relative.sock")).is_err());
    }
}
