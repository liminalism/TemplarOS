//! Index policy belongs to filed; Settings retains only observed state and an explicit lease.
use std::{collections::HashMap, path::PathBuf};
use anyhow::Result;
use templar_client::{file_index::{self, IndexRootPolicy, IndexRoots}, proxies::{FilesBrowseProxy, FilesIndexProxy}};

#[derive(Debug, Default)]
pub struct FilesState {
    pub roots: Option<IndexRoots>,
    pub policies: HashMap<i64, IndexRootPolicy>,
    pub message: Option<String>,
}

#[derive(Debug)]
pub enum FilesCommand {
    Refresh,
    ProducerRestart,
    Observe(bool),
    Add(PathBuf),
    Policy { path: PathBuf, expected_hash: String, policy: IndexRootPolicy },
    Remove { path: PathBuf, expected_hash: String },
    Reconcile { root: i64, cache_identity: String },
    ExportBookmarks,
}

pub struct FilesController {
    connection: Option<zbus::Connection>,
    observing: bool,
    observation: u64,
}

impl Default for FilesController {
    fn default() -> Self {
        Self { connection: None, observing: false, observation: file_index::observation_token() }
    }
}

impl FilesController {
    pub async fn run(&mut self, command: FilesCommand) -> FilesState {
        if let FilesCommand::Observe(observing) = &command { self.observing = *observing; }
        if self.connection.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => return FilesState { message: Some(format!("Index service unavailable — {error}")), ..FilesState::default() },
            }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation = async {
            let proxy = FilesIndexProxy::new(connection).await?;
            if matches!(command, FilesCommand::ProducerRestart) {
                let old = std::mem::replace(&mut self.observation, file_index::observation_token());
                let _ = proxy.observe(false, old).await;
            }
            proxy.observe(self.observing, self.observation).await?;
            match command {
                FilesCommand::Refresh | FilesCommand::Observe(_) | FilesCommand::ProducerRestart => Ok(None),
                FilesCommand::Add(path) => {
                    anyhow::ensure!(path.is_absolute(), "Choose an absolute directory path");
                    file_index::configure_root(&proxy, &path, &IndexRootPolicy::default_home(), "absent").await?;
                    Ok(Some("Root registered; reconciliation is observed below".to_owned()))
                }
                FilesCommand::Policy { path, expected_hash, policy } => {
                    file_index::configure_root(&proxy, &path, &policy, &expected_hash).await?;
                    Ok(Some("Policy stored; results remain stale until reconciliation completes".to_owned()))
                }
                FilesCommand::Remove { path, expected_hash } => {
                    proxy.remove_root(&file_index::encode_path(&path), &expected_hash).await?;
                    Ok(Some("Root removed from durable policy; deleting the cache will not restore it".to_owned()))
                }
                FilesCommand::Reconcile { root, cache_identity } => {
                    proxy.reconcile(&serde_json::to_string(&serde_json::json!({
                        "cache_identity": cache_identity, "roots": [root],
                    }))?).await?;
                    Ok(Some("Reconciliation requested; completion is observed below".to_owned()))
                }
                FilesCommand::ExportBookmarks => {
                    let proxy = FilesBrowseProxy::new(connection).await?;
                    Ok(Some(proxy.export_bookmarks().await?))
                }
            }
        }.await;
        let observed = read(connection).await;
        match observed {
            Ok(mut state) => {
                state.message = match operation {
                    Ok(message) => message,
                    Err(error) => Some(format!("Could not change it — {error:#}")),
                };
                state
            }
            Err(error) => FilesState { message: Some(match operation {
                Err(write) => format!("Could not change it — {write:#}; readback unavailable — {error:#}"),
                Ok(_) => format!("Index state unavailable — {error:#}"),
            }), ..FilesState::default() },
        }
    }
}

async fn read(connection: &zbus::Connection) -> Result<FilesState> {
    let proxy = FilesIndexProxy::new(connection).await?;
    let roots = file_index::roots(&proxy).await?;
    let mut policies = HashMap::with_capacity(roots.roots.len());
    for root in &roots.roots {
        let policy = file_index::root_policy(&proxy, &root.native_path()?, &root.policy_hash).await?;
        anyhow::ensure!(policy.hash() == root.policy_hash, "Policy changed while reading; wait for the next observation before editing");
        policies.insert(root.root, policy);
    }
    Ok(FilesState { roots: Some(roots), policies, message: None })
}

