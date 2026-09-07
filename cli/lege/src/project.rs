//! `lege project` — project sessions instead of virtual desktops (planning doc 01 §7,
//! `crates/lege-session-core/src/projects.rs`): a visible, named working set of files,
//! applications and window arrangement, not a hidden screen.
//!
//! Every D-Bus call goes through [`ProjectsClient`] so the rendering below is testable with a
//! fixture, never a live `org.legeos.Projects1`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

/// A project id derived from a folder, for `lege project create-here` — the route pcmanfm-qt's
/// "Create project here" takes, where nobody is around to type an id.
///
/// Lowercase, `[a-z0-9-]` only, runs collapsed, ends trimmed. `taken` is every id that already
/// exists; a collision takes the next free `-2`, `-3`, because `Projects1.Create` saves over an
/// existing id without asking and losing somebody's project to a same-named folder is not an
/// acceptable way to find that out.
pub fn project_id_for(root: &std::path::Path, taken: &[String]) -> String {
    let base: String = root
        .file_name()
        .unwrap_or(root.as_os_str())
        .to_string_lossy()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let stem =
        base.split('-')
            .filter(|part| !part.is_empty())
            .fold(String::new(), |mut out, part| {
                if !out.is_empty() {
                    out.push('-');
                }
                out.push_str(part);
                out
            });
    let stem = if stem.is_empty() {
        "project".to_owned()
    } else {
        stem
    };
    if !taken.contains(&stem) {
        return stem;
    }
    // At most `taken.len()` of these candidates can be in use, so the range always has a free one.
    (2..=taken.len() + 2)
        .map(|n| format!("{stem}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("more candidates than there are existing ids")
}

/// What `lege project` needs from `org.legeos.Projects1`.
#[async_trait]
pub trait ProjectsClient {
    async fn create(&self, project_id: &str, name: &str, root: &str) -> Result<()>;
    /// `(project_id, name, root, is_open)`
    async fn list(&self) -> Result<Vec<(String, String, String, bool)>>;
    async fn open(&self, project_id: &str) -> Result<()>;
    async fn close(&self, project_id: &str) -> Result<()>;
    async fn add_app(&self, project_id: &str, app_id: &str, arguments: Vec<String>) -> Result<()>;
    async fn remove_app(&self, project_id: &str, app_id: &str) -> Result<()>;
    async fn remove(&self, project_id: &str) -> Result<()>;
    /// The whole project as JSON (`interfaces/schemas/project.schema.json`).
    async fn describe(&self, project_id: &str) -> Result<String>;
    async fn update(&self, project_id: &str, changes_json: &str) -> Result<()>;
    async fn get_current(&self) -> Result<String>;
    async fn set_current(&self, project_id: &str) -> Result<()>;
    async fn set_state(&self, project_id: &str, state: &str) -> Result<()>;
}

#[async_trait]
impl ProjectsClient for lege_client::proxies::ProjectsProxy<'_> {
    async fn create(&self, project_id: &str, name: &str, root: &str) -> Result<()> {
        self.create(project_id, name, root)
            .await
            .context("Projects1.Create")
    }
    async fn list(&self) -> Result<Vec<(String, String, String, bool)>> {
        self.list().await.context("Projects1.List")
    }
    async fn open(&self, project_id: &str) -> Result<()> {
        self.open(project_id).await.context("Projects1.Open")
    }
    async fn close(&self, project_id: &str) -> Result<()> {
        self.close(project_id).await.context("Projects1.Close")
    }
    async fn add_app(&self, project_id: &str, app_id: &str, arguments: Vec<String>) -> Result<()> {
        self.add_app(project_id, app_id, arguments)
            .await
            .context("Projects1.AddApp")
    }
    async fn remove_app(&self, project_id: &str, app_id: &str) -> Result<()> {
        self.remove_app(project_id, app_id)
            .await
            .context("Projects1.RemoveApp")
    }
    async fn remove(&self, project_id: &str) -> Result<()> {
        self.remove(project_id).await.context("Projects1.Remove")
    }
    async fn describe(&self, project_id: &str) -> Result<String> {
        self.describe(project_id)
            .await
            .context("Projects1.Describe")
    }
    async fn update(&self, project_id: &str, changes_json: &str) -> Result<()> {
        self.update(project_id, changes_json)
            .await
            .context("Projects1.Update")
    }
    async fn get_current(&self) -> Result<String> {
        self.get_current().await.context("Projects1.GetCurrent")
    }
    async fn set_current(&self, project_id: &str) -> Result<()> {
        self.set_current(project_id)
            .await
            .context("Projects1.SetCurrent")
    }
    async fn set_state(&self, project_id: &str, state: &str) -> Result<()> {
        self.set_state(project_id, state)
            .await
            .context("Projects1.SetState")
    }
}

/// Just enough of `project.schema.json` to show a person their project. Deliberately partial and
/// non-exhaustive: the payload grows, and a CLI that refused to print a project because it carried
/// a key this binary predates would be worse than useless.
#[derive(Debug, Default, Deserialize)]
pub struct ProjectView {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub terminal_directories: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub apps: Vec<ProjectAppView>,
    #[serde(default)]
    pub importance: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub last_opened_ms: i64,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProjectAppView {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub logical_app: Option<String>,
    #[serde(default)]
    pub last_variant: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
}

pub fn parse_project(project_json: &str) -> Result<ProjectView> {
    serde_json::from_str(project_json).context("parsing Projects1.Describe payload")
}

/// One project as aligned rows. Never the raw JSON: a person asked what their project is, not what
/// the wire format looks like.
pub fn render_show(project: &ProjectView, is_current: bool) -> String {
    let mut rows: Vec<Vec<String>> = vec![
        vec!["id".into(), project.id.clone()],
        vec!["name".into(), project.name.clone()],
        vec!["root".into(), project.root.clone()],
        vec![
            "state".into(),
            if project.state.is_empty() {
                "active".to_owned()
            } else {
                project.state.clone()
            },
        ],
        vec![
            "importance".into(),
            if project.importance.is_empty() {
                "normal".to_owned()
            } else {
                project.importance.clone()
            },
        ],
        vec![
            "current".into(),
            if is_current { "yes" } else { "no" }.to_owned(),
        ],
        vec![
            "last opened".into(),
            if project.last_opened_ms > 0 {
                crate::humanize::timestamp_ms(project.last_opened_ms)
            } else {
                "never".to_owned()
            },
        ],
    ];
    push_list(&mut rows, "directories", &project.directories);
    push_list(&mut rows, "terminals", &project.terminal_directories);
    push_list(&mut rows, "mounts", &project.mounts);
    push_list(&mut rows, "references", &project.references);
    let environment: Vec<String> = project
        .environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    push_list(&mut rows, "environment", &environment);
    let apps: Vec<String> = project
        .apps
        .iter()
        .map(|app| match (&app.logical_app, &app.last_variant) {
            (Some(logical), Some(variant)) => format!("{logical} (last ran {variant})"),
            (Some(logical), None) => logical.clone(),
            (None, _) => app.app_id.clone(),
        })
        .collect();
    push_list(&mut rows, "applications", &apps);
    crate::format::table(&rows)
}

/// One row per value, with the label on the first and blank on the rest, so a project with four
/// directories reads as a list rather than as one crowded line.
fn push_list(rows: &mut Vec<Vec<String>>, label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    for (index, value) in values.iter().enumerate() {
        rows.push(vec![
            if index == 0 {
                label.to_owned()
            } else {
                String::new()
            },
            value.clone(),
        ]);
    }
}

/// The keys `lege project set` accepts, and how a value is typed for each. Kept beside the
/// renderer rather than deferred to the daemon so the refusal arrives before the round-trip and
/// can list the alternatives.
const SETTABLE: &[(&str, ValueShape)] = &[
    ("name", ValueShape::Text),
    ("root", ValueShape::Text),
    ("importance", ValueShape::Text),
    ("state", ValueShape::Text),
    ("directories", ValueShape::List),
    ("terminal_directories", ValueShape::List),
    ("mounts", ValueShape::List),
    ("references", ValueShape::List),
];

#[derive(Clone, Copy, Debug)]
enum ValueShape {
    Text,
    /// Comma-separated on the command line; a JSON array on the wire.
    List,
}

/// Build the `changes_json` for one `key=value` edit.
pub fn changes_json(key: &str, value: &str) -> Result<String> {
    let Some((_, shape)) = SETTABLE.iter().find(|(name, _)| *name == key) else {
        let names: Vec<&str> = SETTABLE.iter().map(|(name, _)| *name).collect();
        bail!(
            "{key:?} is not something you can set on a project. You can set: {}.",
            names.join(", ")
        );
    };
    let value = match shape {
        ValueShape::Text => serde_json::Value::String(value.to_owned()),
        ValueShape::List => serde_json::Value::Array(
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| serde_json::Value::String(part.to_owned()))
                .collect(),
        ),
    };
    serde_json::to_string(&serde_json::json!({ key: value }))
        .context("building the project change payload")
}

pub fn render_list(projects: &[(String, String, String, bool)]) -> String {
    if projects.is_empty() {
        return "no projects".to_owned();
    }
    let rows: Vec<Vec<String>> = projects
        .iter()
        .map(|(id, name, root, is_open)| {
            vec![
                id.clone(),
                name.clone(),
                root.clone(),
                if *is_open {
                    "open".to_owned()
                } else {
                    "closed".to_owned()
                },
            ]
        })
        .collect();
    crate::format::table(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_renders_open_state() {
        let projects = vec![
            (
                "thesis".to_owned(),
                "Thesis".to_owned(),
                "/home/u/thesis".to_owned(),
                true,
            ),
            (
                "taxes".to_owned(),
                "Taxes 2026".to_owned(),
                "/home/u/taxes".to_owned(),
                false,
            ),
        ];
        let rendered = render_list(&projects);
        assert!(rendered.contains("thesis"));
        assert!(rendered.contains("open"));
        assert!(rendered.contains("closed"));
    }

    #[test]
    fn no_projects_says_so_plainly() {
        assert_eq!(render_list(&[]), "no projects");
    }

    #[test]
    fn show_reads_as_a_project_not_as_json() {
        let payload = r#"{
            "id": "thesis", "name": "Thesis", "root": "/home/p/thesis",
            "directories": ["/home/p/refs", "/home/p/figures"],
            "references": ["https://example.org/issue/1"],
            "importance": "important", "state": "active", "last_opened_ms": 0,
            "apps": [
                {"app_id": "desktop-import.firefox.desktop", "logical_app": "logical.firefox",
                 "last_variant": "flatpak.org.mozilla.firefox", "arguments": []},
                {"app_id": "org.legeos.Viewer", "arguments": []}
            ]
        }"#;
        let view = parse_project(payload).unwrap();
        let rendered = render_show(&view, true);
        assert!(rendered.contains("Thesis"));
        assert!(rendered.contains("important"));
        assert!(rendered.contains("never"), "an unopened project says so");
        assert!(rendered.contains("current"));
        assert!(
            rendered.contains("logical.firefox (last ran flatpak.org.mozilla.firefox)"),
            "a person should see which copy actually ran: {rendered}"
        );
        assert!(rendered.contains("org.legeos.Viewer"));
        assert!(!rendered.contains('{'), "never raw JSON: {rendered}");
        // Two directories, two rows, the label on the first only — and the columns line up.
        let lines: Vec<&str> = rendered.lines().collect();
        let first = lines
            .iter()
            .position(|line| line.starts_with("directories"))
            .expect("a labelled first directory row");
        assert!(lines[first].ends_with("/home/p/refs"));
        assert!(lines[first + 1].trim_start().starts_with("/home/p/figures"));
        assert!(
            lines[first + 1].starts_with("   "),
            "the second value stays in the value column: {:?}",
            lines[first + 1]
        );
    }

    #[test]
    fn a_project_carrying_keys_this_binary_predates_still_prints() {
        let view = parse_project(r#"{"id":"x","name":"X","root":"/x","colour":"blue"}"#).unwrap();
        let rendered = render_show(&view, false);
        assert!(rendered.contains("active"), "absent state reads as active");
        assert!(rendered.contains("normal"));
    }

    #[test]
    fn set_types_its_values_and_names_the_alternatives_when_refused() {
        assert_eq!(
            changes_json("name", "Doctoral thesis").unwrap(),
            r#"{"name":"Doctoral thesis"}"#
        );
        assert_eq!(
            changes_json("directories", "/a, /b ,").unwrap(),
            r#"{"directories":["/a","/b"]}"#
        );
        let error = changes_json("colour", "blue").unwrap_err().to_string();
        assert!(error.contains("colour"));
        assert!(
            error.contains("You can set:") && error.contains("importance"),
            "the refusal has to say what is possible instead: {error}"
        );
    }
}

#[cfg(test)]
mod create_here_tests {
    use super::project_id_for;
    use std::path::Path;

    #[test]
    fn a_folder_name_becomes_a_slug_and_a_collision_takes_the_next_free_suffix() {
        assert_eq!(
            project_id_for(Path::new("/home/dk/src/LegeOS"), &[]),
            "legeos"
        );
        assert_eq!(
            project_id_for(Path::new("/home/dk/My Notes (2026)!"), &[]),
            "my-notes-2026"
        );
        // Nothing usable in the name at all, and the filesystem root, still produce an id.
        assert_eq!(project_id_for(Path::new("/tmp/___"), &[]), "project");
        assert_eq!(project_id_for(Path::new("/"), &[]), "project");
        // A taken id never overwrites: Projects1.Create saves over one without asking.
        let taken = vec!["legeos".to_owned(), "legeos-2".to_owned()];
        assert_eq!(
            project_id_for(Path::new("/home/dk/src/legeos"), &taken),
            "legeos-3"
        );
    }
}
