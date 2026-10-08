//! Homegarden's authority snapshots: `Contexts` supplies stable place identity and ledger
//! geometry; versioned `PresentationV1` supplies actionable sessions, recent files and apps.
//! The shell observes, requests tending at explicit moments, and renders the reported work.

use std::{collections::HashMap, path::PathBuf};
use templar_client::proxies::HomeGardenProxy;

/// One used-structure node beneath a place, as the daemon reports it.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct TreeNode {
    pub name: String,
    pub is_dir: bool,
    pub score: f32,
    pub children: Vec<TreeNode>,
}

/// One place: the projection of a ledger context the tile needs.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct GardenPlace {
    pub root: PathBuf,
    pub stage: String,
    /// Legacy ledger form retained in the Contexts wire payload. Project bodies use
    /// `presentation.template`; directory structure is not their default presentation.
    pub form: String,
    pub effective: f32,
    pub roles: Vec<String>,
    /// The place's material identity, deterministic from its root: the theme turns it into the
    /// grain the body is drawn with, so a place looks like itself whenever it regrows. Texture
    /// only — never a hue, which is reserved for attention.
    pub material_seed: u64,
    pub tree: Vec<TreeNode>,
    pub pinned: bool,
    /// Retained for the Contexts wire shape. Pinning does not freeze automatic relevance size;
    /// only an explicit manual resize freezes it in the field authority.
    pub kept: bool,
    pub hidden: bool,
    pub summary: String,
    pub why: String,
    /// Ledger arrivals, available to explanatory tooltips rather than the work state line.
    pub bloom_total: u32,
    /// As many of those arrivals as the body can mark. The count above is the whole of it.
    pub bloomed: Vec<PathBuf>,
    /// The application instances that came out of this place — launched here, or launched to
    /// open a file under it — as the daemon's own launch record saw them.
    ///
    /// Strings rather than parsed ids: the shell compares them against
    /// `ShellToplevel.instance_id`, and a value this build cannot parse should fail to match
    /// rather than fail to load the place.
    pub instances: Vec<String>,
    pub presentation: ProjectPresentation,
}

