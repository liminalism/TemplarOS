//! `org.legeos.Projects1`: project sessions instead of virtual desktops.
//!
//! Closing checkpoints each running, checkpoint-capable application (`docs/protocols/checkpoint.md`)
//! through [`crate::checkpoint::CheckpointTransport`]; opening relaunches every remembered
//! application and, for one with a stored token, best-effort asks it to restore. `appd`'s
//! launcher is called directly (in-process — this daemon *is* `appd`'s host), not through the
//! `Launcher1` D-Bus round-trip.

use crate::{apis::failed, checkpoint::CheckpointTransport};
use lege_appd::{Registry, launcher::LaunchBackend};
use lege_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    projects::{self, Project, ProjectApp, ProjectId, ProjectState},
};
use lege_state::Store;
use lege_types::{
    AppCatalog, AppId, InstanceId,
    checkpoint::{CheckpointRequest, CheckpointResponse},
};
use parking_lot::RwLock;
use rusqlite::{OptionalExtension, params};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};
use zbus::{Connection, fdo, object_server::SignalEmitter};

/// Where the current project is remembered. `settings_values` is the generic key/value table other
/// domains piggyback on, under an `_internal.` prefix that keeps it out of the typed settings
/// schema (the same convention `_internal.location.policy` and `_internal.power.profile.*` use).
const CURRENT_KEY: &str = "_internal.projects.current";

#[derive(Debug)]
pub struct ProjectsApi {
    store: Store,
    registry: Arc<Registry>,
    backend: LaunchBackend,
    connection: Connection,
    checkpoint: Arc<dyn CheckpointTransport>,
    open_projects: Arc<RwLock<BTreeSet<String>>>,
    history: Arc<SqliteHistory>,
}

impl ProjectsApi {
    pub fn new(
        store: Store,
        registry: Arc<Registry>,
        backend: LaunchBackend,
        connection: Connection,
        checkpoint: Arc<dyn CheckpointTransport>,
        open_projects: Arc<RwLock<BTreeSet<String>>>,
        history: Arc<SqliteHistory>,
    ) -> Self {
        Self {
            store,
            registry,
            backend,
            connection,
            checkpoint,
            open_projects,
            history,
        }
    }

    fn load(&self, project_id: &str) -> fdo::Result<Project> {
        projects::load_all(&self.store)
            .map_err(failed)?
            .into_iter()
            .find(|project| project.id.0 == project_id)
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no project {project_id:?}")))
    }

    fn save(&self, project: &Project) -> fdo::Result<()> {
        self.store
            .transaction(|tx| projects::save_within(tx, project))
            .map_err(failed)
    }

    /// Record one event caused by a project. The cause is `project:<id>` rather than a bare
    /// `project`, so "what happened in this project" is a question History1 can actually answer.
    fn record(
        &self,
        project_id: &str,
        kind: EventKind,
        app_id: Option<AppId>,
        instance_id: Option<InstanceId>,
        message: String,
    ) {
        let event = Event {
            id: EventId::new(),
            at_ms: lege_state::now_ms(),
            kind,
            app_id,
            instance_id,
            message,
            cause: Some(format!("project:{project_id}")),
            actions: Vec::new(),
            reversible: false,
        };
        if let Err(error) = self.history.append(event) {
            tracing::warn!(%error, "failed to record project history event");
        }
    }

