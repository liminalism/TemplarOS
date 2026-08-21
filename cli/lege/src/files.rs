//! `lege files` — volumes and the storage reserve, journalled operations, recents and
//! last-used directories, search, and the small path actions (`docs/architecture/files.md`).
//!
//! One trait per `org.legeos.Files.*` sub-interface, so every renderer below is testable with a
//! fixture, never a live `org.legeos.Files`.

use std::fmt::Write as _;

use crate::humanize::{bytes as human_bytes, percent, timestamp_ms};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[async_trait]
pub trait VolumesClient {
    async fn info_for_path(&self, path: &str) -> Result<String>;
    async fn list(&self) -> Result<String>;
    async fn check_reserve(&self, path: &str, pending_bytes: u64) -> Result<(String, String)>;
}

#[async_trait]
impl VolumesClient for lege_client::proxies::FilesVolumesProxy<'_> {
    async fn info_for_path(&self, path: &str) -> Result<String> {
        self.info_for_path(path)
            .await
            .context("Files.Volumes1.InfoForPath")
    }
    async fn list(&self) -> Result<String> {
        self.list().await.context("Files.Volumes1.List")
    }
    async fn check_reserve(&self, path: &str, pending_bytes: u64) -> Result<(String, String)> {
        self.check_reserve(path, pending_bytes)
            .await
            .context("Files.Volumes1.CheckReserve")
    }
}

#[async_trait]
pub trait OperationsClient {
    async fn start(&self, kind_json: &str, options_json: &str, app_id: &str) -> Result<String>;
    async fn cancel(&self, op_id: &str) -> Result<()>;
    async fn get(&self, op_id: &str) -> Result<String>;
    async fn recent(&self, limit: u32) -> Result<String>;
    async fn can_undo(&self, op_id: &str) -> Result<(bool, String)>;
    async fn undo(&self, op_id: &str) -> Result<String>;
}

#[async_trait]
impl OperationsClient for lege_client::proxies::FilesOperationsProxy<'_> {
    async fn start(&self, kind_json: &str, options_json: &str, app_id: &str) -> Result<String> {
        self.start(kind_json, options_json, app_id)
            .await
            .context("Files.Operations1.Start")
    }
    async fn cancel(&self, op_id: &str) -> Result<()> {
        self.cancel(op_id).await.context("Files.Operations1.Cancel")
    }
    async fn get(&self, op_id: &str) -> Result<String> {
        self.get(op_id).await.context("Files.Operations1.Get")
    }
    async fn recent(&self, limit: u32) -> Result<String> {
        self.recent(limit).await.context("Files.Operations1.Recent")
    }
    async fn can_undo(&self, op_id: &str) -> Result<(bool, String)> {
        self.can_undo(op_id)
            .await
            .context("Files.Operations1.CanUndo")
    }
    async fn undo(&self, op_id: &str) -> Result<String> {
        self.undo(op_id).await.context("Files.Operations1.Undo")
    }
}

#[async_trait]
pub trait RecentsClient {
    async fn record(&self, path: &str, app_id: &str, action: &str, mime: &str) -> Result<()>;
    async fn list(&self, app_id: &str, limit: u32) -> Result<String>;
    async fn forget(&self, path: &str) -> Result<()>;
    async fn prune_missing(&self) -> Result<u32>;
    async fn suggest_directory(&self, app_id: &str, operation: &str) -> Result<String>;
}

#[async_trait]
impl RecentsClient for lege_client::proxies::FilesRecentsProxy<'_> {
    async fn record(&self, path: &str, app_id: &str, action: &str, mime: &str) -> Result<()> {
        self.record(path, app_id, action, mime)
            .await
            .context("Files.Recents1.Record")
    }
    async fn list(&self, app_id: &str, limit: u32) -> Result<String> {
        self.list(app_id, limit)
            .await
            .context("Files.Recents1.List")
    }
    async fn forget(&self, path: &str) -> Result<()> {
        self.forget(path).await.context("Files.Recents1.Forget")
    }
    async fn prune_missing(&self) -> Result<u32> {
        self.prune_missing()
            .await
            .context("Files.Recents1.PruneMissing")
    }
    async fn suggest_directory(&self, app_id: &str, operation: &str) -> Result<String> {
        self.suggest_directory(app_id, operation)
            .await
            .context("Files.Recents1.SuggestDirectory")
    }
}

