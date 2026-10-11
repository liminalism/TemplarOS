//! The launcher borrows one overlay; service authority and ordinary window activation stay shared.
use super::*;
use crate::launcher_model::LauncherRow;
use crate::surface::{
    launcher_screen::{LauncherAction, LauncherData},
    omnibox_screen::Intent,
};
use templar_types::launcher::{
    self, Favorite, FavoriteTarget, LauncherMutation, LauncherPreferences,
};

#[derive(Debug)]
pub(super) struct LauncherRequest {
    pub mode: LauncherMode,
    pub scope: Option<PathBuf>,
    pub query: String,
}

pub(super) fn profile_key(id: &str, target: &FavoriteTarget) -> String {
    // Retain proof for copied Home targets even after a rail profile is edited and relaunched.
    serde_json::to_string(&(id, target)).expect("profile serializes to JSON")
}

pub(super) fn mutation_queue(
    runtime: &tokio::runtime::Handle,
    updates: &Sender<DataUpdate>,
    wake: &EventLoopProxy<Wake>,
    epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> tokio::sync::mpsc::Sender<LauncherMutation> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<LauncherMutation>(32);
    let updates = updates.clone();
    let wake = wake.clone();
    runtime.spawn(async move {
        while let Some(command) = receiver.recv().await {
            let result: anyhow::Result<()> = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let settings = SettingsProxy::new(&connection).await?;
                settings
                    .update_launcher_v1(&serde_json::to_string(&command)?, "shell")
                    .await?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                let _ = updates.send(DataUpdate::Notice(format!(
                    "Launcher preference was not saved: {error:#}"
                )));
                let epoch = epoch.load(std::sync::atomic::Ordering::SeqCst);
                if let Some(preferences) = load_preferences().await {
                    let _ = updates.send(DataUpdate::LauncherPreferences { epoch, preferences });
                }
                let _ = wake.send_event(Wake::Data);
            }
        }
    });
    sender
}

pub(super) async fn load_preferences() -> Option<LauncherPreferences> {
    let connection = crate::data::session().await.ok()?;
    let proxy = SettingsProxy::new(&connection).await.ok()?;
    let value: serde_json::Value =
        serde_json::from_str(&proxy.get(launcher::PREFERENCES_KEY).await.ok()?).ok()?;
    let preferences: LauncherPreferences =
        serde_json::from_str(value.get("value")?.as_str()?).ok()?;
    preferences.validate().ok()?;
    Some(preferences)
}

