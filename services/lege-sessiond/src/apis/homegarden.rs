//! `org.legeos.HomeGarden1`: the activity ledger behind Home's grown places, and the tending
//! moments that bring the field in line with it (`lege_session_core::homegarden`, the planner in
//! `lege_field::garden`; `@legeos.decision.homegarden-activity-grown-places`).
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
use lege_field::garden::{ContextView, HomeCorner, garden_tile_id, tend};
use lege_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    homefield,
    homegarden::{self, ActivityKind, GardenConfig, HomeActivity, Ledger, RootProbe},
    providers::ProviderRegistry,
    settings::{SettingValue, SettingsSchema, SettingsValues},
};
use lege_state::Store;
use lege_types::{AppId, ProviderRole, dbus::session as names};
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
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

    fn persist(&self, inner: &Inner) {
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
    ) {
        let config = self.config();
        if !config.enabled {
            return;
        }
        let role = role.or_else(|| app_id.as_ref().and_then(|app| self.role_of(app)));
        let mut inner = self.inner.lock().expect("garden lock");
        let at = inner.ledger.clock.now(inner.monotonic_ms());
        let activity = HomeActivity {
            path,
            kind,
            app_id,
            role,
            at,
        };
        if let Some(root) = inner
            .ledger
            .observe(&activity, self.probe.as_ref(), &config)
        {
            tracing::info!(kind = kind.as_str(), root = %root.display(), "garden observed");
            self.persist(&inner);
        }
    }

    /// A launch happened: a directory argument is a `LaunchedHere` on that directory.
    pub fn observe_launch(&self, app_id: &AppId, arguments: &[String]) {
        let Some(directory) = launch_directory(arguments) else {
            return;
        };
        self.observe_activity(
            ActivityKind::LaunchedHere,
            directory,
            Some(app_id.clone()),
            None,
        );
    }

    fn on_edge(&self, edge: SessionEdge) {
        let mut inner = self.inner.lock().expect("garden lock");
        let now = inner.monotonic_ms();
        match edge {
            SessionEdge::Started => inner.ledger.clock.start_session(now),
            SessionEdge::Unlocked | SessionEdge::Sleeping(false) => inner.ledger.clock.resume(now),
            SessionEdge::Locked | SessionEdge::Sleeping(true) => inner.ledger.clock.pause(now),
        }
        self.persist(&inner);
    }

    /// One tending moment for `output`: stages, then the field.
    fn tend_output(&self, output: &str) -> anyhow::Result<TendReport> {
        let config = self.config();
        if !config.enabled {
            return Ok(TendReport::default());
        }
        let corner = self.corner();
        let mut inner = self.inner.lock().expect("garden lock");
        let now = inner.ledger.clock.now(inner.monotonic_ms());
        let changes = inner.ledger.tend(now, &config);
        let Some(doc) = homefield::load(&self.store, output)? else {
            self.persist(&inner);
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
            let batch = lege_field::FieldOp::Batch {
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
        self.persist(&inner);
        Ok(report)
    }

    /// Where `root`'s tile is placed, on every output that has it.
    fn tile_locations(&self, root: &Path) -> Vec<(String, lege_field::Located)> {
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
            match homefield::apply_op(&self.store, &output, &lege_field::FieldOp::Remove { id }) {
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
            at_ms: lege_state::now_ms(),
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
        "idle_hours": now.hours_since(context.last),
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

#[zbus::interface(name = "org.legeos.HomeGarden1")]
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
        self.observe_activity(kind, path, app_id, role);
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
        self.persist(&inner);
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
        self.persist(&inner);
        Ok(())
    }

    async fn hide(&self, path: &str, hidden: bool) -> fdo::Result<()> {
        let path = parse_path(path)?;
        {
            let mut inner = self.inner.lock().expect("garden lock");
            if !inner.ledger.set_hidden(&path, hidden) {
                return Err(not_a_place(&path));
            }
            self.persist(&inner);
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
            self.persist(&inner);
        }
        self.remove_tile_everywhere(&path, "removed").await;
        Ok(())
    }

    async fn forget_all(&self) -> fdo::Result<()> {
        let mut inner = self.inner.lock().expect("garden lock");
        inner.ledger.forget_all();
        self.persist(&inner);
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
    launches: &tokio::sync::broadcast::Sender<(AppId, Vec<String>)>,
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
                Ok((app_id, arguments)) => api.observe_launch(&app_id, &arguments),
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

/// `~/.config/user-dirs.dirs`, the file `xdg-user-dirs` writes; the English defaults when absent.
fn user_dirs(home: &Path) -> Vec<PathBuf> {
    let config =
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    user_dirs_from(home, &config)
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
        let projects = lege_session_core::projects::load_all(&self.store).ok()?;
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
}
