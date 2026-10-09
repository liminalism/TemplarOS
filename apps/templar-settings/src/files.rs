//! Index policy belongs to filed; Settings retains only observed state and an explicit lease.
use anyhow::Result;
use std::{collections::HashMap, path::PathBuf};
use templar_client::{
    file_index::{self, IndexRootPolicy, IndexRoots, IndexVolume},
    proxies::{FilesBrowseProxy, FilesIndexProxy},
};

#[derive(Debug, Default)]
pub struct FilesState {
    pub roots: Option<IndexRoots>,
    pub policies: HashMap<i64, IndexRootPolicy>,
    pub message: Option<String>,
}

#[derive(Debug)]
pub enum FilesCommand {
    Refresh,
    RetryCache,
    ProducerRestart,
    Observe(bool),
    Add(PathBuf),
    Policy {
        path: PathBuf,
        expected_hash: String,
        policy: IndexRootPolicy,
    },
    Remove {
        path: PathBuf,
        expected_hash: String,
    },
    AcceptVolume {
        root: i64,
        observed_fingerprint: String,
    },
    Relocate {
        old_path: PathBuf,
        new_path: PathBuf,
        expected_hash: String,
        policy: IndexRootPolicy,
        expected_volume: Option<IndexVolume>,
    },
    Reconcile {
        root: i64,
        cache_identity: String,
    },
    ExportBookmarks,
}

pub struct FilesController {
    connection: Option<zbus::Connection>,
    observing: bool,
    observation: u64,
}

impl Default for FilesController {
    fn default() -> Self {
        Self {
            connection: None,
            observing: false,
            observation: file_index::observation_token(),
        }
    }
}

impl FilesController {
    pub async fn run(&mut self, command: FilesCommand) -> FilesState {
        if let FilesCommand::Observe(observing) = &command {
            self.observing = *observing;
        }
        if self.connection.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => {
                    return FilesState {
                        message: Some(format!("Index service unavailable — {error}")),
                        ..FilesState::default()
                    };
                }
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
                FilesCommand::AcceptVolume { root, observed_fingerprint } => {
                    proxy.accept_volume(root, &observed_fingerprint).await?;
                    Ok(Some("Replacement volume accepted; previous cached contents forgotten and full reconciliation queued".into()))
                }
                FilesCommand::Relocate { old_path, new_path, expected_hash, policy, expected_volume } => {
                    anyhow::ensure!(new_path.is_absolute() && new_path != old_path, "Choose a different absolute directory path");
                    // Check before registering; remove_root repeats the fence. A failure retains
                    // both explicit registrations and says so in readback; it never rewrites paths.
                    file_index::root_policy(&proxy, &old_path, &expected_hash).await?;
                    if let Some(expected) = &expected_volume {
                        let observed = file_index::roots(&proxy).await?;
                        let original = observed.roots.iter().find(|root| root.native_path().ok().as_ref() == Some(&old_path));
                        anyhow::ensure!(original.is_some_and(|root| root.state == "offline" && relocation_volume_matches(expected, &root.volume)),
                            "The original volume observation changed; refresh before registering its new location");
                    }
                    let new_root = file_index::configure_root(&proxy, &new_path, &policy, "absent").await?;
                    if let Some(expected) = &expected_volume {
                        let observed = file_index::roots(&proxy).await?;
                        let registered = observed.roots.iter().find(|root| root.root == new_root);
                        anyhow::ensure!(registered.is_some_and(|root| relocation_volume_matches(expected, &root.volume)),
                            "The volume at the new location changed; both registrations are retained and previous cached contents are preserved");
                    }
                    proxy.remove_root(&file_index::encode_path(&old_path), &expected_hash).await?;
                    Ok(Some("New location registered for a full scan; previous registration removed".into()))
                }
                FilesCommand::Reconcile { root, cache_identity } => {
                    proxy.reconcile(&serde_json::to_string(&serde_json::json!({
                        "cache_identity": cache_identity, "roots": [root],
                    }))?).await?;
                    Ok(Some("Reconciliation requested; completion is observed below".to_owned()))
                }
                FilesCommand::RetryCache => {
                    let cache_identity = file_index::roots(&proxy).await?.cache_identity;
                    proxy.reconcile(&serde_json::to_string(&serde_json::json!({"cache_identity": cache_identity, "roots": []}))?).await?;
                    Ok(Some("Cache retry requested; completion is observed below".into()))
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
            Err(error) => FilesState {
                message: Some(match operation {
                    Err(write) => {
                        format!("Could not change it — {write:#}; readback unavailable — {error:#}")
                    }
                    Ok(_) => format!("Index state unavailable — {error:#}"),
                }),
                ..FilesState::default()
            },
        }
    }
}

