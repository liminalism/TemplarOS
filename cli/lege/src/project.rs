//! `lege project` — project sessions instead of virtual desktops (planning doc 01 §7,
//! `crates/lege-session-core/src/projects.rs`): a visible, named working set of files,
//! applications and window arrangement, not a hidden screen.
//!
//! Every D-Bus call goes through [`ProjectsClient`] so the rendering below is testable with a
//! fixture, never a live `org.legeos.Projects1`.

use anyhow::{Context, Result};
use async_trait::async_trait;

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
}
