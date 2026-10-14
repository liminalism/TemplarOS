//! Recent drawer: `org.templaros.Files.Recents1.List("", limit)` — recently used files across every
//! application, newest first (`docs/architecture/files.md` "One answer about the last folder").

use crate::drawers::DrawerFeed;
use crate::format;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use templar_client::proxies::FilesRecentsProxy;

/// Mirrors `services/templar-filed/src/recents.rs::RecentEntry`'s wire JSON. `templar-filed` is not a
/// dependency of the shell (a restartable UI process has no business linking rusqlite/libc for
/// type reuse), so this is the wire contract, kept in step with the source by hand — field names
/// and the `kebab-case` action tag must match exactly, since this is what
/// `Files.Recents1.List`'s `entries_json` actually contains.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RecentEntry {
    pub path: PathBuf,
    pub app_id: Option<String>,
    pub at_ms: i64,
    pub mime: Option<String>,
    pub action: RecentAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecentAction {
    Opened,
    Saved,
    Created,
    Modified,
}

impl RecentAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Saved => "saved",
            Self::Created => "created",
            Self::Modified => "modified",
        }
    }
}

/// What the Recent drawer needs from `Files.Recents1`, behind a trait so tests never touch a
/// session bus.
#[async_trait]
pub trait RecentsClient: Send + Sync {
    /// Raw `entries_json` — parsing stays in [`render`] so a fake can hand back exactly the
    /// bytes a fixture file would contain.
    async fn list(&self, app_id: &str, limit: u32) -> Result<String>;
}

#[async_trait]
impl RecentsClient for FilesRecentsProxy<'_> {
    async fn list(&self, app_id: &str, limit: u32) -> Result<String> {
        self.list(app_id, limit)
            .await
            .context("Files.Recents1.List")
    }
}

/// Render the Recent drawer: every application's recent files, newest first.
pub async fn render(client: &impl RecentsClient, limit: u32) -> Result<String> {
    Ok(feed(client, limit).await?.to_text())
}

pub async fn feed(client: &impl RecentsClient, limit: u32) -> Result<DrawerFeed> {
    feed_json(&client.list("", limit).await?)
}

fn feed_json(json: &str) -> Result<DrawerFeed> {
    let mut entries: Vec<RecentEntry> =
        serde_json::from_str(json).context("parsing Files.Recents1.List entries_json")?;
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.at_ms));
    if entries.is_empty() {
        return Ok(DrawerFeed::vacant("nothing yet"));
    }
    let headline = DrawerFeed::count(entries.len(), "item", "items", "nothing yet");
    let lines = entries
        .iter()
        .map(|entry| {
            format!(
                "{}  {:<8} {:<24} {}",
                format::timestamp_ms(entry.at_ms),
                entry.action.as_str(),
                entry.app_id.as_deref().unwrap_or("(unknown app)"),
                entry.path.display(),
            )
        })
        .collect();
    let mut feed = DrawerFeed::new(headline, lines);
    feed.row_targets = entries
        .iter()
        .map(|entry| Some(entry.path.to_string_lossy().into_owned()))
        .collect();
    feed.recents = entries;
    Ok(feed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRecents(&'static str);

    #[async_trait]
    impl RecentsClient for FakeRecents {
        async fn list(&self, _app_id: &str, _limit: u32) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    const FIXTURE: &str = r#"[
        {"path":"/home/u/report.pdf","app_id":"org.templaros.Viewer","at_ms":1787322720000,"mime":"application/pdf","action":"opened"},
        {"path":"/home/u/thesis.md","app_id":null,"at_ms":1787322600000,"mime":null,"action":"saved"}
    ]"#;

    #[test]
    fn a_service_shaped_fixture_parses_into_the_exact_fields() {
        let entries: Vec<RecentEntry> = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, RecentAction::Opened);
        assert_eq!(entries[0].app_id.as_deref(), Some("org.templaros.Viewer"));
        assert_eq!(entries[1].app_id, None);
        assert_eq!(entries[1].action, RecentAction::Saved);
    }

    #[tokio::test]
    async fn render_lists_newest_first_with_app_and_action() {
        let feed = feed(&FakeRecents(FIXTURE), 10).await.unwrap();
        assert_eq!(feed.headline, "2 items");
        assert_eq!(feed.lines.len(), 2);
        assert!(feed.lines[0].contains("opened"));
        assert!(feed.lines[0].contains("org.templaros.Viewer"));
        assert!(feed.lines[0].contains("/home/u/report.pdf"));
        assert_eq!(feed.row_target(0), Some("/home/u/report.pdf"));
        assert_eq!(feed.recents[0].mime.as_deref(), Some("application/pdf"));
        assert_eq!(feed.row_targets.len(), feed.lines.len());
        assert!(feed.lines[1].contains("(unknown app)"));
        // The text form is the headline followed by those lines.
        let text = render(&FakeRecents(FIXTURE), 10).await.unwrap();
        assert_eq!(text.lines().count(), 3);
    }

    #[tokio::test]
    async fn an_empty_list_is_said_plainly() {
        let text = render(&FakeRecents("[]"), 10).await.unwrap();
        assert_eq!(text, "nothing yet");
    }

    #[tokio::test]
    async fn malformed_json_is_a_clear_error_not_a_panic() {
        let error = render(&FakeRecents("not json"), 10).await.unwrap_err();
        assert!(error.to_string().contains("Files.Recents1.List"));
    }
}
