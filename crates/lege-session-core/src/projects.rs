//! Project sessions instead of virtual desktops: a project is a visible object that stores files,
//! applications, window arrangement, terminal directories, environment, importance and
//! permissions. Closing checkpoints (where supported); reopening restores.
//!
//! The whole record is one JSON blob (`interfaces/schemas/project.schema.json`), read additively in
//! both directions: an older binary ignores keys it does not know, and a newer one defaults keys an
//! older writer omitted. Nothing here uses `deny_unknown_fields`, and a test holds that line —
//! it is what lets project semantics keep growing without a schema migration or a version gate.

use anyhow::{Context, Result};
use lege_state::Store;
use lege_types::{AppId, InstanceId, LogicalAppId};
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ProjectId(pub String);

/// One application instance remembered by a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectApp {
    /// The variant to launch when no logical application is recorded, or the variant a logical
    /// application resolved to when it was added.
    pub app_id: AppId,

    /// The logical application, when the project remembers one. Preferred at open time, so a
    /// project survives the Flatpak replacing the package it was created against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_app: Option<LogicalAppId>,

    /// The variant that actually launched last time. A project should reopen what the person was
    /// using, not merely what is default today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_variant: Option<AppId>,

    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub last_instance: Option<InstanceId>,
    /// Opaque checkpoint token returned by the application, if it supports the protocol.
    #[serde(default)]
    pub checkpoint_token: Option<String>,
}

impl ProjectApp {
    /// A remembered application named by one variant.
    pub fn variant(app_id: AppId, arguments: Vec<String>) -> Self {
        Self {
            app_id,
            logical_app: None,
            last_variant: None,
            arguments,
            last_instance: None,
            checkpoint_token: None,
        }
    }
}

/// How much a project matters, relative to the others.
///
/// Two values, because a person can honestly answer "does this one matter more than the others"
/// and cannot honestly answer "is this a 7 or an 8". It replaces the never-implemented
/// `priority_boost`, an `i32` that no code ever read.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectImportance {
    #[default]
    Normal,
    Important,
}

impl ProjectImportance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Important => "important",
        }
    }
}

/// Whether a project is in the way or out of it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectState {
    #[default]
    Active,
    /// Out of the way, not gone: an archived project stays searchable and it still opens.
    Archived,
}

impl ProjectState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }

    /// Parse the wire form. Returns `None` rather than guessing: a client that sends nonsense
    /// should be told, not quietly given "active".
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
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
    #[serde(default)]
    pub mounts: Vec<PathBuf>,

    /// Things the project points at that are not its own files: a URL, a document elsewhere, an
    /// issue tracker. Opaque strings — a project is a place to work, not a link database.
    #[serde(default)]
    pub references: Vec<String>,

    /// Whether this project matters more than the others. An Important project that is open asks
    /// the resource governor to protect the applications it launched.
    #[serde(default)]
    pub importance: ProjectImportance,

    #[serde(default)]
    pub state: ProjectState,

    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub updated_ms: i64,
    /// Zero (or absent) means never opened.
    #[serde(default)]
    pub last_opened_ms: i64,
}

/// A key `Projects1.Update` will not accept. Named so the refusal can say which one and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownProjectKey(pub String);

impl std::fmt::Display for UnknownProjectKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} is not something you can change on a project. \
             Editable: {}.",
            self.0,
            EDITABLE_KEYS.join(", ")
        )
    }
}

/// The keys a client may change. `id`, `apps` and the timestamps belong to the daemon: `apps` is
/// maintained by AddApp/AddLogicalApp/Open, and a timestamp a client could set is not a record of
/// anything.
pub const EDITABLE_KEYS: &[&str] = &[
    "name",
    "root",
    "directories",
    "environment",
    "terminal_directories",
    "mounts",
    "references",
    "importance",
    "state",
];

impl Project {
    pub fn new(id: impl Into<String>, name: impl Into<String>, root: PathBuf) -> Self {
        let now = lege_state::now_ms();
        Self {
            id: ProjectId(id.into()),
            name: name.into(),
            root,
            directories: Vec::new(),
            apps: Vec::new(),
            environment: BTreeMap::new(),
            terminal_directories: Vec::new(),
            mounts: Vec::new(),
            references: Vec::new(),
            importance: ProjectImportance::Normal,
            state: ProjectState::Active,
            created_ms: now,
            updated_ms: now,
            last_opened_ms: 0,
        }
    }