pub(super) async fn load_transition() -> (u32, u32) {
    let Ok(connection) = crate::data::session().await else {
        return (2, 6);
    };
    let Ok(proxy) = SettingsProxy::new(&connection).await else {
        return (2, 6);
    };
    let parse = |value: String| serde_json::from_str::<serde_json::Value>(&value).ok();
    let kind = proxy
        .get("shell.appearance.window-transition")
        .await
        .ok()
        .and_then(parse)
        .and_then(|value| {
            value
                .get("value")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let grain = proxy
        .get("shell.appearance.particle-size")
        .await
        .ok()
        .and_then(parse)
        .and_then(|value| value.get("value").and_then(serde_json::Value::as_u64))
        .unwrap_or(6)
        .clamp(1, 24) as u32;
    (
        match kind.as_deref() {
            Some("off") => 0,
            Some("simple") => 1,
            _ => 2,
        },
        grain,
    )
}

impl ShellApp {
    pub(super) fn request_launcher(
        &mut self,
        mode: LauncherMode,
        scope: Option<PathBuf>,
        query: String,
    ) {
        self.launcher_request = Some(LauncherRequest { mode, scope, query });
        let _ = self.wake_proxy.send_event(Wake::Data);
    }

    pub(super) fn ensure_launcher(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .bridge
            .as_ref()
            .is_some_and(|bridge| bridge.lock_engaged() || bridge.lock_locked())
        {
            self.launcher_request = None;
            self.dismiss_launcher();
            return;
        }
        let Some(request) = self.launcher_request.take() else {
            return;
        };
        self.note_search(Scope::Applications, String::new());
        self.launcher_screen
            .show(request.mode, request.scope, request.query);
        self.launcher_focus_claimed = false;
        if self.launcher.is_none() {
            let monitor = self
                .home
                .as_ref()
                .and_then(|home| home.window.current_monitor());
            let size = monitor
                .as_ref()
                .map(|monitor| monitor.size().to_logical::<f64>(monitor.scale_factor()));
            let width = size
                .as_ref()
                .map_or(720.0, |size| size.width.min(720.0))
                .max(240.0);
            let height = size
                .as_ref()
                .map_or(480.0, |size| (size.height - 64.0).min(480.0))
                .max(160.0);
            let result: anyhow::Result<HostedWindow> = (|| {
                let window = std::sync::Arc::new(
                    event_loop.create_window(
                        Window::default_attributes()
                            .with_title("Templar Launcher")
                            .with_decorations(false)
                            .with_transparent(true)
                            .with_resizable(false)
                            .with_inner_size(LogicalSize::new(width, height)),
                    )?,
                );
                let mut hosted = if let Some(bridge) = &self.bridge {
                    bridge.set_layer(&window, templar_window::Layer::ShellOverlay)?;
                    bridge.set_exclusive_zone(
                        &window,
                        templar_window::geometry::Edge::Bottom,
                        0,
                    )?;
                    bridge.set_edge_alignment(
                        &window,
                        templar_window::geometry::Edge::Bottom,
                        templar_window::geometry::Alignment::Centre,
                    )?;
                    HostedWindow::new_wayland(window)
                } else {
                    HostedWindow::new(window)?
                };
                hosted.attach_accessibility(event_loop, self.wake_proxy.clone());
                let wake = self.wake_proxy.clone();
                hosted
                    .input
                    .attach_clipboard(pixelkit_shell::clipboard::ClipboardWorker::new(move || {
                        let _ = wake.send_event(Wake::Data);
                    }));
                hosted.focus.set(0);
                Ok(hosted)
            })();
            match result {
                Ok(hosted) => self.launcher = Some(hosted),
                Err(error) => {
                    self.launcher_screen.state_mut().dismiss();
                    self.field_screen.report(
                        format!("Could not open launcher: {error:#}"),
                        self.started.elapsed().as_millis() as u64,
                    );
                    return;
                }
            }
        }
        self.schedule_launcher_search();
        if let Some(hosted) = &self.launcher {
            hosted.request_redraw();
        }
    }

    pub(super) fn dismiss_launcher(&mut self) {
        self.launcher_screen.state_mut().dismiss();
        self.launcher_request = None;
        self.note_search(Scope::Applications, String::new());
        if let Some(hosted) = self.launcher.take() {
            if let Some(bridge) = &mut self.bridge {
                if self.launcher_focus_claimed {
                    let _ = bridge.focus_surface(None);
                }
                bridge.forget_surface(&hosted.window);
            }
        }
        self.launcher_focus_claimed = false;
    }

    pub(super) fn search_scope_current(&self, scope: &Scope, query: &str) -> bool {
        match scope {
            Scope::LauncherFiles(path) => {
                let state = self.launcher_screen.state();
                state.open
                    && state.mode == LauncherMode::Files
                    && &state.scope == path
                    && state.query == query
            }
            Scope::Location(location) => {
                self.field_screen.files_expanded && self.field_screen.files.location() == location
            }
            Scope::Applications => false,
        }
    }

    fn schedule_launcher_search(&mut self) {
        let state = self.launcher_screen.state();
        tracing::debug!(mode = ?state.mode, query = %state.query, generation = state.generation, "launcher search changed");
        let scope = if state.open && state.mode == LauncherMode::Files {
            Scope::LauncherFiles(state.scope.clone())
        } else {
            Scope::Applications
        };
        let query = state.query.clone();
        self.note_search(scope, query);
        if let Some(hosted) = &self.launcher {
            hosted.request_redraw();
        }
    }

    pub(super) fn launcher_selected_path(&self) -> Option<PathBuf> {
        match self.launcher_screen.selected_target(&self.launcher_rows()) {
            Some(LauncherTarget::File(path) | LauncherTarget::Directory(path)) => Some(path),
            _ => None,
        }
    }

    fn app_entry(&self, id: &str) -> Option<&crate::app_search::AppEntry> {
        crate::rail_model::application_for_id(id, &self.apps, &self.app_variants)
    }

    fn app_matches(&self, wanted: &str, actual: &AppId) -> bool {
        crate::rail_model::matches_application(
            wanted,
            actual.as_str(),
            &self.apps,
            &self.app_variants,
        )
    }

    fn app_hidden(&self, id: &str) -> bool {
        self.launcher_preferences
            .visibility
            .hidden_apps
            .contains(id)
            || self.app_entry(id).is_some_and(|entry| {
                self.launcher_preferences
                    .visibility
                    .hidden_apps
                    .contains(entry.id.as_str())
                    || self
                        .launcher_preferences
                        .visibility
                        .hidden_apps
                        .contains(&entry.logical)
            })
    }

    fn target_identity(&self, target: &LauncherTarget) -> Option<String> {
        match target {
            LauncherTarget::App(app) => Some(launcher::app_identity(app.as_str())),
            LauncherTarget::File(path) | LauncherTarget::Directory(path) => {
                Some(launcher::path_identity(path))
            }
            LauncherTarget::Favorite(id) => self
                .launcher_preferences
                .favorites
                .iter()
                .find(|favorite| &favorite.id == id)
                .map(|favorite| launcher::app_identity(favorite.target.app_id())),
        }
    }

    fn label_for(&self, target: &LauncherTarget, default: &str) -> String {
        self.target_identity(target)
            .and_then(|identity| self.launcher_preferences.aliases.get(&identity).cloned())
            .unwrap_or_else(|| default.to_owned())
    }

    fn running_window(&self, id: &str) -> Option<templar_window::ShellToplevel> {
        self.bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default()
            .into_iter()
            .filter(|window| {
                window.verified_identity
                    && matches!(
                        window.layer,
                        templar_window::Layer::Normal | templar_window::Layer::AppOnTop
                    )
                    && window
                        .canonical_app_id
                        .as_ref()
                        .is_some_and(|actual| self.app_matches(id, actual))
            })
            .min_by_key(|window| (window.mru_rank, window.window_id))
    }

    fn profile_window(
        &self,
        favorite: &Favorite,
        toplevels: &[templar_window::ShellToplevel],
    ) -> Option<templar_window::WindowId> {
        let FavoriteTarget::Terminal {
            app_id,
            cwd_b64,
            arguments,
        } = &favorite.target
        else {
            return None;
        };
        let cwd = launcher::decode_path(cwd_b64).ok()?;
        let launched = self
            .favorite_instances
            .get(&profile_key(&favorite.id, &favorite.target))
            .and_then(|id| templar_types::InstanceId::parse(id));
        toplevels
            .iter()
            .filter(|window| {
                window.verified_identity
                    && matches!(
                        window.layer,
                        templar_window::Layer::Normal | templar_window::Layer::AppOnTop
                    )
                    && window
                        .canonical_app_id
                        .as_ref()
                        .is_some_and(|actual| self.app_matches(app_id, actual))
            })
            .filter(|window| {
                window.instance_id.is_some_and(|instance| {
                    launched == Some(instance)
                        || arguments.is_empty()
                            && self.working_directories.get(&instance.to_string()) == Some(&cwd)
                })
            })
            .min_by_key(|window| (window.mru_rank, window.window_id))
            .map(|window| window.window_id)
    }

    pub(super) fn profile_windows(
        &self,
        toplevels: &[templar_window::ShellToplevel],
    ) -> HashMap<String, templar_window::WindowId> {
        self.launcher_preferences
            .favorites
            .iter()
            .filter_map(|favorite| {
                self.profile_window(favorite, toplevels)
                    .map(|window| (favorite.id.clone(), window))
            })
            .collect()
    }

    fn app_row(
        &self,
        entry: &crate::app_search::AppEntry,
        section: Option<&'static str>,
    ) -> LauncherRow {
        let target = LauncherTarget::App(entry.id.clone());
        let running = self.running_window(entry.id.as_str()).is_some();
        LauncherRow {
            label: self.label_for(&target, entry.label()),
            target,
            detail: if running {
                "Activate existing window".into()
            } else {
                entry.desktop_file_id.clone()
            },
            icon_id: Some(entry.id.to_string()),
            running,
            section,
            size_bytes: None,
            mtime_ms: None,
        }
    }

    fn favorite_row(&self, favorite: &Favorite, section: Option<&'static str>) -> LauncherRow {
        let target = LauncherTarget::Favorite(favorite.id.clone());
        let detail = match &favorite.target {
            FavoriteTarget::App { app_id } => self
                .app_entry(app_id)
                .map_or_else(|| app_id.clone(), |entry| entry.desktop_file_id.clone()),
            FavoriteTarget::Terminal { cwd_b64, .. } => launcher::decode_path(cwd_b64).map_or_else(
                |_| "Directory unavailable".into(),
                |path| format!("Opens in {}", path.display()),
            ),
        };
        let running = match &favorite.target {
            FavoriteTarget::App { app_id } => self.running_window(app_id).is_some(),
            FavoriteTarget::Terminal { .. } => self
                .profile_windows(
                    &self
                        .bridge
                        .as_ref()
                        .map(ShellBridge::toplevels)
                        .unwrap_or_default(),
                )
                .contains_key(&favorite.id),
        };
        LauncherRow {
            target,
            label: if favorite.label.is_empty() {
                self.app_entry(favorite.target.app_id()).map_or_else(
                    || favorite.target.app_id().to_owned(),
                    |entry| entry.label().to_owned(),
                )
            } else {
                favorite.label.clone()
            },
            detail,
            icon_id: Some(favorite.target.app_id().into()),
            running,
            section,
            size_bytes: None,
            mtime_ms: None,
        }
    }

    pub(super) fn launcher_rows(&self) -> Vec<Option<LauncherRow>> {
        let state = self.launcher_screen.state();
        if state.mode == LauncherMode::Apps {
            // Saved copies of the same logical application are one launch target.
            // Terminal profiles keep their separate cwd/argv identity.
            let mut saved_apps = HashSet::new();
            let favorites: Vec<_> = self
                .launcher_preferences
                .favorites
                .iter()
                .filter(|favorite| {
                    !matches!(favorite.target, FavoriteTarget::App { .. })
                        || saved_apps.insert(self.app_entry(favorite.target.app_id()).map_or_else(
                            || favorite.target.app_id().to_owned(),
                            |app| {
                                if app.logical.is_empty() {
                                    app.id.to_string()
                                } else {
                                    app.logical.clone()
                                }
                            },
                        ))
                })
                .collect();
            let mut rows = Vec::new();
            if state.query.trim().is_empty() {
                let mut seen = HashSet::new();
                for favorite in &favorites {
                    if self.app_hidden(favorite.target.app_id()) {
                        continue;
                    }
                    rows.push(Some(self.favorite_row(favorite, Some("FAVORITES"))));
                    if matches!(favorite.target, FavoriteTarget::App { .. }) {
                        seen.insert(self.app_entry(favorite.target.app_id()).map_or_else(
                            || favorite.target.app_id().to_owned(),
                            |entry| entry.id.to_string(),
                        ));
                    }
                }
                for recent in self.recent.ids() {
                    if let Some(entry) = self.app_entry(recent)
                        && entry.offerable()
                        && !self.app_hidden(entry.id.as_str())
                        && seen.insert(entry.id.to_string())
                    {
                        rows.push(Some(self.app_row(entry, Some("RECENT APPLICATIONS"))));
                    }
                }
                if rows.is_empty() {
                    rows.extend(
                        crate::app_search::all_offerable(&self.apps)
                            .into_iter()
                            .filter(|hit| !self.app_hidden(hit.entry.id.as_str()))
                            .map(|hit| Some(self.app_row(hit.entry, Some("APPLICATIONS")))),
                    );
                }
                return rows;
            }
            // Feed aliases into the existing matcher while retaining original names as keywords.
            let searchable: Vec<_> = self
                .apps
                .iter()
                .filter(|entry| !self.app_hidden(entry.id.as_str()))
                .cloned()
                .map(|mut entry| {
                    if let Some(alias) = self
                        .launcher_preferences
                        .aliases
                        .get(&launcher::app_identity(entry.id.as_str()))
                    {
                        entry.keywords.push(entry.name.clone());
                        entry.name.clone_from(alias);
                    }
                    entry
                })
                .collect();
            let mut ranked: Vec<_> =
                crate::app_search::search(&searchable, &state.query, self.recent.ids())
                    .into_iter()
                    .map(|hit| (hit.kind, hit.score, self.app_row(hit.entry, None)))
                    .collect();
            for favorite in &favorites {
                if self.app_hidden(favorite.target.app_id()) {
                    continue;
                }
                if let Some(entry) = self.app_entry(favorite.target.app_id()) {
                    let mut profile = entry.clone();
                    if !favorite.label.is_empty() {
                        profile.keywords.push(profile.name.clone());
                        profile.name.clone_from(&favorite.label);
                    }
                    let profiles = [profile];
                    if let Some(hit) =
                        crate::app_search::search(&profiles, &state.query, self.recent.ids())
                            .first()
                    {
                        if matches!(favorite.target, FavoriteTarget::App { .. }) {
                            ranked.retain(|(_, _, row)| !matches!(&row.target, LauncherTarget::App(id) if self.app_matches(favorite.target.app_id(), id)));
                        }
                        ranked.push((hit.kind, hit.score, self.favorite_row(favorite, None)));
                    }
                }
            }
            ranked.sort_by_key(|(kind, score, row)| {
                let app = match &row.target {
                    LauncherTarget::App(id) => id.as_str(),
                    LauncherTarget::Favorite(id) => self
                        .launcher_preferences
                        .favorites
                        .iter()
                        .find(|favorite| &favorite.id == id)
                        .map_or("", |favorite| favorite.target.app_id()),
                    _ => "",
                };
                let canonical = self.app_entry(app).map_or(app, |entry| entry.id.as_str());
                (
                    *kind,
                    self.recent
                        .ids()
                        .iter()
                        .position(|id| id == canonical)
                        .unwrap_or(usize::MAX),
                    std::cmp::Reverse(*score),
                    row.label.chars().count(),
                    row.label.to_lowercase(),
                    canonical.to_owned(),
                )
            });
            return ranked.into_iter().map(|(_, _, row)| Some(row)).collect();
        }
        if state.query.trim().is_empty() {
            let mut seen = HashSet::new();
            let mut rows = Vec::new();
            if let Some(feed) = self.drawer_feeds.get(&Drawer::Recent) {
                for recent in &feed.recents {
                    if state
                        .scope
                        .as_ref()
                        .is_some_and(|root| !recent.path.starts_with(root))
                        || !seen.insert(recent.path.clone())
                    {
                        continue;
                    }
                    if let Some(row) =
                        self.file_row(&recent.path, false, None, None, Some("RECENT FILES"))
                    {
                        rows.push(Some(row));
                    }
                }
            }
            for pinned in &self.browser_sources.pinned {
                if let Some(path) = pinned.location.directory()
                    && seen.insert(path.to_path_buf())
                    && !state
                        .scope
                        .as_ref()
                        .is_some_and(|root| !path.starts_with(root))
                {
                    if let Some(row) = self.file_row(path, true, None, None, Some("FOLDERS")) {
                        rows.push(Some(row));
                    }
                }
            }
            return rows;
        }
        let Some((scope, query, Ok(batch))) = &self.search_results else {
            return Vec::new();
        };
        if !self.search_scope_current(scope, query) {
            return Vec::new();
        }
        self.launcher_file_slots()
            .into_iter()
            .map(|index| {
                batch.pages.entry(index).and_then(|entry| {
                    self.file_row(
                        &entry.path,
                        entry.is_dir(),
                        Some(entry.size_bytes),
                        Some(entry.mtime_ms),
                        None,
                    )
                })
            })
            .collect()
    }

    // Keep unknown slots for incremental paging, but remove known hidden results from the view.
    pub(super) fn launcher_file_slots(&self) -> Vec<usize> {
        let Some((scope, query, Ok(batch))) = &self.search_results else {
            return Vec::new();
        };
        if !self.search_scope_current(scope, query) {
            return Vec::new();
        }
        (0..batch.pages.retained as usize)
            .filter(|index| {
                match batch
                    .pages
                    .pages
                    .get(&(*index as u32 / crate::surface::search_pages::PAGE_SIZE))
                {
                    None => true,
                    Some(page) => page
                        .get(*index % crate::surface::search_pages::PAGE_SIZE as usize)
                        .and_then(Option::as_ref)
                        .is_some_and(|entry| {
                            !self
                                .launcher_preferences
                                .visibility
                                .hidden_paths_b64
                                .contains(&launcher::encode_path(&entry.path))
                        }),
                }
            })
            .collect()
    }

    fn file_row(
        &self,
        path: &Path,
        directory: bool,
        size_bytes: Option<u64>,
        mtime_ms: Option<i64>,
        section: Option<&'static str>,
    ) -> Option<LauncherRow> {
        if self
            .launcher_preferences
            .visibility
            .hidden_paths_b64
            .contains(&launcher::encode_path(path))
        {
            return None;
        }
        let target = if directory {
            LauncherTarget::Directory(path.to_path_buf())
        } else {
            LauncherTarget::File(path.to_path_buf())
        };
        let label = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Some(LauncherRow {
            label: self.label_for(&target, &label),
            target,
            detail: path.display().to_string(),
            icon_id: None,
            running: false,
            section,
            size_bytes,
            mtime_ms,
        })
    }

    pub(super) fn activate_launcher_target(&mut self, target: LauncherTarget, intent: Intent) {
        tracing::debug!(?target, ?intent, "activating launcher target");
        match target {
            LauncherTarget::App(id) => {
                if intent != Intent::LaunchNew
                    && let Some(window) = self.running_window(id.as_str())
                {
                    if intent == Intent::ShowAll {
                        self.open_taskfield(
                            &templar_window::taskfield::TaskFilter::Windows(
                                self.bridge
                                    .as_ref()
                                    .map(ShellBridge::toplevels)
                                    .unwrap_or_default()
                                    .into_iter()
                                    .filter(|window| {
                                        window.verified_identity
                                            && window.canonical_app_id.as_ref().is_some_and(
                                                |actual| self.app_matches(id.as_str(), actual),
                                            )
                                    })
                                    .map(|window| window.window_id)
                                    .collect(),
                            ),
                            &self.app_label(&id),
                        );
                    } else {
                        self.perform(FieldAction::Activate(window.window_id));
                    }
                } else if intent != Intent::ShowAll {
                    self.record_recent(&id);
                    self.spawn_launch(id);
                }
            }
            LauncherTarget::File(path) => self.spawn_open_file(path),
            LauncherTarget::Directory(path) => self.open_in_file_manager(&path),
            LauncherTarget::Favorite(id) => {
                let Some(favorite) = self
                    .launcher_preferences
                    .favorites
                    .iter()
                    .find(|favorite| favorite.id == id)
                    .cloned()
                else {
                    return;
                };
                self.activate_launch_profile(favorite, intent);
            }
        }
    }

    pub(super) fn activate_launch_profile(&mut self, favorite: Favorite, intent: Intent) {
        if let Err(error) = favorite.validate() {
            self.field_screen
                .report(error, self.started.elapsed().as_millis() as u64);
            return;
        }
        let id = favorite.id.clone();
        let matching = self.profile_window(
            &favorite,
            &self
                .bridge
                .as_ref()
                .map(ShellBridge::toplevels)
                .unwrap_or_default(),
        );
        let target = favorite.target.clone();
        match favorite.target {
            FavoriteTarget::App { app_id } => {
                if let Ok(id) = app_id.parse() {
                    self.activate_launcher_target(LauncherTarget::App(id), intent);
                }
            }
            FavoriteTarget::Terminal {
                app_id,
                cwd_b64,
                arguments,
            } => {
                if intent != Intent::LaunchNew
                    && let Some(window) = matching
                {
                    if intent == Intent::ShowAll {
                        self.open_taskfield(
                            &templar_window::taskfield::TaskFilter::Windows(vec![window]),
                            &favorite.label,
                        );
                    } else {
                        self.perform(FieldAction::Activate(window));
                    }
                    return;
                }
                if intent == Intent::ShowAll {
                    return;
                }
                let Ok(directory) = launcher::decode_path(&cwd_b64) else {
                    return;
                };
                let tx = self.data_tx.clone();
                let wake = self.wake_proxy.clone();
                self.runtime.spawn(async move {
                    let result: anyhow::Result<String> = async {
                        let connection =
                            crate::data::session().await.map_err(anyhow::Error::msg)?;
                        let proxy = LauncherProxy::new(&connection).await?;
                        Ok(proxy
                            .launch_in_directory_v1(
                                &app_id,
                                arguments,
                                &templar_types::uri_list::encode_path(&directory),
                                "",
                            )
                            .await?)
                    }
                    .await;
                    match result {
                        Ok(instance) => {
                            let _ = tx.send(DataUpdate::FavoriteLaunched {
                                id,
                                target,
                                instance,
                            });
                        }
                        Err(error) => {
                            let _ = tx.send(DataUpdate::Notice(format!(
                                "Could not open {}: {error:#}",
                                favorite.label
                            )));
                        }
                    }
                    let _ = wake.send_event(Wake::Data);
                });
            }
        }
    }

    fn mutate_launcher(&mut self, mutation: LauncherMutation) {
        let mut preferences = self.launcher_preferences.clone();
        if let Err(error) = preferences.apply(&mutation) {
            self.launcher_screen.show_info(error);
            return;
        }
        self.launcher_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = self.launcher_mutations.try_send(mutation) {
            self.launcher_screen
                .show_info(format!("Launcher changes are busy: {error}"));
            return;
        }
        self.launcher_preferences = preferences;
        self.bar_screen.reset();
        if let Some(bar) = &self.bar {
            bar.request_redraw();
        }
        if let Some(hosted) = &self.launcher {
            hosted.request_redraw();
        }
    }

    fn context_launcher(&mut self, id: &'static str, target: LauncherTarget) {
        match id {
            "launcher.pin-rail" => {
                let app = match &target {
                    LauncherTarget::App(app) => Some(app.to_string()),
                    LauncherTarget::Favorite(_) => None,
                    _ => None,
                };
                if let Some(app_id) = app {
                    if self.launcher_preferences.favorites.iter().any(|favorite| matches!(&favorite.target, FavoriteTarget::App { app_id: pinned } if pinned.parse().is_ok_and(|actual| self.app_matches(&app_id, &actual)))) { return }
                    let label = self
                        .app_entry(&app_id)
                        .map_or_else(|| app_id.clone(), |entry| entry.label().to_owned());
                    self.mutate_launcher(LauncherMutation::UpsertFavorite {
                        favorite: Favorite {
                            id: format!("app-{app_id}"),
                            label,
                            target: FavoriteTarget::App { app_id },
                        },
                    });
                }
            }
            "launcher.add-home" => match target {
                LauncherTarget::App(app) => self.perform(FieldAction::PinApp(app)),
                LauncherTarget::Favorite(id) => {
                    if let Some(favorite) = self
                        .launcher_preferences
                        .favorites
                        .iter()
                        .find(|favorite| favorite.id == id)
                        .cloned()
                    {
                        let mut doc = self.field.clone();
                        let ops = field_model::pin_launch_profile(&mut doc, &favorite);
                        if !ops.is_empty() {
                            self.perform(FieldAction::Apply(FieldOp::Batch { ops }));
                        }
                    }
                }
                LauncherTarget::File(path) | LauncherTarget::Directory(path) => {
                    let mut doc = self.field.clone();
                    let ops = field_model::route_file_drop(&mut doc, &path);
                    if !ops.is_empty() {
                        self.perform(FieldAction::Apply(FieldOp::Batch { ops }));
                    }
                }
            },
            "launcher.hide" | "launcher.restore" => {
                if let Some(identity) = self.target_identity(&target) {
                    self.mutate_launcher(LauncherMutation::SetHidden {
                        identity,
                        hidden: id == "launcher.hide",
                    });
                }
            }
            "launcher.info" => {
                let row = self
                    .launcher_rows()
                    .into_iter()
                    .flatten()
                    .find(|row| row.target == target);
                let detail = row.map_or_else(String::new, |row| {
                    let app = row.icon_id.as_deref().and_then(|id| self.app_entry(id));
                    let provenance = app
                        .and_then(|entry| {
                            self.app_variants.get(&entry.logical).and_then(|variants| {
                                variants.iter().find(|variant| {
                                    variant.0 == row.icon_id.as_deref().unwrap_or(entry.id.as_str())
                                })
                            })
                        })
                        .and_then(|variant| {
                            serde_json::from_str::<templar_types::variant::AppProvenance>(
                                &variant.2,
                            )
                            .ok()
                        });
                    if let Some(app) = app {
                        format!(
                            "{}\nApp ID: {}\nDesktop entry: {}\nInstallation: {}",
                            row.label,
                            app.id,
                            app.desktop_file_id,
                            provenance.map_or_else(
                                || "Catalogue provenance unavailable".into(),
                                |source| format!(
                                    "{}\n{}\n{}",
                                    source.origin.as_str(),
                                    source.source_path.map_or_else(String::new, |path| path
                                        .display()
                                        .to_string()),
                                    source.package_ref.unwrap_or_default()
                                )
                            )
                        )
                    } else {
                        let modified = row
                            .mtime_ms
                            .and_then(chrono::DateTime::from_timestamp_millis)
                            .map_or_else(
                                || "Unknown".into(),
                                |time| time.format("%Y-%m-%d %H:%M UTC").to_string(),
                            );
                        format!(
                            "{}\n{}\nType: {}\nSize: {}\nModified: {}",
                            row.label,
                            row.detail,
                            if matches!(row.target, LauncherTarget::Directory(_)) {
                                "Folder"
                            } else {
                                "File"
                            },
                            row.size_bytes
                                .map_or_else(|| "Unknown".into(), crate::format::bytes),
                            modified
                        )
                    }
                });
                let identity = match &target {
                    LauncherTarget::Favorite(id) => self
                        .launcher_preferences
                        .favorites
                        .iter()
                        .find(|favorite| &favorite.id == id)
                        .map_or_else(String::new, |favorite| match &favorite.target {
                            FavoriteTarget::App { app_id } => format!("App ID: {app_id}"),
                            FavoriteTarget::Terminal {
                                app_id,
                                cwd_b64,
                                arguments,
                            } => format!(
                                "App ID: {app_id}\nDirectory: {}\nArguments: {}",
                                launcher::decode_path(cwd_b64).map_or_else(
                                    |_| "Unavailable".into(),
                                    |path| templar_types::uri_list::encode_path(&path)
                                ),
                                serde_json::to_string(arguments).unwrap_or_default()
                            ),
                        }),
                    LauncherTarget::File(path) | LauncherTarget::Directory(path) => format!(
                        "{}\n{}",
                        path.display(),
                        templar_types::uri_list::encode_path(path)
                    ),
                    _ => self.target_identity(&target).unwrap_or_default(),
                };
                self.launcher_screen
                    .show_info(format!("{detail}\n{identity}"));
            }
            _ => {}
        }
    }

    pub(super) fn redraw_launcher(&mut self) {
        if !self.launcher_screen.state().open {
            return;
        }
        self.flush_search();
        self.fetch_search_pages();
        let rows = self.launcher_rows();
        for icon in rows
            .iter()
            .flatten()
            .filter_map(|row| row.icon_id.as_ref())
            .take(LAUNCHER_ICON_LOOKAHEAD)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.ensure_icon(&icon);
        }
        let scope = Scope::LauncherFiles(self.launcher_screen.state().scope.clone());
        let active = self
            .search_results
            .as_ref()
            .filter(|(owner, query, _)| *owner == scope && self.search_scope_current(owner, query));
        let notice = active.map(|(_, _, result)| match result {
            Ok(batch) => batch.pages.notice.as_deref().unwrap_or(&batch.notice),
            Err(error) => error.as_str(),
        });
        let loading = self.launcher_screen.state().mode == LauncherMode::Files
            && !self.launcher_screen.state().query.trim().is_empty()
            && active.is_none();
        let Some(hosted) = &mut self.launcher else {
            return;
        };
        if !hosted.prepare() {
            return;
        }
        hosted.begin_semantics("Launcher");
        let width = hosted.buffer.width as i32;
        let height = hosted.buffer.height as i32;
        let mut ui = Ui::new(
            if self.bridge.is_some() {
                Painter::new_premultiplied(&mut hosted.buffer)
            } else {
                Painter::new(&mut hosted.buffer)
            },
            &mut self.text,
            &mut hosted.input,
            self.theme,
            hosted.scale,
            &mut self.kernel,
        )
        .with_semantics(&mut hosted.semantics);
        let actions = self.launcher_screen.draw(
            &mut ui,
            Rect::new(0, 0, width, height),
            &LauncherData {
                rows: &rows,
                icons: &self.icons,
                notice,
                loading,
                now_ms: self.started.elapsed().as_millis() as u64,
            },
        );
        drop(ui);
        hosted.update_ime(self.launcher_screen.ime_caret());
        hosted.publish_semantics();
        hosted.input.end_frame();
        let committed = if let Some(bridge) = &mut self.bridge {
            let logical = hosted
                .window
                .inner_size()
                .to_logical::<f64>(hosted.window.scale_factor());
            match bridge.present_premultiplied(
                &hosted.window,
                &hosted.buffer.pixels,
                &[(
                    0,
                    0,
                    logical.width.ceil() as i32,
                    logical.height.ceil() as i32,
                )],
                hosted.buffer.width,
                hosted.buffer.height,
            ) {
                Ok(PresentOutcome::Submitted) => {
                    hosted.pending = false;
                    true
                }
                Ok(PresentOutcome::Deferred) => {
                    hosted.pending = true;
                    false
                }
                Err(error) => {
                    tracing::warn!(%error, "presenting launcher");
                    false
                }
            }
        } else {
            hosted.present();
            true
        };
        if committed && !self.launcher_focus_claimed {
            if let Some(bridge) = &self.bridge {
                if bridge.focus_surface(Some(&hosted.window)).is_ok() {
                    self.launcher_focus_claimed = true;
                }
            } else {
                hosted.window.focus_window();
                self.launcher_focus_claimed = true;
            }
        }
        for action in actions {
            match action {
                LauncherAction::Dismiss => self.dismiss_launcher(),
                LauncherAction::QueryChanged | LauncherAction::SwitchMode => {
                    self.schedule_launcher_search()
                }
                LauncherAction::Open { target, intent } => {
                    self.dismiss_launcher();
                    self.activate_launcher_target(target, intent);
                }
                LauncherAction::Context { id, target } => self.context_launcher(id, target),
                LauncherAction::SetAlias { target, label } => {
                    if let LauncherTarget::Favorite(id) = &target {
                        if let Some(mut favorite) = self
                            .launcher_preferences
                            .favorites
                            .iter()
                            .find(|favorite| &favorite.id == id)
                            .cloned()
                        {
                            favorite.label = label;
                            self.mutate_launcher(LauncherMutation::UpsertFavorite { favorite });
                        }
                    } else if let Some(identity) = self.target_identity(&target) {
                        self.mutate_launcher(LauncherMutation::SetAlias {
                            identity,
                            label: Some(label),
                        });
                    }
                }
            }
        }
        self.fetch_search_pages();
    }

    pub(super) fn launcher_event(&mut self, event: &WindowEvent) {
        if matches!(event, WindowEvent::Focused(false)) && self.launcher_focus_claimed {
            self.dismiss_launcher();
            return;
        }
        if matches!(event, WindowEvent::CloseRequested) {
            self.dismiss_launcher();
            return;
        }
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw_launcher();
            return;
        }
        let Some(hosted) = &mut self.launcher else {
            return;
        };
        hosted.process_accessibility_event(event);
        hosted.invalidate_semantics();
        match event {
            WindowEvent::Focused(true) => {
                self.launcher_focus_claimed = true;
                self.launcher_screen.focus_search();
            }
            WindowEvent::Ime(winit::event::Ime::Preedit(text, _)) => {
                hosted.input.set_composition(text)
            }
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                hosted.input.set_composition("");
                for character in text.chars() {
                    hosted
                        .input
                        .key(pixelkit_shell::KeyInput::Character(character));
                }
            }
            WindowEvent::Ime(winit::event::Ime::Disabled) => hosted.input.set_composition(""),
            WindowEvent::CursorMoved { position, .. } => hosted
                .input
                .cursor_moved(position.x as f32, position.y as f32),
            WindowEvent::CursorLeft { .. } => hosted.input.cursor_left(),
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = translate_button(*button) {
                    hosted.input.mouse(button, *state == ElementState::Pressed);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = scroll_pixels(*delta);
                hosted.input.scrolled(x, y);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if key_pressed(event.state) {
                    for key in translate_keys(&event.logical_key, event.text.as_deref()) {
                        hosted.input.key(key);
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                hosted.input.modifiers_changed(modifiers_of(*modifiers))
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                hosted.scale = pixelkit_shell::Scale::new(*scale_factor)
            }
            _ => {}
        }
        hosted.request_redraw();
    }

    pub(super) fn relay_window_transition(&mut self) {
        if self.window_transition_relayed == Some(self.window_transition) {
            return;
        }
        if let Some(bridge) = &self.bridge {
            match bridge.set_window_transition(self.window_transition.0, self.window_transition.1) {
                Ok(()) => self.window_transition_relayed = Some(self.window_transition),
                Err(error) => tracing::debug!(%error, "window transitions require compositor v15"),
            }
        }
    }

    pub(super) fn record_activation_recency(
        &mut self,
        toplevels: &[templar_window::ShellToplevel],
    ) {
        let active = toplevels.iter().find(|window| {
            window.activated
                && window.verified_identity
                && matches!(
                    window.layer,
                    templar_window::Layer::Normal | templar_window::Layer::AppOnTop
                )
        });
        let id = active.map(|window| window.window_id);
        if id == self.last_recent_activation {
            return;
        }
        self.last_recent_activation = id;
        if let Some(app) = active.and_then(|window| window.canonical_app_id.as_ref()) {
            let canonical = self
                .app_entry(app.as_str())
                .map_or_else(|| app.clone(), |entry| entry.id.clone());
            self.record_recent(&canonical);
        }
    }
}