    /// The project the session is working in, or `None`.
    fn current(&self) -> Option<String> {
        let connection = self.store.lock();
        connection
            .query_row(
                "SELECT value_json FROM settings_values WHERE key = ?1",
                params![CURRENT_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<String>(&json).ok())
            .filter(|id| !id.is_empty())
    }

    fn store_current(&self, project_id: Option<&str>) -> fdo::Result<()> {
        self.store
            .transaction(|tx| {
                match project_id {
                    None => {
                        tx.execute(
                            "DELETE FROM settings_values WHERE key = ?1",
                            params![CURRENT_KEY],
                        )?;
                    }
                    Some(id) => {
                        tx.execute(
                            "INSERT INTO settings_values (key, value_json, updated_ms)
                             VALUES (?1, ?2, ?3)
                             ON CONFLICT (key) DO UPDATE SET
                                value_json = excluded.value_json, updated_ms = excluded.updated_ms",
                            params![
                                CURRENT_KEY,
                                serde_json::to_string(id)?,
                                lege_state::now_ms()
                            ],
                        )?;
                    }
                }
                Ok(())
            })
            .map_err(failed)
    }

    /// Tell the file service which project is open, so recent-file and last-directory suggestions
    /// follow the work. Best-effort: `lege-filed` may not be up yet, and a project must still open
    /// when it is not.
    async fn tell_files_the_project(&self, project: Option<&Project>, closing: Option<&str>) {
        let proxy = match lege_client::proxies::FilesRecentsProxy::new(&self.connection).await {
            Ok(proxy) => proxy,
            Err(error) => {
                tracing::debug!(%error, "the file service is not answering; recent-file suggestions will not follow this project");
                return;
            }
        };
        let result = match (project, closing) {
            (Some(project), _) => {
                proxy
                    .set_project(&project.id.0, &project.root.to_string_lossy())
                    .await
            }
            (None, Some(project_id)) => proxy.clear_project(project_id).await,
            (None, None) => Ok(()),
        };
        if let Err(error) = result {
            tracing::debug!(%error, "telling the file service about the project failed");
        }
    }
}

/// Which installed copy a remembered application should open as.
///
/// The copy the project names comes first — a person who chose one meant that one. Then the copy
/// that actually ran last time, which is the sibling a previous open already substituted. Then the
/// logical application's current default. `None` means nothing is installed to open it with, and
/// the caller says so and moves on rather than failing the whole project: one missing application
/// must not cost a person the other six.
fn resolve_project_app(
    registry: &Registry,
    app: &ProjectApp,
) -> Option<(AppId, std::sync::Arc<lege_types::AppManifest>)> {
    let mut candidates = vec![app.app_id.clone()];
    if let Some(last) = &app.last_variant {
        candidates.push(last.clone());
    }
    if let Some(logical) = &app.logical_app
        && let Some(default) = registry.logical_index().default_variant(logical)
    {
        candidates.push(default.clone());
    }
    candidates.into_iter().find_map(|candidate| {
        let resolved = registry.resolve_variant(&candidate);
        registry
            .manifest(&resolved)
            .map(|manifest| (resolved, manifest))
    })
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_app_id(app_id: &str) -> fdo::Result<AppId> {
    app_id
        .parse()
        .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))
}