    /// Merge a partial project object into this one.
    ///
    /// Every key is checked against [`EDITABLE_KEYS`] first, and an unknown one is **refused**
    /// rather than dropped: a client that thinks it changed something must not be told it
    /// succeeded. A key that is present but malformed is refused for the same reason.
    pub fn apply_changes(
        &mut self,
        changes: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), UnknownProjectKey> {
        for key in changes.keys() {
            if !EDITABLE_KEYS.contains(&key.as_str()) {
                return Err(UnknownProjectKey(key.clone()));
            }
        }

        // Round-trip through the whole record so each field is decoded by its own serde impl:
        // one place that knows what a `PathBuf` or a `ProjectImportance` looks like on the wire.
        let mut merged =
            serde_json::to_value(&*self).map_err(|_| UnknownProjectKey("name".to_owned()))?;
        let object = merged
            .as_object_mut()
            .ok_or_else(|| UnknownProjectKey("name".to_owned()))?;
        for (key, value) in changes {
            object.insert(key.clone(), value.clone());
        }
        let updated: Self =
            serde_json::from_value(merged).map_err(|error| UnknownProjectKey(error.to_string()))?;

        *self = Self {
            // Identity and the daemon-owned fields survive whatever the client sent.
            id: self.id.clone(),
            apps: self.apps.clone(),
            created_ms: self.created_ms,
            updated_ms: lege_state::now_ms(),
            last_opened_ms: self.last_opened_ms,
            ..updated
        };
        Ok(())
    }
}

/// What `lege-resourced` is told about one instance.
///
/// The governor's contract is four booleans on `Resources1.SetInstanceHints`; this is the three of
/// them a project has an opinion about (`has_unsaved_work` belongs to the checkpoint protocol, not
/// to a project).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceHints {
    pub protected: bool,
    pub disposable: bool,
    pub supports_checkpoint: bool,
}

