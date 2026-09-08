//! Homegarden page: the switch behind Home's grown places, and the two lists that are the only
//! way back from a decision a person made by pointing at a tile.
//!
//! Home grows places out of the user's own activity
//! (`@legeos.decision.homegarden-activity-grown-places`). That is on by default, which makes the
//! two irreversible-feeling gestures — hiding a place, and adding a directory to the ignore list —
//! the ones that most needed a graphical way back. Hiding was reachable only from a tile that,
//! once hidden, is no longer on Home; the ignore list was reachable only through
//! `lege settings set`.
//!
//! The typed keys are `shell.home-garden.*` on `Settings1`, drawn by the ordinary key form.
//! Everything here is the part that is not a typed key: `HomeGarden1.Contexts` (a JSON array, of
//! which this page wants the hidden ones) and `HomeGarden1.Hide`.

use anyhow::{Context as _, Result};
use lege_client::proxies::HomeGardenProxy;
use serde::Deserialize;

/// One row of `HomeGarden1.Contexts`. Every other field of that JSON is another page's business,
/// and `serde` drops what is not named here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Context {
    pub(crate) root: String,
    #[serde(default)]
    pub(crate) hidden: bool,
}

/// The hidden places, in path order, out of the raw `Contexts` JSON.
///
/// Order matters only because the service's own is relevance order, which changes under the
/// person's feet between visits — a list you have to re-find your row in is a list you stop using.
pub(crate) fn hidden_places(contexts_json: &str) -> Result<Vec<String>> {
    let contexts: Vec<Context> = serde_json::from_str(contexts_json)
        .context("HomeGarden1.Contexts is not the JSON array")?;
    let mut hidden: Vec<String> = contexts
        .into_iter()
        .filter(|context| context.hidden)
        .map(|context| context.root)
        .collect();
    hidden.sort();
    Ok(hidden)
}

/// Read the hidden places from the running session.
pub(crate) async fn fetch_hidden(connection: &zbus::Connection) -> Result<Vec<String>> {
    let proxy = HomeGardenProxy::new(connection)
        .await
        .context("HomeGarden1")?;
    hidden_places(&proxy.contexts().await?)
}

/// Put one place back on Home: `Hide(path, false)`.
pub(crate) async fn unhide(connection: &zbus::Connection, path: &str) -> Result<()> {
    let proxy = HomeGardenProxy::new(connection)
        .await
        .context("HomeGarden1")?;
    proxy.hide(path, false).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing this parse must get right: a page that showed every context as hidden would
    /// offer to unhide places that were never hidden.
    #[test]
    fn only_the_hidden_contexts_are_listed_and_they_are_sorted() {
        let json = r#"[
            {"root": "/home/dk/z", "hidden": true, "relevance": 0.4},
            {"root": "/home/dk/a", "hidden": false},
            {"root": "/home/dk/b", "hidden": true}
        ]"#;
        assert_eq!(
            hidden_places(json).unwrap(),
            vec!["/home/dk/b".to_owned(), "/home/dk/z".to_owned()]
        );
        assert_eq!(hidden_places("[]").unwrap(), Vec::<String>::new());
        assert!(hidden_places("not json").is_err());
    }

    /// `hidden` is absent from a context that was never hidden, and an absent flag is not hidden.
    #[test]
    fn a_context_with_no_hidden_flag_is_not_hidden() {
        assert_eq!(
            hidden_places(r#"[{"root": "/home/dk/a"}]"#).unwrap(),
            Vec::<String>::new()
        );
    }
}