#[async_trait]
pub trait SearchClient {
    async fn search(&self, query: &str, roots: Vec<String>, limit: u32) -> Result<String>;
    async fn saved_queries(&self) -> Result<String>;
}

#[async_trait]
impl SearchClient for lege_client::proxies::FilesSearchProxy<'_> {
    async fn search(&self, query: &str, roots: Vec<String>, limit: u32) -> Result<String> {
        self.search(query, roots, limit)
            .await
            .context("Files.Search1.Search")
    }
    async fn saved_queries(&self) -> Result<String> {
        self.saved_queries()
            .await
            .context("Files.Search1.SavedQueries")
    }
}

#[async_trait]
pub trait ActionsClient {
    async fn render(&self, path: &str, form: &str) -> Result<String>;
    async fn forms(&self) -> Result<Vec<String>>;
    async fn terminal_directory(&self, path: &str) -> Result<String>;
}

#[async_trait]
impl ActionsClient for lege_client::proxies::FilesActionsProxy<'_> {
    async fn render(&self, path: &str, form: &str) -> Result<String> {
        self.render(path, form)
            .await
            .context("Files.Actions1.Render")
    }
    async fn forms(&self) -> Result<Vec<String>> {
        self.forms().await.context("Files.Actions1.Forms")
    }
    async fn terminal_directory(&self, path: &str) -> Result<String> {
        self.terminal_directory(path)
            .await
            .context("Files.Actions1.TerminalDirectory")
    }
}

/// Mirrors `lege_filed::storage::VolumeInfo`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub reserved_bytes: u64,
    pub read_only: bool,
}

pub fn parse_volume(volume_json: &str) -> Result<VolumeInfo> {
    serde_json::from_str(volume_json).context("parsing Files.Volumes1 volume JSON")
}

pub fn parse_volumes(volumes_json: &str) -> Result<Vec<VolumeInfo>> {
    serde_json::from_str(volumes_json).context("parsing Files.Volumes1.List JSON")
}

pub fn render_volume(volume: &VolumeInfo) -> String {
    format!(
        "{}  {}  {} free of {} ({} reserved){}",
        volume.mount_point.display(),
        volume.fs_type,
        human_bytes(volume.free_bytes),
        human_bytes(volume.total_bytes),
        human_bytes(volume.reserved_bytes),
        if volume.read_only { "  read-only" } else { "" },
    )
}

