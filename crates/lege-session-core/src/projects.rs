//! Project sessions instead of virtual desktops: a project is a visible object that stores
//! files, applications, window arrangement, terminal directories, environment, priority and
//! permissions. Closing checkpoints (where supported); reopening restores.

use anyhow::{Context, Result};
use lege_state::Store;
use lege_types::{AppId, InstanceId};
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ProjectId(pub String);

/// One application instance remembered by a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectApp {
    pub app_id: AppId,
    pub arguments: Vec<String>,
    pub last_instance: Option<InstanceId>,
    /// Opaque checkpoint token returned by the application, if it supports the protocol.
    pub checkpoint_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub root: PathBuf,
    #[serde(default)]
    pub directories: Vec<PathBuf>,
    #[serde(default)]
    pub apps: Vec<ProjectApp>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub terminal_directories: Vec<PathBuf>,
    /// Extra weight for the governor while the project is open.
    #[serde(default)]
    pub priority_boost: i32,
    #[serde(default)]
    pub mounts: Vec<PathBuf>,
}

impl Project {
    pub fn new(id: impl Into<String>, name: impl Into<String>, root: PathBuf) -> Self {
        Self {
            id: ProjectId(id.into()),
            name: name.into(),
            root,
            directories: Vec::new(),
            apps: Vec::new(),
            environment: BTreeMap::new(),
            terminal_directories: Vec::new(),
            priority_boost: 0,
            mounts: Vec::new(),
        }
    }
}

/// Load every persisted project, ordered by ID.
pub fn load_all(store: &Store) -> Result<Vec<Project>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT json FROM projects ORDER BY project_id")
        .context("preparing projects query")?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("reading projects")?;
    let mut projects = Vec::new();
    for row in rows {
        let json = row?;
        projects.push(serde_json::from_str(&json).context("parsing stored project")?);
    }
    Ok(projects)
}

/// Insert or update one project (the whole record is one JSON blob; projects are edited as a
/// unit, never column-by-column).
pub fn save_within(tx: &Transaction<'_>, project: &Project) -> Result<()> {
    let json = serde_json::to_string(project).context("serializing project")?;
    tx.execute(
        "INSERT INTO projects (project_id, json) VALUES (?1, ?2)
         ON CONFLICT (project_id) DO UPDATE SET json = excluded.json",
        params![project.id.0, json],
    )
    .context("persisting project")?;
    Ok(())
}

/// Remove a project permanently (its applications are not touched; close it first).
pub fn delete_within(tx: &Transaction<'_>, id: &ProjectId) -> Result<()> {
    tx.execute("DELETE FROM projects WHERE project_id = ?1", params![id.0])
        .context("deleting project")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_round_trip_and_can_be_deleted() {
        let store = Store::in_memory().unwrap();
        let mut project = Project::new("thesis", "Thesis", PathBuf::from("/home/user/thesis"));
        project.apps.push(ProjectApp {
            app_id: "org.legeos.Viewer".parse().unwrap(),
            arguments: vec!["main.pdf".into()],
            last_instance: None,
            checkpoint_token: None,
        });

        store.transaction(|tx| save_within(tx, &project)).unwrap();
        let loaded = load_all(&store).unwrap();
        assert_eq!(loaded, vec![project.clone()]);

        store
            .transaction(|tx| delete_within(tx, &project.id))
            .unwrap();
        assert!(load_all(&store).unwrap().is_empty());
    }
}
