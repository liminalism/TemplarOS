//! What the shell knows about Homegarden's places: the `HomeGarden1.Contexts` snapshot the
//! Folder tile draws its form from (`crate::surface::place_forms`), and the fetches behind the
//! tending moments (`@legeos.decision.homegarden-activity-grown-places`). The shell owns no garden
//! state: it observes, asks for a tending, and draws what the daemon reports.

use lege_client::proxies::HomeGardenProxy;
use std::{collections::HashMap, path::PathBuf};

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
}

impl GardenPlace {
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
}