#[zbus::interface(name = "org.legeos.Projects1")]
impl ProjectsApi {
    async fn create(&self, project_id: &str, name: &str, root: &str) -> fdo::Result<()> {
        if project_id.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "project_id must not be empty".into(),
            ));
        }
        self.save(&Project::new(project_id, name, PathBuf::from(root)))
    }

    /// `(project_id, name, root, is_open)`
    async fn list(&self) -> fdo::Result<Vec<(String, String, String, bool)>> {
        let projects = projects::load_all(&self.store).map_err(failed)?;
        let open = self.open_projects.read();
        Ok(projects
            .into_iter()
            .map(|project| {
                let is_open = open.contains(&project.id.0);
                (
                    project.id.0,
                    project.name,
                    project.root.to_string_lossy().into_owned(),
                    is_open,
                )
            })
            .collect())
    }

    async fn open(&self, project_id: &str) -> fdo::Result<()> {
        let mut project = self.load(project_id)?;
        for app in &mut project.apps {
            let Some((chosen, manifest)) = resolve_project_app(&self.registry, app) else {
                tracing::warn!(
                    app = %app.app_id,
                    logical = ?app.logical_app.as_ref().map(ToString::to_string),
                    "project remembers an application that is no longer installed; skipped"
                );
                self.record(
                    project_id,
                    EventKind::Launch,
                    Some(app.app_id.clone()),
                    None,
                    format!(
                        "{} is remembered by project {} but is not installed, so it was not opened.",
                        app.logical_app
                            .as_ref()
                            .map_or_else(|| app.app_id.to_string(), ToString::to_string),
                        project.name
                    ),
                );
                continue;
            };
            let previous_instance = app.last_instance;
            match lege_appd::launcher::launch(
                Arc::clone(&self.registry),
                manifest,
                &app.arguments,
                None,
                self.backend,
                Some(&self.connection),
            )
            .await
            {
                Ok(record) => {
                    if let Some(token) = app.checkpoint_token.clone() {
                        let _ = token; // wire format has no token field yet; see crate::checkpoint.
                        let response = self
                            .checkpoint
                            .send(
                                record.id,
                                CheckpointRequest::RestoreInstance {
                                    instance: record.id,
                                    previous: previous_instance.unwrap_or(record.id),
                                },
                            )
                            .await;
                        if let CheckpointResponse::Refused { reason, .. } = response {
                            tracing::debug!(app = %app.app_id, %reason, "restore not honoured (legacy application)");
                        }
                    }
                    self.record(
                        project_id,
                        EventKind::Launch,
                        Some(chosen.clone()),
                        Some(record.id),
                        format!(
                            "{chosen} was restored while opening project {}.",
                            project.name
                        ),
                    );
                    app.last_instance = Some(record.id);
                    // What actually ran, so next time the project reopens the copy the person was
                    // using rather than whatever is default that day.
                    app.last_variant = Some(chosen);
                }
                Err(error) => {
                    tracing::warn!(app = %chosen, %error, "failed to relaunch project application");
                }
            }
        }
        project.last_opened_ms = lege_state::now_ms();
        self.save(&project)?;
        self.open_projects.write().insert(project_id.to_owned());
        // Recent-file and last-directory suggestions should follow the work.
        self.tell_files_the_project(Some(&project), None).await;
        Ok(())
    }

    async fn close(&self, project_id: &str) -> fdo::Result<()> {
        let mut project = self.load(project_id)?;
        for app in &mut project.apps {
            let Some(instance) = app.last_instance else {
                continue;
            };
            let response = self
                .checkpoint
                .send(
                    instance,
                    CheckpointRequest::CheckpointNow {
                        instance,
                        deadline: Duration::from_secs(5),
                    },
                )
                .await;
            match response {
                CheckpointResponse::CheckpointComplete {
                    token,
                    has_unsaved_work,
                    ..
                } => {
                    app.checkpoint_token = Some(token);
                    self.record(
                        project_id,
                        EventKind::ResourceAction,
                        Some(app.app_id.clone()),
                        Some(instance),
                        format!(
                            "{} was checkpointed while closing project {} ({}).",
                            app.app_id,
                            project.name,
                            if has_unsaved_work {
                                "unsaved work preserved"
                            } else {
                                "no unsaved work"
                            }
                        ),
                    );
                }
                CheckpointResponse::Refused { reason, .. } => {
                    self.record(
                        project_id,
                        EventKind::ResourceAction,
                        Some(app.app_id.clone()),
                        Some(instance),
                        format!(
                            "{} could not be checkpointed while closing project {}: {reason}.",
                            app.app_id, project.name
                        ),
                    );
                }
                CheckpointResponse::Prepared { .. } => {}
            }
        }
        self.save(&project)?;
        self.open_projects.write().remove(project_id);
        self.tell_files_the_project(None, Some(project_id)).await;
        Ok(())
    }

    async fn add_app(
        &self,
        project_id: &str,
        app_id: &str,
        arguments: Vec<String>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        let mut project = self.load(project_id)?;
        project.apps.retain(|app| app.app_id != app_id);
        project.apps.push(ProjectApp::variant(app_id, arguments));
        self.save(&project)
    }

    async fn remove_app(&self, project_id: &str, app_id: &str) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        let mut project = self.load(project_id)?;
        project.apps.retain(|app| app.app_id != app_id);
        self.save(&project)
    }

    async fn remove(&self, project_id: &str) -> fdo::Result<()> {
        self.store
            .transaction(|tx| projects::delete_within(tx, &ProjectId(project_id.to_owned())))
            .map_err(failed)?;
        self.open_projects.write().remove(project_id);
        if self.current().as_deref() == Some(project_id) {
            self.store_current(None)?;
        }
        Ok(())
    }

    /// Remember a *logical* application rather than one copy of it, so a project opened after the
    /// Flatpak replaced the distribution package still opens the same application.
    ///
    /// `preferred_variant` may be empty to mean "whichever copy is default when the project
    /// opens". A named variant that is not a copy of that application is refused: it would be a
    /// different program than the one asked for.
    async fn add_logical_app(
        &self,
        project_id: &str,
        logical_app_id: &str,
        preferred_variant: &str,
        arguments: Vec<String>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let logical: lege_types::LogicalAppId = logical_app_id
            .parse()
            .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        let index = self.registry.logical_index();
        let variants = index.variants_of(&logical);
        if variants.is_empty() {
            return Err(fdo::Error::FileNotFound(format!(
                "no application {logical} is installed"
            )));
        }
        let preferred = match nonempty(preferred_variant) {
            None => variants[0].clone(),
            Some(named) => {
                let named = self.registry.resolve_variant(&parse_app_id(&named)?);
                if !variants.contains(&named) {
                    return Err(fdo::Error::InvalidArgs(format!(
                        "{named} is not a copy of {logical}."
                    )));
                }
                named
            }
        };

        let mut project = self.load(project_id)?;
        project
            .apps
            .retain(|app| app.logical_app.as_ref() != Some(&logical) && app.app_id != preferred);
        project.apps.push(ProjectApp {
            app_id: preferred,
            logical_app: Some(logical),
            last_variant: None,
            arguments,
            last_instance: None,
            checkpoint_token: None,
        });
        project.updated_ms = lege_state::now_ms();
        self.save(&project)?;
        let _ = emitter.changed(project_id.to_owned()).await;
        Ok(())
    }

    /// The whole project as JSON (`interfaces/schemas/project.schema.json`).
    ///
    /// `List` answers "which projects exist"; this answers "what is this one". The payload is JSON
    /// rather than a struct signature because project semantics keep growing and a signature
    /// cannot — the same reason `Providers1.Report` and `HomeField1.Get` do it.
    async fn describe(&self, project_id: &str) -> fdo::Result<String> {
        let project = self.load(project_id)?;
        serde_json::to_string(&project)
            .map_err(|error| fdo::Error::Failed(format!("serializing project: {error}")))
    }

    /// `(project_id, project_json)` for every project — one round-trip instead of `List` plus N
    /// `Describe`s, which is what a launcher or an omnibox actually needs.
    async fn list_detailed(&self) -> fdo::Result<Vec<(String, String)>> {
        Ok(projects::load_all(&self.store)
            .map_err(failed)?
            .into_iter()
            .filter_map(|project| {
                let json = serde_json::to_string(&project).ok()?;
                Some((project.id.0, json))
            })
            .collect())
    }

    /// Merge a partial project object into the stored project.
    ///
    /// Only the user-editable keys are accepted. An unknown key is refused rather than dropped: a
    /// client that believes it changed something it did not is worse than an error.
    async fn update(
        &self,
        project_id: &str,
        changes_json: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let changes: serde_json::Value = serde_json::from_str(changes_json)
            .map_err(|error| fdo::Error::InvalidArgs(format!("changes must be JSON: {error}")))?;
        let changes = changes.as_object().ok_or_else(|| {
            fdo::Error::InvalidArgs("changes must be a JSON object of project keys".into())
        })?;

        let mut project = self.load(project_id)?;
        let previous_importance = project.importance;
        project
            .apply_changes(changes)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        self.save(&project)?;

        if project.importance != previous_importance {
            self.record(
                project_id,
                EventKind::SettingChange,
                None,
                None,
                format!(
                    "Project {} is now {}.",
                    project.name,
                    project.importance.as_str()
                ),
            );
        }
        let _ = emitter.changed(project_id.to_owned()).await;
        Ok(())
    }

    /// The project the session is currently working in, or `""` for none.
    async fn get_current(&self) -> String {
        self.current().unwrap_or_default()
    }

    /// Set (or clear, with `""`) the current project.
    async fn set_current(
        &self,
        project_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if project_id.is_empty() {
            let previous = self.current();
            self.store_current(None)?;
            if let Some(previous) = previous.as_deref() {
                self.tell_files_the_project(None, Some(previous)).await;
            }
            let _ = emitter.current_changed(String::new()).await;
            return Ok(());
        }
        let project = self.load(project_id)?;
        self.store_current(Some(project_id))?;
        self.tell_files_the_project(Some(&project), None).await;
        let _ = emitter.current_changed(project_id.to_owned()).await;
        Ok(())
    }

    /// Archive a project, or bring it back. An archived project stays searchable and openable; it
    /// is simply out of the way.
    async fn set_state(
        &self,
        project_id: &str,
        state: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let state = ProjectState::parse(state).ok_or_else(|| {
            fdo::Error::InvalidArgs(format!(
                "{state:?} is not a project state. Use \"active\" or \"archived\"."
            ))
        })?;
        let mut project = self.load(project_id)?;
        if project.state == state {
            return Ok(());
        }
        project.state = state;
        project.updated_ms = lege_state::now_ms();
        self.save(&project)?;
        self.record(
            project_id,
            EventKind::SettingChange,
            None,
            None,
            match state {
                ProjectState::Archived => format!(
                    "Project {} was archived. It is out of the way, not gone: it still opens.",
                    project.name
                ),
                ProjectState::Active => format!("Project {} is active again.", project.name),
            },
        );
        let _ = emitter.changed(project_id.to_owned()).await;
        Ok(())
    }

    /// This project changed; re-read it with `Describe`.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, project_id: String) -> zbus::Result<()>;

    /// The current project changed (`""` when it was cleared).
    #[zbus(signal)]
    async fn current_changed(
        signal_emitter: &SignalEmitter<'_>,
        project_id: String,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{AppAliases, AppManifest, AppOriginKind, AppProvenance, ResourceClass};
    use std::collections::{BTreeMap, BTreeSet};

    fn manifest(id: &str, name: &str, wm_class: &str, origin: AppOriginKind) -> AppManifest {
        AppManifest {
            id: id.parse().unwrap(),
            display_name: name.into(),
            primary_desktop_file_id: format!("{id}.desktop"),
            executable: PathBuf::from(format!("/usr/bin/{id}")),
            aliases: AppAliases {
                startup_wm_classes: [wm_class.to_owned()].into(),
                ..AppAliases::default()
            },
            mime_types: BTreeSet::default(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
            icon: None,
            categories: BTreeSet::default(),
            keywords: BTreeSet::default(),
            no_display: false,
            provenance: Some(AppProvenance::from_origin(origin)),
        }
    }

    /// The two Firefoxes, grouped.
    fn registry_with(installed: &[AppManifest]) -> Arc<Registry> {
        let registry = Arc::new(Registry::default());
        for manifest in installed {
            registry.insert_manifest(manifest.clone());
        }
        registry.regroup(&BTreeMap::new());
        registry
    }

    #[test]
    fn a_project_reopens_the_copy_it_named_while_that_copy_exists() {
        let distro = manifest(
            "desktop-import.firefox.desktop",
            "Firefox",
            "firefox",
            AppOriginKind::DistroPackage,
        );
        let flatpak = manifest(
            "flatpak.org.mozilla.firefox",
            "Firefox",
            "firefox",
            AppOriginKind::Flatpak,
        );
        let registry = registry_with(&[distro.clone(), flatpak.clone()]);
        let logical = registry.logical_of(&distro.id).unwrap();

        let app = ProjectApp {
            app_id: flatpak.id.clone(),
            logical_app: Some(logical),
            last_variant: None,
            arguments: vec![],
            last_instance: None,
            checkpoint_token: None,
        };
        let (chosen, _) = resolve_project_app(&registry, &app).unwrap();
        assert_eq!(
            chosen, flatpak.id,
            "the copy the project named wins over the logical default"
        );
    }

    #[test]
    fn a_project_still_opens_when_the_copy_it_named_was_uninstalled() {
        // The acceptance case: the project was created against the distribution's Firefox, the
        // person replaced it with the Flatpak, and the project must still open Firefox.
        let distro = manifest(
            "desktop-import.firefox.desktop",
            "Firefox",
            "firefox",
            AppOriginKind::DistroPackage,
        );
        let flatpak = manifest(
            "flatpak.org.mozilla.firefox",
            "Firefox",
            "firefox",
            AppOriginKind::Flatpak,
        );
        let both = registry_with(&[distro.clone(), flatpak.clone()]);
        let logical = both.logical_of(&distro.id).unwrap();

        let app = ProjectApp {
            app_id: distro.id.clone(),
            logical_app: Some(logical.clone()),
            last_variant: None,
            arguments: vec![],
            last_instance: None,
            checkpoint_token: None,
        };

        // Only the Flatpak remains.
        let after = registry_with(std::slice::from_ref(&flatpak));
        assert_eq!(
            after.logical_of(&flatpak.id),
            Some(logical),
            "the logical id has to survive the uninstall, or nothing can be found by it"
        );
        let (chosen, _) = resolve_project_app(&after, &app).unwrap();
        assert_eq!(chosen, flatpak.id);
    }

    #[test]
    fn what_actually_ran_is_preferred_over_todays_default() {
        let distro = manifest(
            "desktop-import.firefox.desktop",
            "Firefox",
            "firefox",
            AppOriginKind::DistroPackage,
        );
        let flatpak = manifest(
            "flatpak.org.mozilla.firefox",
            "Firefox",
            "firefox",
            AppOriginKind::Flatpak,
        );
        let registry = registry_with(&[distro.clone(), flatpak.clone()]);
        let logical = registry.logical_of(&distro.id).unwrap();

        let app = ProjectApp {
            // A copy that is not installed any more.
            app_id: "desktop-import.firefox-esr.desktop".parse().unwrap(),
            logical_app: Some(logical),
            last_variant: Some(flatpak.id.clone()),
            arguments: vec![],
            last_instance: None,
            checkpoint_token: None,
        };
        let (chosen, _) = resolve_project_app(&registry, &app).unwrap();
        assert_eq!(
            chosen, flatpak.id,
            "the copy that ran last time beats the default the machine would pick today"
        );
    }

    #[test]
    fn nothing_installed_means_skip_and_say_so_not_fail_the_project() {
        let registry = registry_with(&[]);
        let app = ProjectApp::variant("org.example.Gone".parse().unwrap(), vec![]);
        assert!(resolve_project_app(&registry, &app).is_none());
    }

    #[test]
    fn a_renamed_variant_is_followed_through_its_alias() {
        let flatpak = manifest(
            "flatpak.org.mozilla.firefox",
            "Firefox",
            "firefox",
            AppOriginKind::Flatpak,
        );
        let registry = registry_with(std::slice::from_ref(&flatpak));
        registry.set_variant_aliases(
            [(
                "desktop-import.org.mozilla.firefox.desktop"
                    .parse()
                    .unwrap(),
                flatpak.id.clone(),
            )]
            .into_iter()
            .collect(),
        );
        let app = ProjectApp::variant(
            "desktop-import.org.mozilla.firefox.desktop"
                .parse()
                .unwrap(),
            vec![],
        );
        let (chosen, _) = resolve_project_app(&registry, &app).unwrap();
        assert_eq!(chosen, flatpak.id, "the same copy, under its new id");
    }
}