fn relocation_volume_matches(expected: &IndexVolume, observed: &IndexVolume) -> bool {
    if expected.confidence != templar_file_index::VolumeConfidence::Strong
        || observed.confidence != templar_file_index::VolumeConfidence::Strong
        || !expected
            .fs_uuid
            .as_ref()
            .is_some_and(|uuid| !uuid.is_empty())
        || expected.fs_uuid != observed.fs_uuid
    {
        return false;
    }
    expected
        .mount_root
        .as_ref()
        .and_then(|root| root.native_path().ok())
        .zip(
            observed
                .mount_root
                .as_ref()
                .and_then(|root| root.native_path().ok()),
        )
        .is_some_and(|(expected, observed)| expected == observed)
}

async fn read(connection: &zbus::Connection) -> Result<FilesState> {
    let proxy = FilesIndexProxy::new(connection).await?;
    let roots = file_index::roots(&proxy).await?;
    let mut policies = HashMap::with_capacity(roots.roots.len());
    for root in &roots.roots {
        let policy =
            file_index::root_policy(&proxy, &root.native_path()?, &root.policy_hash).await?;
        anyhow::ensure!(
            policy.hash() == root.policy_hash,
            "Policy changed while reading; wait for the next observation before editing"
        );
        policies.insert(root.root, policy);
    }
    Ok(FilesState {
        roots: Some(roots),
        policies,
        message: None,
    })
}