pub fn render_volumes(volumes: &[VolumeInfo]) -> String {
    if volumes.is_empty() {
        return "no volumes".to_owned();
    }
    volumes
        .iter()
        .map(render_volume)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mirrors `lege_filed::operations::{Operation, OperationState}` (only the fields `lege files`
/// renders; the full record has more, all preserved by `#[serde(default)]` tolerance elsewhere).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub done_bytes: u64,
    #[serde(default)]
    pub total_items: u64,
    #[serde(default)]
    pub done_items: u64,
    #[serde(default)]
    pub error: Option<String>,
}

pub fn parse_operation(operation_json: &str) -> Result<Operation> {
    serde_json::from_str(operation_json).context("parsing Files.Operations1 operation JSON")
}

pub fn parse_operations(operations_json: &str) -> Result<Vec<Operation>> {
    serde_json::from_str(operations_json).context("parsing Files.Operations1.Recent JSON")
}

pub fn render_operation(operation: &Operation) -> String {
    let mut line = format!(
        "{}  {:<10} {} bytes, {} items ({})",
        operation.id,
        operation.state,
        percent(operation.done_bytes, operation.total_bytes),
        percent(operation.done_items, operation.total_items),
        human_bytes(operation.done_bytes),
    );
    if let Some(error) = &operation.error {
        let _ = write!(line, " — {error}");
    }
    line
}

pub fn render_operations(operations: &[Operation]) -> String {
    if operations.is_empty() {
        return "no operations".to_owned();
    }
    operations
        .iter()
        .map(render_operation)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mirrors `lege_filed::recents::RecentEntry`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub path: PathBuf,
    #[serde(default)]
    pub app_id: Option<String>,
    pub at_ms: i64,
    #[serde(default)]
    pub mime: Option<String>,
    pub action: String,
}

pub fn parse_recents(entries_json: &str) -> Result<Vec<RecentEntry>> {
    serde_json::from_str(entries_json).context("parsing Files.Recents1.List JSON")
}

pub fn render_recents(entries: &[RecentEntry]) -> String {
    if entries.is_empty() {
        return "no recent files".to_owned();
    }
    entries
        .iter()
        .map(|entry| {
            format!(
                "{}  {} {}{}",
                timestamp_ms(entry.at_ms),
                entry.action,
                entry.path.display(),
                entry
                    .app_id
                    .as_ref()
                    .map_or_else(String::new, |id| format!(" ({id})")),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mirrors `lege_filed::lastdir::Suggestion`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "source")]
pub enum Suggestion {
    LastForApp {
        app_id: String,
        directory: PathBuf,
    },
    LastForOperation {
        operation: String,
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

pub fn parse_suggestion(suggestion_json: &str) -> Result<Suggestion> {
    serde_json::from_str(suggestion_json).context("parsing Files.Recents1.SuggestDirectory JSON")
}

pub fn render_suggestion(suggestion: &Suggestion) -> String {
    match suggestion {
        Suggestion::LastForApp { app_id, directory } => {
            format!("{} (last used by {app_id})", directory.display())
        }
        Suggestion::LastForOperation {
            operation,
            directory,
        } => {
            format!("{} (last used for {operation})", directory.display())
        }
        Suggestion::CurrentProject {
            project_id,
            directory,
        } => {
            format!("{} (the open project {project_id})", directory.display())
        }
        Suggestion::Fallback { directory } => format!("{} (fallback)", directory.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_renders_its_free_space_and_reserve() {
        let volume = parse_volume(
            r#"{"mount_point":"/","fs_type":"btrfs","total_bytes":100000000000,
                "free_bytes":20000000000,"available_bytes":20000000000,
                "reserved_bytes":3000000000,"read_only":false}"#,
        )
        .unwrap();
        let rendered = render_volume(&volume);
        assert!(rendered.contains('/'));
        assert!(rendered.contains("btrfs"));
        assert!(rendered.contains("reserved"));
        assert!(!rendered.contains("read-only"));
    }

    #[test]
    fn an_operation_shows_progress_and_any_error() {
        let operation = parse_operation(
            r#"{"id":"11112222-3333-4444-5555-666677778888","state":"running",
                "total_bytes":1000,"done_bytes":250,"total_items":4,"done_items":1}"#,
        )
        .unwrap();
        let rendered = render_operation(&operation);
        assert!(rendered.contains("running"));
        assert!(rendered.contains("25%"));

        let failed = parse_operation(
            r#"{"id":"x","state":"failed","total_bytes":0,"done_bytes":0,
                "total_items":0,"done_items":0,"error":"disk full"}"#,
        )
        .unwrap();
        assert!(render_operation(&failed).contains("disk full"));
    }

    #[test]
    fn recents_render_with_the_app_that_used_them() {
        let entries = parse_recents(
            r#"[{"path":"/a/one.pdf","app_id":"org.legeos.Viewer","at_ms":1787401920000,
                 "mime":"application/pdf","action":"opened"}]"#,
        )
        .unwrap();
        let rendered = render_recents(&entries);
        assert!(rendered.contains("opened"));
        assert!(rendered.contains("org.legeos.Viewer"));
    }

    #[test]
    fn suggestions_explain_their_source() {
        let suggestion = parse_suggestion(
            r#"{"source":"current-project","project_id":"thesis","directory":"/home/u/thesis"}"#,
        )
        .unwrap();
        assert_eq!(
            render_suggestion(&suggestion),
            "/home/u/thesis (the open project thesis)"
        );
    }

    #[test]
    fn empty_lists_say_so_plainly() {
        assert_eq!(render_volumes(&[]), "no volumes");
        assert_eq!(render_operations(&[]), "no operations");
        assert_eq!(render_recents(&[]), "no recent files");
    }
}