impl GardenPlace {
    /// All live and parked windows that belong to this place, most recently activated first.
    /// Membership comes from the authority's verified instance set, never proximity,
    /// recency or an unproven title.
    #[must_use]
    pub fn windows(
        &self,
        toplevels: &[templar_window::ShellToplevel],
    ) -> Vec<templar_window::WindowId> {
        if self.instances.is_empty() {
            return Vec::new();
        }
        let mut mine: Vec<&templar_window::ShellToplevel> = toplevels
            .iter()
            .filter(|top| {
                top.instance_id
                    .is_some_and(|id| self.instances.iter().any(|known| *known == id.to_string()))
            })
            .collect();
        mine.sort_by_key(|top| (top.mru_rank, top.window_id));
        mine.iter().map(|top| top.window_id).collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ProjectPresentation {
    #[serde(with = "templar_types::native_path")]
    pub root: PathBuf,
    pub name: String,
    pub template: String,
    pub secondary_templates: Vec<String>,
    pub template_override: Option<String>,
    pub activity: String,
    pub attention: String,
    pub state_line: String,
    pub instances: Vec<String>,
    pub sessions: Vec<ProjectSession>,
    pub recent_files: Vec<ProjectFile>,
    pub apps: Vec<ProjectApp>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ProjectSession {
    pub id: String,
    pub instance_id: Option<String>,
    pub kind: String,
    pub program: String,
    pub subject: String,
    pub state: String,
    pub updated_ms: u64,
    pub exit_status: Option<i32>,
    pub commands: Vec<String>,
    #[serde(with = "templar_types::native_path")]
    pub cwd: PathBuf,
    pub resume: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ProjectFile {
    #[serde(with = "templar_types::native_path")]
    pub path: PathBuf,
    pub name: String,
    pub category: String,
    pub app_id: Option<String>,
    pub modified_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ProjectApp {
    pub app_id: String,
    pub name: String,
    pub template: String,
}

#[derive(serde::Deserialize)]
struct PresentationSnapshot {
    projects: Vec<ProjectPresentation>,
}

/// Decode HomeGarden's cwd map without turning native directory identity into display text.
pub fn decode_working_directories(json: &str) -> serde_json::Result<HashMap<String, PathBuf>> {
    #[derive(serde::Deserialize)]
    struct Directories(#[serde(with = "templar_types::native_path::map")] HashMap<String, PathBuf>);
    serde_json::from_str::<Directories>(json).map(|directories| directories.0)
}

/// `HomeGarden1.Contexts`, keyed by root. Empty when sessiond is absent or the garden is off.
pub async fn fetch_places() -> HashMap<PathBuf, GardenPlace> {
    let Ok(connection) = crate::data::session().await else {
        return HashMap::new();
    };
    let Ok(proxy) = HomeGardenProxy::new(&connection).await else {
        return HashMap::new();
    };
    let Ok(json) = proxy.contexts().await else {
        return HashMap::new();
    };
    let mut places: HashMap<PathBuf, GardenPlace> = serde_json::from_str::<Vec<GardenPlace>>(&json)
        .unwrap_or_default()
        .into_iter()
        .filter(|place| !place.hidden)
        .map(|place| (place.root.clone(), place))
        .collect();
    if let Ok(json) = proxy.presentation_v1().await
        && let Ok(snapshot) = serde_json::from_str::<PresentationSnapshot>(&json)
    {
        for presentation in snapshot.projects {
            if let Some(place) = places.get_mut(&presentation.root) {
                place.instances.clone_from(&presentation.instances);
                place.presentation = presentation;
            }
        }
    }
    places
}

pub async fn resume_session(path: &std::path::Path, session_id: &str) -> Result<(), String> {
    let connection = crate::data::session().await?;
    let proxy = HomeGardenProxy::new(&connection)
        .await
        .map_err(|error| format!("garden unavailable: {error}"))?;
    proxy
        .resume_session_v1(&path.to_string_lossy(), session_id)
        .await
        .map(|_| ())
        .map_err(|error| format!("resume failed: {error}"))
}

pub async fn launch_app_in_place(
    path: &std::path::Path,
    app: &templar_types::AppId,
) -> Result<(), String> {
    let connection = crate::data::session().await?;
    let proxy = HomeGardenProxy::new(&connection)
        .await
        .map_err(|error| format!("garden unavailable: {error}"))?;
    proxy
        .launch_app_in_place_v1(&path.to_string_lossy(), &app.to_string())
        .await
        .map(|_| ())
        .map_err(|error| format!("project launch failed: {error}"))
}

pub async fn activate_instance(instance: templar_types::InstanceId) {
    let Ok(connection) = crate::data::session().await else {
        return;
    };
    let Ok(proxy) = HomeGardenProxy::new(&connection).await else {
        return;
    };
    let _ = proxy.activate_instance_v1(&instance.to_string()).await;
}

/// One tending moment for `output`. Returns the daemon's report, or the reason it could not.
pub async fn tend(output: &str) -> Result<String, String> {
    let connection = crate::data::session().await?;
    let proxy = HomeGardenProxy::new(&connection)
        .await
        .map_err(|error| format!("garden unavailable: {error}"))?;
    proxy
        .tend(output)
        .await
        .map_err(|error| format!("tending failed: {error}"))
}

/// Fire-and-forget: tell the garden one thing happened. Never an error for the caller.
///
/// No instance id, deliberately. The only acts that associate a window with a place are the ones
/// the daemon itself brokered — it is the side that knows which instance a launch produced — so
/// the association is made there (`homegarden::observe_launch`) and this stays what it has always
/// been: a report that something significant happened at a path.
pub async fn observe(kind: &str, path: &std::path::Path, app_id: &str) {
    let Ok(connection) = crate::data::session().await else {
        return;
    };
    let Ok(proxy) = HomeGardenProxy::new(&connection).await else {
        return;
    };
    let _ = proxy
        .observe(kind, &path.to_string_lossy(), app_id, "")
        .await;
}

/// Say outright that a running window belongs to this place.
///
/// The window is named by its *instance* — the launch it came from — because that is what a
/// window and a place have in common: `ShellToplevel.instance_id` is the value the daemon's own
/// launch record filed the place's other windows under, so an adopt lands in the same set by the
/// same key. A window with no instance is a window no launch of ours brokered, and there is
/// nothing to adopt.
pub async fn adopt(
    path: &std::path::Path,
    instance: templar_types::InstanceId,
) -> Result<(), String> {
    let connection = crate::data::session().await?;
    let proxy = HomeGardenProxy::new(&connection)
        .await
        .map_err(|error| format!("garden unavailable: {error}"))?;
    proxy
        .adopt_window(&path.to_string_lossy(), &instance.to_string())
        .await
        .map_err(|error| format!("could not adopt that window: {error}"))
}

/// `garden.pin` and friends against one root; `why` returns the description.
pub async fn manage(id: &str, path: &std::path::Path) -> Result<Option<String>, String> {
    let connection = crate::data::session().await?;
    let proxy = HomeGardenProxy::new(&connection)
        .await
        .map_err(|error| format!("garden unavailable: {error}"))?;
    let root = path.to_string_lossy();
    let outcome = match id {
        "garden.pin" => proxy.pin(&root, true).await.map(|()| None),
        "garden.keep" => proxy.keep(&root).await.map(|()| None),
        "garden.unpin" => proxy.pin(&root, false).await.map(|()| None),
        "garden.hide" => proxy.hide(&root, true).await.map(|()| None),
        "garden.forget" => proxy.forget(&root).await.map(|()| None),
        "garden.forget-all" => proxy.forget_all().await.map(|()| None),
        "garden.show-coding" => proxy.show_as_v1(&root, "coding").await.map(|()| None),
        "garden.show-video" => proxy.show_as_v1(&root, "video").await.map(|()| None),
        "garden.show-images" => proxy.show_as_v1(&root, "images").await.map(|()| None),
        "garden.show-documents" => proxy.show_as_v1(&root, "documents").await.map(|()| None),
        "garden.show-plain" => proxy.show_as_v1(&root, "plain").await.map(|()| None),
        "garden.why" => proxy.describe(&root).await.map(|json| {
            let value: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            Some(match value["why"].as_str() {
                Some(why) if !why.is_empty() => {
                    format!("{why} — {}", value["summary"].as_str().unwrap_or(""))
                }
                _ if json.is_empty() => "not a place the garden knows".to_owned(),
                _ => format!(
                    "not yet tended — {}",
                    value["summary"].as_str().unwrap_or("")
                ),
            })
        }),
        other => return Err(format!("{other} is not a garden verb")),
    };
    outcome.map_err(|error| format!("{id} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_context_payload_with_unknown_fields_still_parses() {
        let place: GardenPlace = serde_json::from_str(
            r#"{"root":"/home/dk/x","stage":"tile","effective":9.5,"roles":["terminal"],
                "tree":[{"name":"src","is_dir":true,"score":3.0,"last":{"session":1,"active_ms":0}}],
                "summary":"3 opens, 1 session","form":"branch","material_seed":7,"tile":42}"#,
        )
        .unwrap();
        assert_eq!(place.stage, "tile");
        assert_eq!(place.tree[0].name, "src");
        assert_eq!(place.form, "branch");
        assert_eq!(place.summary, "3 opens, 1 session");
    }

    #[test]
    fn presentation_retains_actionable_work_without_replacing_ledger_explanation() {
        let place: GardenPlace = serde_json::from_str(
            r#"{"root":"/home/dk/work","summary":"4 opens, 2 sessions","pinned":true,
                "bloom_total":2,"presentation":{"root":"/home/dk/work","template":"coding",
                "activity":"resting","state_line":"Resume: auth refactor","sessions":[{
                    "id":"auth","program":"claude","state":"ended","cwd":"/home/dk/work"
                }]}}"#,
        )
        .unwrap();
        assert_eq!(place.summary, "4 opens, 2 sessions");
        assert_eq!(place.presentation.state_line, "Resume: auth refactor");
        assert_eq!(place.presentation.sessions[0].id, "auth");
        assert_eq!(
            place.presentation.sessions[0].cwd,
            PathBuf::from("/home/dk/work")
        );
        assert!(
            place.presentation.apps.is_empty(),
            "missing data is not invented"
        );
    }

    #[test]
    fn anonymous_recents_and_distinct_native_sessions_preserve_the_snapshot() {
        use std::os::unix::ffi::OsStringExt;
        #[derive(serde::Serialize)]
        struct Native<'a>(#[serde(with = "templar_types::native_path")] &'a std::path::Path);
        let paths: Vec<_> = [0xfe, 0xff]
            .into_iter()
            .map(|byte| {
                let mut bytes = b"/home/dk/work/native-".to_vec();
                bytes.push(byte);
                PathBuf::from(std::ffi::OsString::from_vec(bytes))
            })
            .collect();
        let payload = serde_json::json!({"version":1,"projects":[{
            "root":"/home/dk/work","template":"coding","state_line":"Resume: Terminal session",
            "sessions":[
                {"id":"first","cwd":Native(&paths[0]),"kind":"terminal","state":"ended"},
                {"id":"second","cwd":Native(&paths[1]),"kind":"terminal","state":"ended"}
            ],
            "recent_files":[
                {"path":Native(&paths[0]),"name":"native-�","app_id":null},
                {"path":"/home/dk/work/notes.txt","name":"notes.txt","app_id":"org.example.Editor"}
            ],
            "apps":[{"app_id":"org.example.Terminal","name":"Terminal","template":"coding"}]
        }]})
        .to_string();
        let snapshot: PresentationSnapshot = serde_json::from_str(&payload).unwrap();
        let project = &snapshot.projects[0];
        assert_eq!(project.state_line, "Resume: Terminal session");
        assert_eq!(project.sessions.len(), 2);
        assert_eq!(project.sessions[0].cwd, paths[0]);
        assert_eq!(project.sessions[1].cwd, paths[1]);
        assert_ne!(project.sessions[0].cwd, project.sessions[1].cwd);
        assert_eq!(project.recent_files.len(), 2);
        assert_eq!(project.recent_files[0].path, paths[0]);
        assert_eq!(project.recent_files[0].app_id, None);
        assert_eq!(
            project.recent_files[1].app_id.as_deref(),
            Some("org.example.Editor")
        );
        assert_eq!(project.apps[0].app_id, "org.example.Terminal");
        let cwd_json = serde_json::json!({"first":Native(&paths[0]),"second":Native(&paths[1]),"text":"/home/dk/work"}).to_string();
        let cwds = decode_working_directories(&cwd_json).unwrap();
        assert_eq!(cwds["first"], paths[0]);
        assert_eq!(cwds["second"], paths[1]);
        assert_eq!(cwds["text"], PathBuf::from("/home/dk/work"));
    }

    /// A place's windows are the ones that came out of it, and only those. The daemon vouches
    /// for the association; this side only recognises it — an unparseable or unknown instance is
    /// a window that is not this place's, never a window that might be.
    #[test]
    fn a_place_claims_only_the_windows_its_own_launches_produced() {
        let mine = templar_types::InstanceId::new();
        let theirs = templar_types::InstanceId::new();
        let place: GardenPlace = serde_json::from_str(&format!(
            r#"{{"root":"/home/dk/work","summary":"x","instances":["{mine}","not-a-uuid"]}}"#
        ))
        .unwrap();

        let window = |id: u64, instance: Option<templar_types::InstanceId>, shelved: bool| {
            templar_window::ShellToplevel {
                window_id: templar_window::WindowId(id),
                canonical_app_id: Some("org.example.App".parse().unwrap()),
                claimed_app_id: None,
                instance_id: instance,
                title: String::new(),
                verified_identity: true,
                activated: false,
                mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
                fullscreen: false,
                shelved,
                maximized: false,
                geometry: templar_window::Rect::new(0, 0, 100, 100),
                layer: templar_window::Layer::Normal,
                group: None,
                tab_count: None,
            }
        };
        let toplevels = vec![
            window(1, Some(theirs), false),
            window(2, Some(mine), false),
            // Parked: still belongs here and must remain reachable in the project's overview.
            window(3, Some(mine), true),
            // No identity at all — the ordinary case for an unbrokered client.
            window(4, None, false),
        ];
        assert_eq!(
            place.windows(&toplevels),
            vec![templar_window::WindowId(2), templar_window::WindowId(3)],
            "both live and parked verified members remain reachable"
        );

        // A place that has launched nothing claims nothing, whatever is on screen.
        let quiet: GardenPlace =
            serde_json::from_str(r#"{"root":"/home/dk/x","summary":"y"}"#).unwrap();
        assert!(quiet.windows(&toplevels).is_empty());
    }
}