pub fn root_lines(root: &file_index::IndexRoot) -> Vec<String> {
    let watch = root.watch.as_ref().map_or_else(
        || "Watches: no observation".into(),
        |watch| {
            format!(
                "Watches: required {} · armed {} · refused {}",
                watch.required_display(),
                watch.watches_armed,
                watch.refused_display()
            )
        },
    );
    vec![
        format!(
            "{} {} · generation {} · {} entries{}",
            root.state,
            root.reason.as_deref().unwrap_or(""),
            root.generation,
            root.entry_count,
            if root.complete { "" } else { " · incomplete" }
        ),
        format!("Metadata: {}", root.metadata),
        watch,
        format!(
            "Nested in: {} · contains: {}",
            root.nested_in
                .map_or_else(|| "none".into(), |id| id.to_string()),
            if root.contains.is_empty() {
                "none".into()
            } else {
                root.contains
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
        format!(
            "Volume: {} · identity {}",
            root.volume.fs_type.as_deref().unwrap_or("unknown"),
            format!("{:?}", root.volume.confidence).to_lowercase()
        ),
    ]
}

pub fn status_lines(roots: &IndexRoots) -> Vec<String> {
    let diagnostics = &roots.diagnostics;
    let storage = diagnostics.storage;
    let accelerator = diagnostics
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.arrays.as_ref());
    vec![
        format!(
            "Cache: {} bytes (main {} · WAL {} · SHM {})",
            storage.total(),
            storage.main_bytes,
            storage.wal_bytes,
            storage.shm_bytes
        ),
        format!(
            "Availability: {}{}",
            diagnostics.availability.state,
            diagnostics
                .availability
                .reason
                .as_ref()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default()
        ),
        accelerator.map_or_else(
            || "Accelerator: unavailable or disabled".into(),
            |arrays| {
                format!(
                    "Accelerator: {} bytes retained · hit rate {}",
                    arrays.retained_bytes,
                    arrays
                        .hit_rate()
                        .map_or_else(|| "unmeasured".into(), |rate| format!("{rate:.2}%"))
                )
            },
        ),
    ]
}

pub fn render_roots(roots: &IndexRoots) -> String {
    let mut lines = status_lines(roots);
    if !roots.diagnostics.availability.is_available() {
        lines.push("Retry cache".into());
    }
    for root in &roots.roots {
        lines.push(root.display.clone());
        lines.extend(root_lines(root));
        if root.state == "offline" {
            lines.push(
                if root.observed_fingerprint.is_some() {
                    "Index this volume here (forgets previous cached contents)"
                } else {
                    "Volume absent: cached contents retained"
                }
                .into(),
            );
            lines.push("Index at new location (absolute path entered above)".into());
            if let Some(location) = &root.mounted_elsewhere {
                lines.push(format!("This volume is mounted at {}", location.display));
                lines.push("Index at observed location (new registration and full scan)".into());
            }
        }
    }
    lines.join("\n")
}

pub async fn print() -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let state = read(&connection).await?;
    if let Some(roots) = state.roots {
        println!("{}", render_roots(&roots));
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
    summaries: HashMap<i64, Vec<String>>,
    rule_labels: HashMap<i64, Vec<String>>,
    exclude_drawn: bool,
    volume_confirmation: Option<(i64, String)>,
}

impl FilesView {
    pub fn replace(&mut self, state: FilesState) -> bool {
        if self
            .volume_confirmation
            .as_ref()
            .is_some_and(|(id, fingerprint)| {
                !state.roots.as_ref().is_some_and(|roots| {
                    roots.roots.iter().any(|root| {
                        root.root == *id && root.observed_fingerprint.as_ref() == Some(fingerprint)
                    })
                })
            })
        {
            self.volume_confirmation = None;
        }
        let layout_changed = match (&self.state.roots, &state.roots) {
            (Some(old), Some(new)) => {
                old.roots.len() != new.roots.len()
                    || old.roots.iter().zip(&new.roots).any(|(a, b)| {
                        a.root != b.root
                            || a.path_b64 != b.path_b64
                            || self
                                .state
                                .policies
                                .get(&a.root)
                                .map(|policy| &policy.coverage_rules)
                                != state
                                    .policies
                                    .get(&b.root)
                                    .map(|policy| &policy.coverage_rules)
                    })
            }
            (None, None) => false,
            _ => true,
        };
        self.summaries.clear();
        self.rule_labels.clear();
        if let Some(roots) = &state.roots {
            for root in &roots.roots {
                self.summaries.insert(root.root, root_lines(root));
                if let Some(policy) = state.policies.get(&root.root) {
                    self.rule_labels.insert(
                        root.root,
                        policy
                            .coverage_rules
                            .iter()
                            .map(|rule| {
                                use templar_file_index::CoverageMatch;
                                let name = match &rule.matcher {
                                    CoverageMatch::Name { name } => name.clone(),
                                    CoverageMatch::Trash => "Trash directories".to_owned(),
                                    CoverageMatch::Subtree { path } => {
                                        path.as_path().display().to_string()
                                    }
                                };
                                format!("{name}: {}", rule.coverage.as_str())
                            })
                            .collect(),
                    );
                }
            }
            if !roots
                .roots
                .iter()
                .any(|root| Some(root.root) == self.selected)
            {
                self.selected = roots.roots.first().map(|root| root.root);
            }
        }
        self.state = state;
        layout_changed
    }

    pub fn ime_caret(&self) -> Option<pixelkit_raster::Rect> {
        self.root_draft.caret_area().or_else(|| {
            self.exclude_drawn
                .then(|| self.exclude_draft.caret_area())
                .flatten()
        })
    }

    pub fn draw(
        &mut self,
        ui: &mut pixelkit_ui::Ui<'_>,
        area: pixelkit_raster::Rect,
    ) -> (Option<FilesCommand>, bool) {
        use pixelkit_raster::Rect;
        use pixelkit_text::Align;
        self.exclude_drawn = false;
        let mut command = None;
        let gap = ui.px(8);
        let h = ui.px(32);
        let add = Rect::new(area.right() - ui.px(72), area.y, ui.px(72), h);
        ui.text_field(
            &mut self.root_draft,
            Rect::new(area.x, area.y, area.w - add.w - gap, h),
            "/absolute/directory",
            false,
            false,
        );
        if ui.button(add, "Add root") {
            command = Some(FilesCommand::Add(PathBuf::from(self.root_draft.text())));
        }
        let refresh = Rect::new(area.x, area.y + h + gap, ui.px(82), h);
        let export = Rect::new(refresh.right() + gap, refresh.y, ui.px(174), h);
        if ui.button(refresh, "Refresh") {
            command = Some(FilesCommand::Refresh);
        }
        if ui.button(export, "Export GTK bookmarks") {
            command = Some(FilesCommand::ExportBookmarks);
        }
        let mut list_area = Rect::new(
            area.x,
            export.bottom() + gap,
            area.w,
            (area.bottom() - export.bottom() - gap).max(0),
        );
        let Some(roots) = &self.state.roots else {
            ui.label(
                list_area,
                self.state
                    .message
                    .as_deref()
                    .unwrap_or("Index service unavailable"),
                ui.theme.text_dim,
                Align::Left,
            );
            return (command, false);
        };
        for line in status_lines(roots) {
            ui.label(
                Rect::new(
                    list_area.x,
                    list_area.y,
                    list_area.w - ui.px(110),
                    ui.px(28),
                ),
                &line,
                ui.theme.text_dim,
                Align::Left,
            );
            list_area.y += ui.px(28);
            list_area.h = (list_area.h - ui.px(28)).max(0);
        }
        if !roots.diagnostics.availability.is_available() {
            let retry = Rect::new(export.right() + gap, export.y, ui.px(100), h);
            if ui.button(retry, "Retry cache") {
                command = Some(FilesCommand::RetryCache);
            }
        }
        if roots.roots.is_empty() {
            ui.label(
                list_area,
                "No roots configured. Add one explicitly; an empty policy is preserved.",
                ui.theme.text_dim,
                Align::Left,
            );
            return (command, false);
        }
        self.heights.clear();
        for root in &roots.roots {
            let rules = self.rule_labels.get(&root.root).map_or(0, Vec::len);
            self.heights
                .push(ui.px(if Some(root.root) == self.selected {
                    478 + if root.state == "offline" { 80 } else { 0 }
                        + if root.state == "offline" && root.mounted_elsewhere.is_some() {
                            40
                        } else {
                            0
                        }
                        + i32::try_from(rules)
                            .unwrap_or(i32::MAX / 40)
                            .saturating_mul(40)
                } else {
                    194
                }));
        }
        let mut selected = self.selected;
        let previous_selected = self.selected;
        let mut exclude_drawn = false;
        ui.scroll_list(&mut self.scroll, list_area, &self.heights, |ui, index, rect| {
            let root = &roots.roots[index];
            let title = Rect::new(rect.x, rect.y, rect.w, ui.px(32));
            if ui.button(title, &root.display) { selected = Some(root.root); }
            if let Some(lines) = self.summaries.get(&root.root) {
                for (index, line) in lines.iter().enumerate() {
                    ui.label(Rect::new(rect.x, title.bottom() + ui.px(4 + index as i32 * 28), rect.w, ui.px(28)),
                        line, ui.theme.text_dim, Align::Left);
                }
            }
            if self.selected != Some(root.root) { return; }
            let Some(policy) = self.state.policies.get(&root.root) else { return; };
            let Ok(path) = root.native_path() else { return; };
            let mut y = title.bottom() + ui.px(152);
            if root.state == "offline" {
                if let Some(fingerprint) = &root.observed_fingerprint {
                    let confirmed = self.volume_confirmation.as_ref() == Some(&(root.root, fingerprint.clone()));
                    let label = if confirmed { "Confirm: forget cached contents and index this volume" }
                        else { "Index this volume here (forgets previous cached contents)" };
                    if ui.button(Rect::new(rect.x, y, rect.w, h), label) {
                        if confirmed {
                            command = Some(FilesCommand::AcceptVolume { root: root.root, observed_fingerprint: fingerprint.clone() });
                            self.volume_confirmation = None;
                        } else { self.volume_confirmation = Some((root.root, fingerprint.clone())); }
                    }
                } else {
                    ui.label(Rect::new(rect.x, y, rect.w, h), "Volume absent: cached contents retained", ui.theme.text_dim, Align::Left);
                }
                y += ui.px(40);
                let observed_location = root.mounted_elsewhere.as_ref().and_then(|location| location.native_path().ok());
                let label = if observed_location.is_some() { "Index at observed location (new registration and full scan)" }
                    else { "Index at new location (absolute path entered above)" };
                if let Some(location) = &root.mounted_elsewhere {
                    ui.label(Rect::new(rect.x, y, rect.w, h), &format!("This volume is mounted at {}", location.display), ui.theme.text_dim, Align::Left);
                    y += ui.px(40);
                }
                if ui.button(Rect::new(rect.x, y, rect.w, h), label) {
                    let expected_volume = observed_location.as_ref().map(|_| root.volume.clone());
                    command = Some(FilesCommand::Relocate { old_path: path.clone(),
                        new_path: observed_location.unwrap_or_else(|| PathBuf::from(self.root_draft.text())), expected_hash: root.policy_hash.clone(), policy: policy.clone(), expected_volume });
                }
                y += ui.px(40);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relocation_confirmation_requires_the_original_strong_volume_at_registration() {
        let original: IndexVolume = serde_json::from_value(serde_json::json!({
            "confidence":"strong", "fs_uuid":"original",
            "mount_root":{"bytes_b64":"Lw==", "display":"/"}, "mount_id":42
        }))
        .unwrap();
        let mut relocated = original.clone();
        relocated.mount_id = Some(43);
        assert!(relocation_volume_matches(&original, &relocated));
        relocated.fs_uuid = Some("replacement".into());
        assert!(!relocation_volume_matches(&original, &relocated));
        relocated = original.clone();
        relocated.confidence = templar_file_index::VolumeConfidence::Weak;
        assert!(!relocation_volume_matches(&original, &relocated));
        relocated = original.clone();
        relocated.mount_root.as_mut().unwrap().bytes_b64 = "L3N1YnZvbHVtZQ==".into();
        assert!(!relocation_volume_matches(&original, &relocated));
        relocated.mount_root.as_mut().unwrap().bytes_b64 = "malformed".into();
        assert!(!relocation_volume_matches(&original, &relocated));
        assert!(!relocation_volume_matches(
            &IndexVolume::default(),
            &IndexVolume::default()
        ));
    }
    #[test]
    fn offline_volume_render_has_freshness_coverage_identity_and_explicit_actions() {
        let roots: IndexRoots = serde_json::from_value(serde_json::json!({
            "cache_identity":"fixture", "roots":[{
                "root":2,"path_b64":"L21lZGlhL29sZA==","display":"/media/old", "state":"offline",
                "reason":"different-volume", "metadata":"unverified", "generation":9,"complete":true,
                "entry_count":123,"policy_hash":"policy", "last_error":null,
                "watch":{"backend":"fake", "watches_required":3, "watches_armed":2, "watches_refused":1,
                    "watch_limit":10,"queue_limit":10,"last_event_ms":null,"last_overflow_ms":null,
                    "overflow_count":0,"pending_events":0,"last_error":null},
                "volume":{"confidence":"strong","fs_type":"ext4","fs_uuid":"previous-volume"},
                "offline_reason":{"kind":"different-volume","observed":{"fs_uuid":"replacement","confidence":"strong"}},
                "observed_fingerprint":"replacement-fence", "nested_in":1,"contains":[3],
                "mounted_elsewhere":{"bytes_b64":"L21lZGlhL25ldy3/", "display":"/media/new-�"}
            }], "diagnostics":{"schema_version":8,"generation":9,"planner":"arrays", "fts_available":true,
                "change_seq_floor":1,"change_seq_head":9,"cursor_count":0,
                "availability":{"state":"unavailable","reason":"permission-denied","since_ms":1},
                "storage":{"main_bytes":10,"wal_bytes":20,"shm_bytes":30},
                "runtime":{"arrays":{"retained_bytes":4096,"hits":90,"misses":{"stale":10}}}}
        })).unwrap();
        let text = render_roots(&roots);
        for expected in [
            "different-volume",
            "Metadata: unverified",
            "required 3",
            "armed 2",
            "refused 1",
            "Nested in: 1",
            "contains: 3",
            "identity strong",
            "60 bytes",
            "WAL 20",
            "SHM 30",
            "90.00%",
            "4096 bytes",
            "Retry cache",
            "Index this volume here",
            "Index at new location",
            "This volume is mounted at /media/new-�",
            "Index at observed location (new registration and full scan)",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        use std::os::unix::ffi::OsStrExt as _;
        assert_eq!(
            roots.roots[0]
                .mounted_elsewhere
                .as_ref()
                .unwrap()
                .native_path()
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"/media/new-\xff"
        );
        let mut view = FilesView::default();
        view.replace(FilesState {
            roots: Some(roots),
            ..FilesState::default()
        });
        assert_eq!(view.summaries[&2].len(), 5);
        assert_eq!(view.selected, Some(2));
    }
}
