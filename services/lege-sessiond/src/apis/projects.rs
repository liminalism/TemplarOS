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
    projects::{self, Project, ProjectApp, ProjectId},
};
use lege_state::Store;
use lege_types::{
    AppCatalog, AppId, InstanceId,
    checkpoint::{CheckpointRequest, CheckpointResponse},
};
use parking_lot::RwLock;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};
use zbus::{Connection, fdo};

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

    fn record(
        &self,
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
            cause: Some("project".into()),
            actions: Vec::new(),
            reversible: false,
        };
        if let Err(error) = self.history.append(event) {
            tracing::warn!(%error, "failed to record project history event");
        }
    }
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
            let Some(manifest) = self.registry.manifest(&app.app_id) else {
                tracing::warn!(app = %app.app_id, "project references an app with no installed manifest; skipped");
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
                        EventKind::Launch,
                        Some(app.app_id.clone()),
                        Some(record.id),
                        format!(
                            "{} was restored while opening project {}.",
                            app.app_id, project.name
                        ),
                    );
                    app.last_instance = Some(record.id);
                }
                Err(error) => {
                    tracing::warn!(app = %app.app_id, %error, "failed to relaunch project application");
                }
            }
        }
        self.save(&project)?;
        self.open_projects.write().insert(project_id.to_owned());
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
        project.apps.push(ProjectApp {
            app_id,
            arguments,
            last_instance: None,
            checkpoint_token: None,
        });
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
        Ok(())
    }
}
