//! `org.templaros.HomeGarden1`: the activity ledger behind Home's grown places, and the tending
//! moments that bring the field in line with it (`templar_session_core::homegarden`, the planner in
//! `templar_field::garden`; `@templaros.decision.homegarden-activity-grown-places`).
//!
//! The ledger lives in the same store as the Home Field it tends and every field change goes
//! through `homefield::apply_op` as one `Batch`, so it is undoable through `HomeField1.Undo` and
//! visible in `History1` like any other change. The Garden Clock is driven by the session edges
//! `Lifecycle1` observes; launches with a directory argument arrive on the launch channel.
//! Nothing here ticks.

use crate::apis::{
    failed,
    lifecycle::{SessionEdge, SessionEdges},
};
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use templar_field::garden::{ContextView, HomeCorner, garden_tile_id, tend};
use templar_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    homefield,
    homegarden::{self, ActivityKind, GardenConfig, HomeActivity, IndexChange, Ledger, RootProbe},
    providers::ProviderRegistry,
    settings::{SettingValue, SettingsSchema, SettingsValues},
};
use templar_state::Store;
use templar_types::{AppId, InstanceId, ProviderRole, dbus::session as names};
use zbus::{fdo, object_server::SignalEmitter};

/// Everything the host shares between D-Bus calls and the edge listener.
struct Inner {
    ledger: Ledger,
    /// Process-relative monotonic origin: the clock's intervals are never persisted, so the
    /// origin need not be.
    origin: Instant,
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
pub struct HomeGardenApi {
    inner: Arc<Mutex<Inner>>,
    store: Store,
    history: Arc<SqliteHistory>,
    providers: Arc<RwLock<ProviderRegistry>>,
    schema: Arc<RwLock<SettingsSchema>>,
    settings: Arc<RwLock<SettingsValues>>,
    probe: Arc<DaemonProbe>,
    connection: zbus::Connection,
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
    ) -> Self {
        let ledger = homegarden::load(&store);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                ledger,
                origin: Instant::now(),
            })),
            probe: Arc::new(DaemonProbe::new(store.clone())),
            store,
            history,
            providers,
            schema,
            settings,
            connection,
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
        let _ = inner.now();
        if let Err(error) = homegarden::save(&self.store, &inner.ledger) {
            tracing::warn!(%error, "failed to persist the Homegarden ledger");
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
        let config = self.config();
        if !config.enabled {
            return;
        }
        let role = role.or_else(|| app_id.as_ref().and_then(|app| self.role_of(app)));
        let mut inner = self.inner.lock().expect("garden lock");
        let at = inner.now();
        let activity = HomeActivity {
            path,
            kind,
            app_id,
            role,
            at,
            instance_id,
        };
        if let Some(root) = inner
            .ledger
            .observe(&activity, self.probe.as_ref(), &config)
        {
            tracing::info!(kind = kind.as_str(), root = %root.display(), "garden observed");
            self.persist(&mut inner);
        }
    }

    /// A launch happened: a directory argument is a `LaunchedHere` on that directory, and either
    /// way the instance it produced belongs to whatever place the argument lies under.
    ///
    /// The two associations the doc calls strong are both this one event. A directory argument is
    /// "launched here" and is evidence; a file argument is the brokered open, whose *weight* the
    /// shell has already reported as `file-opened` — so the instance is attached without counting
    /// the act twice. This is the only place in the system that knows which instance a launch
    /// produced, which is why the association is made here and not over the bus.
    pub fn observe_launch(&self, app_id: &AppId, arguments: &[String], instance: InstanceId) {
        if let Some(directory) = launch_directory(arguments) {
            self.observe_activity(
                ActivityKind::LaunchedHere,
                directory,
                Some(app_id.clone()),
                None,
                Some(instance),
            );
            return;
        }
        let Some(file) = launch_file(arguments) else {
            return;
        };
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
            self.persist(&mut inner);
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
            }
            self.persist(&mut inner);
        }
        if edge == SessionEdge::Started {
            self.ensure_desktop_place();
        }
    }

    /// Desktop starts as a Place, once.
    ///
    /// Home used to merge a permanent Desktop tray of its own; it no longer does, because the one
    /// tile the person never chose was also the one that manufactured its own evidence. The
    /// directory things arrive in still has to be *there* on a first run, so the first session
    /// records the same act a person's own "Add to Home" records — `placed-on-home`, which pins
    /// it — and from then on it is an ordinary Place: unpin, hide, keep and recession all work on
    /// it. Recorded only when the ledger has never heard of the directory, so a hidden or unpinned
    /// Desktop stays the way it was left.
    fn ensure_desktop_place(&self) {
        let Some(desktop) = desktop_dir(self.probe.home()) else {
            return;
        };
        {
            let inner = self.inner.lock().expect("garden lock");
            if inner.ledger.context(&desktop).is_some() {
                return;
            }
        }
        self.observe_activity(ActivityKind::PlacedOnHome, desktop, None, None, None);
    }

    /// One tending moment for `output`: stages, then the field.
    fn tend_output(&self, output: &str) -> anyhow::Result<TendReport> {
        let config = self.config();
        if !config.enabled {
            return Ok(TendReport::default());
        }
        let corner = self.corner();
        let mut inner = self.inner.lock().expect("garden lock");
        let now = inner.now();
        let changes = inner.ledger.tend(now, &config);
        let Some(doc) = homefield::load(&self.store, output)? else {
            self.persist(&mut inner);
            return Ok(TendReport::default());
        };
        // The user's hand since last time: a moved tile updates its landing; a resized tile
        // becomes user-sized and the tender stops resizing it; a tile the tender never placed —
        // a folder the user put on Home by hand, or one that was there before the garden was —
        // is the user's outright: pinned and kept, so it is neither resized nor removed by
        // recession. Only what the garden grew is the garden's to prune.
        for context in &mut inner.ledger.contexts {
            if let Some(located) = doc.location(garden_tile_id(&context.root)) {
                context.landing = Some(located.placement.cell);
                match context.tended_span {
                    Some(span) if span != located.placement.span => {
                        context.user_span = Some(located.placement.span);
                    }
                    None if !context.hidden => {
                        context.pinned = true;
                        context.user_span = Some(located.placement.span);
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
                wanted: if context.hidden {
                    None
                } else {
                    context.stage.span()
                },
                landing: context.landing,
                pinned: context.pinned,
                user_span: context.user_span,
                summary: context.summary(),
            })
            .collect();
        let tended = tend(&doc, &views, corner);
        let mut report = TendReport {
            changed: 0,
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
            report.changed = u32::try_from(tended.len()).unwrap_or(u32::MAX);
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
        let changes = garden_changes(&batch);
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

/// The parsed launch directory: `--directory X`, `--working-directory X`, `-d X`, or the first
/// bare absolute argument that is a directory.
/// The first absolute file path among the arguments — the brokered open's own argument.
fn launch_file(arguments: &[String]) -> Option<PathBuf> {
    arguments
        .iter()
        .filter(|argument| argument.starts_with('/'))
        .map(PathBuf::from)
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
        "breadth": context.breadth(now),
        "idle_sessions": now.sessions_since(context.last),
        "idle_garden_hours": now.hours_since(context.last),
        "roles": context.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        "tree": context.tree,
        "pinned": context.pinned,
        "kept": context.user_span.is_some(),
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
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(fdo::Error::InvalidArgs("path must be absolute".into()));
    }
    Ok(path)
}

#[zbus::interface(name = "org.templaros.HomeGarden1")]
impl HomeGardenApi {
    async fn observe(&self, kind: &str, path: &str, app_id: &str, role: &str) -> fdo::Result<()> {
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
        let role = if role.is_empty() {
            None
        } else {
            Some(
                ProviderRole::ALL
                    .into_iter()
                    .find(|r| r.as_str() == role)
                    .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown role {role:?}")))?,
            )
        };
        // No instance: an act observed over the bus is one nobody can vouch for the instance
        // of, and a guess would be an association the user never made.
        self.observe_activity(kind, path, app_id, role, None);
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
        self.read_index_changes().await;
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
        // The size to keep is the tile's, wherever it is; a place with no tile yet is pinned, and
        // the first tending sizes it.
        let span = self
            .tile_locations(&path)
            .into_iter()
            .next()
            .map(|(_, located)| located.placement.span);
        let mut inner = self.inner.lock().expect("garden lock");
        let known = match span {
            Some(span) => inner.ledger.keep(&path, span),
            None => inner.ledger.set_pinned(&path, true),
        };
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
    launches: &tokio::sync::broadcast::Sender<(AppId, Vec<String>, InstanceId)>,
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
        loop {
            match launches.recv().await {
                Ok((app_id, arguments, instance)) => {
                    api.observe_launch(&app_id, &arguments, instance);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
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

impl DaemonProbe {
    fn new(store: Store) -> Self {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
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

/// The XDG Desktop directory, the one Place a first run starts with.
fn desktop_dir(home: &Path) -> Option<PathBuf> {
    desktop_dir_from(home, &config_dir(home))
}

/// `$XDG_DESKTOP_DIR`, or `~/Desktop` when the file says nothing. `None` when it is `$HOME`
/// itself — a home directory is never a Place — or when there is no such directory, which is
/// the readable degradation for an account that has none.
fn desktop_dir_from(home: &Path, config_dir: &Path) -> Option<PathBuf> {
    let named = std::fs::read_to_string(config_dir.join("user-dirs.dirs"))
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .find(|(key, _)| key.trim() == "XDG_DESKTOP_DIR")
                .map(|(_, value)| value.trim().trim_matches('"').to_owned())
        });
    let path = match named.as_deref() {
        Some("$HOME") => return None,
        Some(value) => value
            .strip_prefix("$HOME/")
            .map_or_else(|| PathBuf::from(value), |rest| home.join(rest)),
        None => home.join("Desktop"),
    };
    path.is_dir().then_some(path)
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

    /// Home no longer merges a permanent Desktop tray, so the directory things arrive in has to
    /// reach the field the way every other Place does: one `placed-on-home` on a first run, which
    /// pins it. This is the directory that act names.
    #[test]
    fn the_desktop_place_is_the_xdg_directory_and_never_home_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config = home.join(".config");
        std::fs::create_dir_all(&config).unwrap();

        // No file: the English default, and only if it is really there.
        assert_eq!(desktop_dir_from(home, &config), None);
        std::fs::create_dir(home.join("Desktop")).unwrap();
        assert_eq!(desktop_dir_from(home, &config), Some(home.join("Desktop")));

        // The user's own name for it wins.
        std::fs::create_dir(home.join("Skrivebord")).unwrap();
        std::fs::write(
            config.join("user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/Skrivebord\"\n",
        )
        .unwrap();
        assert_eq!(
            desktop_dir_from(home, &config),
            Some(home.join("Skrivebord"))
        );

        // A desktop that *is* $HOME is the setting for "no desktop directory", and a home
        // directory is never a Place.
        std::fs::write(config.join("user-dirs.dirs"), "XDG_DESKTOP_DIR=\"$HOME\"\n").unwrap();
        assert_eq!(desktop_dir_from(home, &config), None);
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
