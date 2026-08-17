//! Project sessions instead of virtual desktops: a project is a visible object that stores
//! files, applications, window arrangement, terminal directories, environment, priority and
//! permissions. Closing checkpoints (where supported); reopening restores.

use lege_types::{AppId, InstanceId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
