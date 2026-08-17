//! Last-used directory policy shared by the file manager, the Home Surface and the file-chooser
//! portal — so no two clients remember different "last folders".

use lege_types::AppId;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationType {
    Open,
    Save,
    Import,
    Export,
}

/// Where the last-used directory came from (always explainable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "source")]
pub enum Suggestion {
    LastForApp {
        app_id: AppId,
        directory: PathBuf,
    },
    LastForOperation {
        operation: OperationType,
        directory: PathBuf,
    },
    CurrentProject {
        project_id: String,
        directory: PathBuf,
    },
    Fallback {
        directory: PathBuf,
    },
}

impl Suggestion {
    pub fn directory(&self) -> &PathBuf {
        match self {
            Self::LastForApp { directory, .. }
            | Self::LastForOperation { directory, .. }
            | Self::CurrentProject { directory, .. }
            | Self::Fallback { directory } => directory,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastDirectories {
    pub per_app: BTreeMap<AppId, PathBuf>,
    pub per_operation: BTreeMap<OperationType, PathBuf>,
    pub project_root: Option<(String, PathBuf)>,
    pub fallback: PathBuf,
}

impl LastDirectories {
    pub fn record(&mut self, app_id: Option<&AppId>, operation: OperationType, directory: PathBuf) {
        if let Some(app_id) = app_id {
            self.per_app.insert(app_id.clone(), directory.clone());
        }
        self.per_operation.insert(operation, directory);
    }

    /// Order: last directory for *this app* → last for this operation type → project root → fallback.
    /// A directory a sandboxed app is not allowed to see is filtered by the caller.
    pub fn suggest(&self, app_id: Option<&AppId>, operation: OperationType) -> Suggestion {
        if let Some(app_id) = app_id
            && let Some(directory) = self.per_app.get(app_id)
        {
            return Suggestion::LastForApp {
                app_id: app_id.clone(),
                directory: directory.clone(),
            };
        }
        if let Some(directory) = self.per_operation.get(&operation) {
            return Suggestion::LastForOperation {
                operation,
                directory: directory.clone(),
            };
        }
        if let Some((project_id, directory)) = &self.project_root {
            return Suggestion::CurrentProject {
                project_id: project_id.clone(),
                directory: directory.clone(),
            };
        }
        Suggestion::Fallback {
            directory: self.fallback.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_order() {
        let mut dirs = LastDirectories {
            fallback: "/home/u".into(),
            ..Default::default()
        };
        let viewer: AppId = "org.legeos.Viewer".parse().unwrap();
        assert_eq!(
            dirs.suggest(Some(&viewer), OperationType::Open),
            Suggestion::Fallback {
                directory: "/home/u".into()
            }
        );

        dirs.project_root = Some(("thesis".into(), "/home/u/thesis".into()));
        assert!(matches!(
            dirs.suggest(Some(&viewer), OperationType::Open),
            Suggestion::CurrentProject { .. }
        ));

        dirs.record(None, OperationType::Open, "/home/u/dl".into());
        assert!(matches!(
            dirs.suggest(Some(&viewer), OperationType::Open),
            Suggestion::LastForOperation { .. }
        ));

        dirs.record(Some(&viewer), OperationType::Save, "/home/u/papers".into());
        assert_eq!(
            dirs.suggest(Some(&viewer), OperationType::Open).directory(),
            &PathBuf::from("/home/u/papers")
        );
    }
}
