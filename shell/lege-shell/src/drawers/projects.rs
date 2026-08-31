//! Projects drawer: `org.legeos.Projects1.List` — explicit, visible working sets instead of
//! hidden virtual desktops (planning doc 01 §7).
//!
//! The point of the design is that a project is an object the user can see and name, so this
//! drawer shows every project whether or not it is open, and says which is which — a project you
//! cannot find is the problem virtual desktops have.

use crate::drawers::DrawerFeed;
use anyhow::{Context, Result};
use async_trait::async_trait;
use lege_client::proxies::ProjectsProxy;

/// One row of `Projects1.List`: `(project_id, name, root, is_open)`.
pub type ProjectRow = (String, String, String, bool);

/// What the Projects drawer needs from `Projects1`, behind a trait so tests never touch a
/// session bus.
#[async_trait]
pub trait ProjectsClient: Send + Sync {
    async fn list(&self) -> Result<Vec<ProjectRow>>;
}

#[async_trait]
impl ProjectsClient for ProjectsProxy<'_> {
    async fn list(&self) -> Result<Vec<ProjectRow>> {
        self.list().await.context("Projects1.List")
    }
}

pub async fn render(client: &impl ProjectsClient) -> Result<String> {
    Ok(feed(client).await?.to_text())
}

pub async fn feed(client: &impl ProjectsClient) -> Result<DrawerFeed> {
    Ok(feed_projects(&client.list().await?))
}

pub fn render_projects(projects: &[ProjectRow]) -> String {
    feed_projects(projects).to_text()
}

pub fn feed_projects(projects: &[ProjectRow]) -> DrawerFeed {
    if projects.is_empty() {
        return DrawerFeed::new("none yet", Vec::new());
    }
    let open = projects
        .iter()
        .filter(|(_, _, _, is_open)| *is_open)
        .count();
    let headline = format!("{open} open of {}", projects.len());
    // Open projects first: they are the ones the user is working in right now.
    let mut ordered: Vec<&ProjectRow> = projects.iter().collect();
    ordered.sort_by_key(|(id, _, _, is_open)| (!*is_open, id.clone()));
    let lines = ordered
        .into_iter()
        .map(|(id, name, root, is_open)| {
            format!(
                "{:<8} {:<24} {:<32} {}",
                if *is_open { "open" } else { "closed" },
                name,
                root,
                id,
            )
        })
        .collect();
    DrawerFeed::new(headline, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProjects(Vec<ProjectRow>);

    #[async_trait]
    impl ProjectsClient for FakeProjects {
        async fn list(&self) -> Result<Vec<ProjectRow>> {
            Ok(self.0.clone())
        }
    }

    fn fixture() -> Vec<ProjectRow> {
        vec![
            (
                "zeta".to_owned(),
                "Zeta paper".to_owned(),
                "/home/u/zeta".to_owned(),
                false,
            ),
            (
                "thesis".to_owned(),
                "Thesis".to_owned(),
                "/home/u/thesis".to_owned(),
                true,
            ),
            (
                "alpha".to_owned(),
                "Alpha notes".to_owned(),
                "/home/u/alpha".to_owned(),
                false,
            ),
        ]
    }

    #[tokio::test]
    async fn open_projects_are_listed_before_closed_ones() {
        let feed = feed(&FakeProjects(fixture())).await.unwrap();
        assert_eq!(feed.headline, "1 open of 3");
        let lines = &feed.lines;
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("open"), "{}", lines[0]);
        assert!(lines[0].contains("Thesis"));
        // The two closed projects follow, ordered by id so the drawer never reshuffles itself.
        assert!(lines[1].contains("alpha"));
        assert!(lines[2].contains("zeta"));
    }

    #[tokio::test]
    async fn every_project_shows_its_root_and_id() {
        let text = render(&FakeProjects(fixture())).await.unwrap();
        assert!(text.contains("/home/u/thesis"));
        assert!(text.contains("thesis"));
    }

    #[tokio::test]
    async fn no_projects_is_said_plainly() {
        assert_eq!(render(&FakeProjects(Vec::new())).await.unwrap(), "none yet");
    }
}