/// The hints for one instance a project launched.
///
/// Pure, and deliberately small: importance reaches the governor as `protected`, which is a channel
/// that already exists and already means something. A graded weight would need a governor that has
/// something to multiply it by, and it does not have one yet — inventing the channel first is how
/// `priority_boost` came to sit in this file for a year doing nothing.
///
/// An Important project that is *open* protects what it launched. Closing it, or demoting it, hands
/// the instance back to whatever its own manifest says, which is why this takes the manifest rather
/// than only the importance: the answer on the way down has to be the manifest's, not `false`.
pub fn hints_for(
    manifest: &lege_types::AppManifest,
    importance: ProjectImportance,
    project_open: bool,
) -> ResourceHints {
    use lege_types::ResourceClass;
    let by_project = project_open && importance == ProjectImportance::Important;
    ResourceHints {
        protected: by_project || manifest.resource_class == ResourceClass::SessionCritical,
        // Protecting something and calling it disposable in the same breath is not a policy. While
        // an Important project holds it, it is not first to go.
        disposable: !by_project && manifest.resource_class == ResourceClass::Disposable,
        supports_checkpoint: manifest.supports_checkpoint,
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
        project.apps.push(ProjectApp::variant(
            "org.legeos.Viewer".parse().unwrap(),
            vec!["main.pdf".into()],
        ));

        store.transaction(|tx| save_within(tx, &project)).unwrap();
        let loaded = load_all(&store).unwrap();
        assert_eq!(loaded, vec![project.clone()]);

        store
            .transaction(|tx| delete_within(tx, &project.id))
            .unwrap();
        assert!(load_all(&store).unwrap().is_empty());
    }

    #[test]
    fn a_project_written_before_these_fields_existed_still_reads() {
        // The blob is read additively in both directions. This is the "older writer" direction:
        // every field added since must default rather than fail the parse.
        let json = r#"{
            "id": "thesis",
            "name": "Thesis",
            "root": "/home/person/thesis",
            "directories": ["/home/person/refs"],
            "apps": [{"app_id": "org.legeos.Viewer", "arguments": [], "last_instance": null,
                      "checkpoint_token": null}],
            "priority_boost": 7
        }"#;
        let project: Project = serde_json::from_str(json).unwrap();
        assert_eq!(project.name, "Thesis");
        assert_eq!(project.importance, ProjectImportance::Normal);
        assert_eq!(project.state, ProjectState::Active);
        assert_eq!(project.last_opened_ms, 0);
        assert!(project.references.is_empty());
        assert_eq!(project.apps[0].logical_app, None);
        // `priority_boost` is gone, and an old blob still carrying it is read, not rejected.
    }

    #[test]
    fn an_older_binary_can_still_read_a_project_this_one_wrote() {
        // The "newer writer" direction. `PreviousRelease` is the struct as it shipped: an older
        // binary must ignore what it does not know rather than refuse the row.
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct PreviousRelease {
            id: ProjectId,
            name: String,
            root: PathBuf,
            #[serde(default)]
            directories: Vec<PathBuf>,
            #[serde(default)]
            environment: BTreeMap<String, String>,
            #[serde(default)]
            priority_boost: i32,
        }

        let mut project = Project::new("thesis", "Thesis", PathBuf::from("/home/person/thesis"));
        project.importance = ProjectImportance::Important;
        project.state = ProjectState::Archived;
        project
            .references
            .push("https://example.org/issue/1".into());
        let json = serde_json::to_string(&project).unwrap();

        let old: PreviousRelease = serde_json::from_str(&json).unwrap();
        assert_eq!(old.name, "Thesis");
        assert_eq!(old.priority_boost, 0, "a field nobody writes reads as zero");
    }

    #[test]
    fn update_accepts_the_editable_keys_and_refuses_everything_else() {
        let mut project = Project::new("thesis", "Thesis", PathBuf::from("/home/person/thesis"));
        project.apps.push(ProjectApp::variant(
            "org.legeos.Viewer".parse().unwrap(),
            vec![],
        ));
        let created = project.created_ms;

        let changes = serde_json::json!({
            "name": "Doctoral thesis",
            "importance": "important",
            "references": ["https://example.org/issue/1"],
        });
        project
            .apply_changes(changes.as_object().unwrap())
            .expect("every key here is editable");
        assert_eq!(project.name, "Doctoral thesis");
        assert_eq!(project.importance, ProjectImportance::Important);
        assert_eq!(project.references.len(), 1);
        assert_eq!(
            project.apps.len(),
            1,
            "apps are the daemon's, not a client's"
        );
        assert_eq!(project.created_ms, created);

        for refused in ["id", "apps", "created_ms", "last_opened_ms", "colour"] {
            let changes = serde_json::json!({ refused: serde_json::Value::Null });
            let error = project
                .apply_changes(changes.as_object().unwrap())
                .expect_err("{refused} must be refused rather than silently dropped");
            assert!(
                error.to_string().contains(refused),
                "the refusal has to name the key: {error}"
            );
            assert!(
                error.to_string().contains("Editable:"),
                "and say what can be changed: {error}"
            );
        }
    }

    #[test]
    fn a_malformed_value_is_refused_rather_than_half_applied() {
        let mut project = Project::new("thesis", "Thesis", PathBuf::from("/home/person/t"));
        let changes = serde_json::json!({ "importance": "extremely" });
        assert!(project.apply_changes(changes.as_object().unwrap()).is_err());
        assert_eq!(
            project.importance,
            ProjectImportance::Normal,
            "nothing was changed"
        );
    }

    #[test]
    fn importance_reaches_the_governor_only_while_the_project_is_open() {
        use lege_types::{AppAliases, AppManifest, ResourceClass};
        let ordinary = AppManifest {
            id: "org.legeos.Viewer".parse().unwrap(),
            display_name: "Viewer".into(),
            primary_desktop_file_id: "org.legeos.Viewer.desktop".into(),
            executable: PathBuf::from("/usr/bin/lege-viewer"),
            aliases: AppAliases::default(),
            mime_types: std::collections::BTreeSet::default(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: true,
            icon: None,
            categories: std::collections::BTreeSet::default(),
            keywords: std::collections::BTreeSet::default(),
            no_display: false,
            provenance: None,
        };

        let closed = hints_for(&ordinary, ProjectImportance::Important, false);
        assert!(!closed.protected, "a closed project protects nothing");
        assert!(closed.supports_checkpoint);

        let open = hints_for(&ordinary, ProjectImportance::Important, true);
        assert!(open.protected);

        let normal = hints_for(&ordinary, ProjectImportance::Normal, true);
        assert!(!normal.protected, "an open Normal project changes nothing");

        // Demotion has to hand back the manifest's own answer, not `false`.
        let critical = AppManifest {
            resource_class: ResourceClass::SessionCritical,
            ..ordinary.clone()
        };
        assert!(hints_for(&critical, ProjectImportance::Normal, false).protected);

        // Protected and disposable at once is not a policy.
        let disposable = AppManifest {
            resource_class: ResourceClass::Disposable,
            ..ordinary
        };
        assert!(hints_for(&disposable, ProjectImportance::Normal, true).disposable);
        let held = hints_for(&disposable, ProjectImportance::Important, true);
        assert!(held.protected && !held.disposable);
    }
}