pub async fn print() -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let state = read(&connection).await?;
    if let Some(roots) = state.roots {
        for root in roots.roots {
            println!("{} — {} {} · generation {} · {} entries", root.display, root.state,
                root.reason.as_deref().unwrap_or(""), root.generation, root.entry_count);
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct FilesView {
    pub state: FilesState,
    pub root_draft: pixelkit_ui::TextFieldState,
    pub exclude_draft: pixelkit_ui::TextFieldState,
    selected: Option<i64>,
    scroll: pixelkit_ui::ScrollState,
    heights: Vec<i32>,
    summaries: HashMap<i64, String>,
    rule_labels: HashMap<i64, Vec<String>>,
    exclude_drawn: bool,
}

impl FilesView {
    pub fn replace(&mut self, state: FilesState) -> bool {
        let layout_changed = match (&self.state.roots, &state.roots) {
            (Some(old), Some(new)) => old.roots.len() != new.roots.len()
                || old.roots.iter().zip(&new.roots).any(|(a, b)| a.root != b.root
                    || a.path_b64 != b.path_b64
                    || self.state.policies.get(&a.root).map(|policy| &policy.coverage_rules)
                        != state.policies.get(&b.root).map(|policy| &policy.coverage_rules)),
            (None, None) => false,
            _ => true,
        };
        self.summaries.clear();
        self.rule_labels.clear();
        if let Some(roots) = &state.roots {
            for root in &roots.roots {
                self.summaries.insert(root.root, format!("{} {} · generation {} · {} entries · {}",
                    root.state, root.reason.as_deref().unwrap_or(""), root.generation, root.entry_count,
                    if root.complete { "complete" } else { "incomplete" }));
                if let Some(policy) = state.policies.get(&root.root) {
                    self.rule_labels.insert(root.root, policy.coverage_rules.iter().map(|rule| {
                        use templar_file_index::CoverageMatch;
                        let name = match &rule.matcher {
                            CoverageMatch::Name { name } => name.clone(),
                            CoverageMatch::Trash => "Trash directories".to_owned(),
                            CoverageMatch::Subtree { path } => path.as_path().display().to_string(),
                        };
                        format!("{name}: {}", rule.coverage.as_str())
                    }).collect());
                }
            }
            if !roots.roots.iter().any(|root| Some(root.root) == self.selected) {
                self.selected = roots.roots.first().map(|root| root.root);
            }
        }
        self.state = state;
        layout_changed
    }

    pub fn ime_caret(&self) -> Option<pixelkit_raster::Rect> {
        self.root_draft.caret_area().or_else(|| self.exclude_drawn.then(|| self.exclude_draft.caret_area()).flatten())
    }

    pub fn draw(&mut self, ui: &mut pixelkit_ui::Ui<'_>, area: pixelkit_raster::Rect) -> (Option<FilesCommand>, bool) {
        use pixelkit_text::Align;
        use pixelkit_raster::Rect;
        self.exclude_drawn = false;
        let mut command = None;
        let gap = ui.px(8);
        let h = ui.px(32);
        let add = Rect::new(area.right() - ui.px(72), area.y, ui.px(72), h);
        ui.text_field(&mut self.root_draft, Rect::new(area.x, area.y, area.w - add.w - gap, h),
            "/absolute/directory", false, false);
        if ui.button(add, "Add root") {
            command = Some(FilesCommand::Add(PathBuf::from(self.root_draft.text())));
        }
        let refresh = Rect::new(area.x, area.y + h + gap, ui.px(82), h);
        let export = Rect::new(refresh.right() + gap, refresh.y, ui.px(174), h);
        if ui.button(refresh, "Refresh") { command = Some(FilesCommand::Refresh); }
        if ui.button(export, "Export GTK bookmarks") { command = Some(FilesCommand::ExportBookmarks); }
        let list_area = Rect::new(area.x, export.bottom() + gap, area.w,
            (area.bottom() - export.bottom() - gap).max(0));
        let Some(roots) = &self.state.roots else {
            ui.label(list_area, self.state.message.as_deref().unwrap_or("Index service unavailable"),
                ui.theme.text_dim, Align::Left);
            return (command, false);
        };
        if roots.roots.is_empty() {
            ui.label(list_area, "No roots configured. Add one explicitly; an empty policy is preserved.",
                ui.theme.text_dim, Align::Left);
            return (command, false);
        }
        self.heights.clear();
        for root in &roots.roots {
            let rules = self.rule_labels.get(&root.root).map_or(0, Vec::len);
            self.heights.push(ui.px(if Some(root.root) == self.selected {
                286 + i32::try_from(rules).unwrap_or(i32::MAX / 40).saturating_mul(40)
            } else { 82 }));
        }
        let mut selected = self.selected;
        let previous_selected = self.selected;
        let mut exclude_drawn = false;
        ui.scroll_list(&mut self.scroll, list_area, &self.heights, |ui, index, rect| {
            let root = &roots.roots[index];
            let title = Rect::new(rect.x, rect.y, rect.w, ui.px(32));
            if ui.button(title, &root.display) { selected = Some(root.root); }
            ui.label(Rect::new(rect.x, title.bottom() + ui.px(4), rect.w, ui.px(32)),
                self.summaries.get(&root.root).map_or("State unavailable", String::as_str),
                ui.theme.text_dim, Align::Left);
            if self.selected != Some(root.root) { return; }
            let Some(policy) = self.state.policies.get(&root.root) else { return; };
            let Ok(path) = root.native_path() else { return; };
            let mut y = title.bottom() + ui.px(40);
            for (label, initial, field) in [
                ("Monitor native changes while consumers are attached", policy.monitoring, 0),
                ("Include hidden files", policy.include_hidden, 1),
                ("Cross mounted filesystems (explicit volume opt-in)", policy.cross_mount, 2),
            ] {
                let mut value = initial;
                if ui.toggle(Rect::new(rect.x, y, rect.w, ui.px(30)), &mut value, label) {
                    let mut changed = policy.clone();
                    match field { 0 => changed.monitoring = value, 1 => changed.include_hidden = value,
                        _ => changed.cross_mount = value }
                    command = Some(FilesCommand::Policy { path: path.clone(), expected_hash: root.policy_hash.clone(), policy: changed });
                }
                y += ui.px(36);
            }
            let reconcile = Rect::new(rect.x, y, ui.px(100), h);
            if ui.button(reconcile, "Reconcile") {
                command = Some(FilesCommand::Reconcile { root: root.root, cache_identity: roots.cache_identity.clone() });
            }
            if ui.button(Rect::new(reconcile.right() + gap, y, ui.px(100), h), "Remove root") {
                command = Some(FilesCommand::Remove { path: path.clone(), expected_hash: root.policy_hash.clone() });
            }
            y += ui.px(40);
            let exclude = Rect::new(rect.right() - ui.px(100), y, ui.px(100), h);
            ui.text_field(&mut self.exclude_draft, Rect::new(rect.x, y, rect.w - exclude.w - gap, h),
                "Exclude directory name", false, false);
            exclude_drawn = true;
            if ui.button(exclude, "Exclude") {
                let name = self.exclude_draft.text();
                if !name.is_empty() && !name.contains('/') && !name.contains('\0') && name != "." && name != ".." {
                    let mut changed = policy.clone();
                    changed.coverage_rules.insert(0, templar_file_index::CoverageRule::name(
                        name, templar_file_index::Coverage::Ignored));
                    command = Some(FilesCommand::Policy { path: path.clone(), expected_hash: root.policy_hash.clone(), policy: changed });
                    self.exclude_draft.clear();
                }
                else {
                    self.state.message = Some("Enter one exact directory name, without '/' or NUL; '.' and '..' are not names to exclude".to_owned());
                }
            }
            y += ui.px(40);
            if let Some(labels) = self.rule_labels.get(&root.root) {
                for (rule, label) in labels.iter().enumerate() {
                    let remove = Rect::new(rect.right() - ui.px(76), y, ui.px(76), h);
                    ui.label(Rect::new(rect.x, y, rect.w - remove.w - gap, h), label, ui.theme.text_dim, Align::Left);
                    if ui.button(remove, "Remove") {
                        let mut changed = policy.clone();
                        changed.coverage_rules.remove(rule);
                        command = Some(FilesCommand::Policy { path: path.clone(), expected_hash: root.policy_hash.clone(), policy: changed });
                    }
                    y += ui.px(40);
                }
            }
        });
        self.selected = selected;
        self.exclude_drawn = exclude_drawn;
        (command, self.selected != previous_selected)
    }
}
