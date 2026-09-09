//! What the shell knows about Homegarden's places: the `HomeGarden1.Contexts` snapshot the
//! Folder tile draws its form from (`crate::surface::place_forms`), and the fetches behind the
//! tending moments (`@templaros.decision.homegarden-activity-grown-places`). The shell owns no garden
//! state: it observes, asks for a tending, and draws what the daemon reports.

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
    /// The form the place draws in — `plain`, `branch`, `shelf`, `folio` — as the ledger settled
    /// it. A string rather than an enum: the shell draws what the daemon reports and must keep
    /// working against a daemon that has learned a form this build has never heard of.
    pub form: String,
    pub effective: f32,
    pub roles: Vec<String>,
    pub tree: Vec<TreeNode>,
    pub pinned: bool,
    /// Pinned and frozen at its size (`garden.keep`, or the user's own hand).
    pub kept: bool,
    pub hidden: bool,
    pub summary: String,
    pub why: String,
    /// What arrived here and has not been opened yet — the `2 new` on the state line. A bloom
    /// is a projection over a place that was already earned; it never grew or moved one.
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
}

impl GardenPlace {
    /// The live windows that belong to this place, most recently activated first.
    ///
    /// A window belongs when its instance is one the place remembers — strong evidence only:
    /// started here, or started to open a file under here. Never "it was open at the time".
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
            .filter(|top| !top.shelved)
            .filter(|top| {
                top.instance_id
                    .is_some_and(|id| self.instances.iter().any(|known| *known == id.to_string()))
            })
            .collect();
        mine.sort_by_key(|top| (top.mru_rank, top.window_id));
        mine.iter().map(|top| top.window_id).collect()
    }

    /// The one-line state a Small place shows under its name. News first: what arrived is the
    /// thing that changed since the user last looked.
    pub fn state_line(&self) -> String {
        let held = if self.kept {
            format!("kept · {}", self.summary)
        } else if self.pinned {
            format!("pinned · {}", self.summary)
        } else {
            self.summary.clone()
        };
        match self.bloom_total {
            0 => held,
            new => format!("{new} new · {held}"),
        }
    }

    /// Whether `path` is one of the arrivals this place has still to show. The body marks these
    /// rows; the mark itself is the painter's, not this module's.
    #[must_use]
    pub fn is_bloomed(&self, path: &std::path::Path) -> bool {
        self.bloomed.iter().any(|item| item == path)
    }
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
    serde_json::from_str::<Vec<GardenPlace>>(&json)
        .unwrap_or_default()
        .into_iter()
        .filter(|place| !place.hidden)
        .map(|place| (place.root.clone(), place))
        .collect()
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
        assert_eq!(place.state_line(), "3 opens, 1 session");
    }

    #[test]
    fn a_place_says_what_arrived_before_it_says_what_it_is() {
        let place: GardenPlace = serde_json::from_str(
            r#"{"root":"/home/dk/Downloads","stage":"expanded","summary":"4 opens, 2 sessions",
                "pinned":true,"bloom_total":2,
                "bloomed":["/home/dk/Downloads/a.mp4","/home/dk/Downloads/b.pdf"]}"#,
        )
        .unwrap();
        assert_eq!(place.state_line(), "2 new · pinned · 4 opens, 2 sessions");
        assert!(place.is_bloomed(std::path::Path::new("/home/dk/Downloads/a.mp4")));
        assert!(!place.is_bloomed(std::path::Path::new("/home/dk/Downloads/old.mp4")));

        // Nothing new is the ordinary case, and it says nothing.
        let quiet: GardenPlace =
            serde_json::from_str(r#"{"root":"/home/dk/x","summary":"1 open, 1 session"}"#).unwrap();
        assert_eq!(quiet.state_line(), "1 open, 1 session");
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
            }
        };
        let toplevels = vec![
            window(1, Some(theirs), false),
            window(2, Some(mine), false),
            // Parked: still this place's launch, but not a window anyone can be shown.
            window(3, Some(mine), true),
            // No identity at all — the ordinary case for an unbrokered client.
            window(4, None, false),
        ];
        assert_eq!(
            place.windows(&toplevels),
            vec![templar_window::WindowId(2)],
            "only the live window this place launched"
        );

        // A place that has launched nothing claims nothing, whatever is on screen.
        let quiet: GardenPlace =
            serde_json::from_str(r#"{"root":"/home/dk/x","summary":"y"}"#).unwrap();
        assert!(quiet.windows(&toplevels).is_empty());
    }
}
