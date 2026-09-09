//! Last-used directory policy shared by the file manager, the Home Surface and the file-chooser
//! portal — so no two clients remember different "last folders".

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use templar_types::AppId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationType {
    Open,
    Save,
    Import,
    Export,
}

impl OperationType {
    pub const ALL: [OperationType; 4] = [Self::Open, Self::Save, Self::Import, Self::Export];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Save => "save",
            Self::Import => "import",
            Self::Export => "export",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
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

/// The persistent form, in the one per-user state store.
///
/// This is the whole point of the module: the file manager, the Home Surface and the file
/// chooser read and write the same rows, so they cannot drift apart. Two forks that each
/// remembered their own last folder is the problem this replaces.
#[derive(Clone, Debug)]
pub struct LastDirectoryStore {
    store: templar_state::Store,
    /// Used when nothing else is known — the user's home directory.
    pub fallback: PathBuf,
}

/// Scope column values. Kept as constants rather than an enum in the schema so a new scope does
/// not need a migration.
const SCOPE_APP: &str = "app";
const SCOPE_OPERATION: &str = "operation";
const SCOPE_PROJECT: &str = "project";

impl LastDirectoryStore {
    pub fn new(store: templar_state::Store, fallback: PathBuf) -> Self {
        Self { store, fallback }
    }

    /// Record where a chooser or file manager ended up.
    pub fn record(
        &self,
        app_id: Option<&AppId>,
        operation: OperationType,
        directory: &std::path::Path,
    ) -> anyhow::Result<()> {
        let now = templar_state::now_ms();
        self.store.transaction(|transaction| {
            let write = |scope: &str, key: String| -> anyhow::Result<()> {
                transaction.execute(
                    "INSERT INTO last_directories (scope, key, directory, at_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (scope, key) DO UPDATE SET
                        directory = excluded.directory, at_ms = excluded.at_ms",
                    rusqlite::params![scope, key, directory.to_string_lossy(), now],
                )?;
                Ok(())
            };
            if let Some(app_id) = app_id {
                write(SCOPE_APP, app_id.to_string())?;
            }
            write(SCOPE_OPERATION, operation.as_str().to_owned())
        })
    }

    /// Note which project is open, so its root can be suggested.
    pub fn set_project(&self, project_id: &str, root: &std::path::Path) -> anyhow::Result<()> {
        self.store.lock().execute(
            "INSERT INTO last_directories (scope, key, directory, at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (scope, key) DO UPDATE SET
                directory = excluded.directory, at_ms = excluded.at_ms",
            rusqlite::params![
                SCOPE_PROJECT,
                project_id,
                root.to_string_lossy(),
                templar_state::now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn clear_project(&self, project_id: &str) -> anyhow::Result<()> {
        self.store.lock().execute(
            "DELETE FROM last_directories WHERE scope = ?1 AND key = ?2",
            rusqlite::params![SCOPE_PROJECT, project_id],
        )?;
        Ok(())
    }

    /// Load the in-memory view the pure policy works on.
    pub fn load(&self) -> anyhow::Result<LastDirectories> {
        let store = self.store.lock();
        let mut statement = store
            .prepare("SELECT scope, key, directory FROM last_directories ORDER BY at_ms DESC")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                PathBuf::from(row.get::<_, String>(2)?),
            ))
        })?;
        let mut directories = LastDirectories {
            fallback: self.fallback.clone(),
            ..LastDirectories::default()
        };
        for (scope, key, directory) in rows.filter_map(Result::ok) {
            match scope.as_str() {
                SCOPE_APP => {
                    if let Ok(app_id) = key.parse::<AppId>() {
                        directories.per_app.insert(app_id, directory);
                    }
                }
                SCOPE_OPERATION => {
                    if let Some(operation) = OperationType::parse(&key) {
                        directories.per_operation.insert(operation, directory);
                    }
                }
                // Ordered newest first, so the most recently opened project wins.
                SCOPE_PROJECT if directories.project_root.is_none() => {
                    directories.project_root = Some((key, directory));
                }
                _ => {}
            }
        }
        Ok(directories)
    }

    /// The answer a file chooser actually asks for, with the reason attached.
    ///
    /// A suggestion pointing at a directory that has since been deleted is worse than no
    /// suggestion, so each candidate is checked before it is offered.
    pub fn suggest(
        &self,
        app_id: Option<&AppId>,
        operation: OperationType,
    ) -> anyhow::Result<Suggestion> {
        let mut directories = self.load()?;
        directories
            .per_app
            .retain(|_, directory| directory.is_dir());
        directories
            .per_operation
            .retain(|_, directory| directory.is_dir());
        if directories
            .project_root
            .as_ref()
            .is_some_and(|(_, root)| !root.is_dir())
        {
            directories.project_root = None;
        }
        Ok(directories.suggest(app_id, operation))
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
        let viewer: AppId = "org.templaros.Viewer".parse().unwrap();
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

    #[test]
    fn an_operation_name_round_trips() {
        for operation in OperationType::ALL {
            assert_eq!(OperationType::parse(operation.as_str()), Some(operation));
        }
        assert_eq!(OperationType::parse("teleport"), None);
    }

    #[test]
    fn the_store_is_shared_by_every_client() {
        let dir = tempfile::tempdir().unwrap();
        let papers = dir.path().join("papers");
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&papers).unwrap();
        std::fs::create_dir_all(&downloads).unwrap();

        let store = LastDirectoryStore::new(
            templar_state::Store::in_memory().unwrap(),
            dir.path().to_path_buf(),
        );
        let viewer: AppId = "org.templaros.Viewer".parse().unwrap();

        // The file manager records where the user went...
        store
            .record(Some(&viewer), OperationType::Open, &papers)
            .unwrap();
        // ...and the portal, a different process entirely, gets the same answer.
        assert_eq!(
            store.suggest(Some(&viewer), OperationType::Open).unwrap(),
            Suggestion::LastForApp {
                app_id: viewer.clone(),
                directory: papers.clone(),
            }
        );

        // An application that has never been used falls back to the operation type.
        let editor: AppId = "org.templaros.Editor".parse().unwrap();
        store.record(None, OperationType::Save, &downloads).unwrap();
        assert_eq!(
            store.suggest(Some(&editor), OperationType::Save).unwrap(),
            Suggestion::LastForOperation {
                operation: OperationType::Save,
                directory: downloads,
            }
        );
    }

    #[test]
    fn a_project_root_beats_the_fallback_and_a_deleted_directory_is_never_suggested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("thesis");
        std::fs::create_dir_all(&root).unwrap();
        let store = LastDirectoryStore::new(
            templar_state::Store::in_memory().unwrap(),
            dir.path().to_path_buf(),
        );

        store.set_project("thesis", &root).unwrap();
        assert_eq!(
            store.suggest(None, OperationType::Open).unwrap(),
            Suggestion::CurrentProject {
                project_id: "thesis".into(),
                directory: root.clone(),
            }
        );

        // The directory goes away; the suggestion must not point at it.
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            store.suggest(None, OperationType::Open).unwrap(),
            Suggestion::Fallback {
                directory: dir.path().to_path_buf()
            }
        );

        std::fs::create_dir_all(&root).unwrap();
        store.clear_project("thesis").unwrap();
        assert!(matches!(
            store.suggest(None, OperationType::Open).unwrap(),
            Suggestion::Fallback { .. }
        ));
    }
}
