//! `templar files` — volumes and the storage reserve, journalled operations, recents and
//! last-used directories, search, and the small path actions (`docs/architecture/files.md`).
//!
//! One trait per `org.templaros.Files.*` sub-interface, so every renderer below is testable with a
//! fixture, never a live `org.templaros.Files`.

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
impl VolumesClient for templar_client::proxies::FilesVolumesProxy<'_> {
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
impl OperationsClient for templar_client::proxies::FilesOperationsProxy<'_> {
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
pub trait TrashClient {
    async fn list(&self) -> Result<String>;
    async fn restore(&self, trashed_paths: Vec<String>, app_id: &str) -> Result<String>;
    async fn delete_permanently(&self, trashed_paths: Vec<String>, app_id: &str) -> Result<String>;
    async fn empty(&self, app_id: &str) -> Result<String>;
}

#[async_trait]
impl TrashClient for templar_client::proxies::FilesTrashProxy<'_> {
    async fn list(&self) -> Result<String> {
        self.list().await.context("Files.Trash1.List")
    }
    async fn restore(&self, trashed_paths: Vec<String>, app_id: &str) -> Result<String> {
        self.restore(trashed_paths, app_id)
            .await
            .context("Files.Trash1.Restore")
    }
    async fn delete_permanently(&self, trashed_paths: Vec<String>, app_id: &str) -> Result<String> {
        self.delete_permanently(trashed_paths, app_id)
            .await
            .context("Files.Trash1.DeletePermanently")
    }
    async fn empty(&self, app_id: &str) -> Result<String> {
        self.empty(app_id).await.context("Files.Trash1.Empty")
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
impl RecentsClient for templar_client::proxies::FilesRecentsProxy<'_> {
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
impl SearchClient for templar_client::proxies::FilesSearchProxy<'_> {
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

/// `org.templaros.Files.Index1` — the shared, generation-aware index (additive to `Search1`, which
/// `SearchClient` above still speaks for compatibility). See
/// `docs/planning/shared-incremental-file-index.md` and `F1-contract.md` §9.
#[async_trait]
pub trait IndexClient {
    async fn roots(&self) -> Result<String>;
    async fn search(&self, query_json: &str) -> Result<(String, u64, String)>;
    async fn reconcile(&self, roots_json: &str) -> Result<String>;
}

#[async_trait]
impl IndexClient for templar_client::proxies::FilesIndexProxy<'_> {
    async fn roots(&self) -> Result<String> {
        self.roots().await.context("Files.Index1.Roots")
    }
    async fn search(&self, query_json: &str) -> Result<(String, u64, String)> {
        self.search(query_json).await.context("Files.Index1.Search")
    }
    async fn reconcile(&self, roots_json: &str) -> Result<String> {
        self.reconcile(roots_json)
            .await
            .context("Files.Index1.Reconcile")
    }
}

#[async_trait]
pub trait ActionsClient {
    async fn render(&self, path: &str, form: &str) -> Result<String>;
    async fn forms(&self) -> Result<Vec<String>>;
    async fn terminal_directory(&self, path: &str) -> Result<String>;
}

#[async_trait]
impl ActionsClient for templar_client::proxies::FilesActionsProxy<'_> {
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

/// Mirrors `templar_filed::storage::VolumeInfo`.
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

/// Mirrors `templar_filed::operations::{Operation, OperationState}` (only the fields `templar files`
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

/// Mirrors `templar_filed::trash::TrashEntry` — one item in one of this user's trashes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrashEntry {
    pub name: String,
    pub trashed_path: PathBuf,
    pub info_path: PathBuf,
    pub original_path: PathBuf,
    pub deletion_date: String,
    pub trash_root: PathBuf,
    pub is_directory: bool,
    pub size_bytes: u64,
}

pub fn parse_trash(entries_json: &str) -> Result<Vec<TrashEntry>> {
    serde_json::from_str(entries_json).context("parsing Files.Trash1.List JSON")
}

/// The trash as a listing that is not a directory: when it went, what it was, and — the column
/// that makes restoring meaningful — where it came from.
pub fn render_trash(entries: &[TrashEntry]) -> String {
    if entries.is_empty() {
        return "the trash is empty".to_owned();
    }
    entries
        .iter()
        .map(|entry| {
            format!(
                "{}  {:>10}  {}\n{:19}  {:>10}  was {}",
                entry.deletion_date,
                if entry.is_directory {
                    "folder".to_owned()
                } else {
                    human_bytes(entry.size_bytes)
                },
                entry.trashed_path.display(),
                "",
                "",
                entry.original_path.display(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mirrors `templar_filed::recents::RecentEntry`.
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

/// Mirrors `templar_filed::lastdir::Suggestion`.
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

/// A native path as `Index1` carries it: authoritative bytes (base64), beside a lossy display
/// string. Mirrors `templar_filed::index_host::wire_path`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexWirePath {
    pub bytes_b64: String,
    pub display: String,
}

impl IndexWirePath {
    /// The path exactly as the filesystem has it, decoded from its base64 bytes rather than the
    /// lossy `display` string — what `--print0` writes, so a consumer never opens the wrong file.
    #[must_use]
    pub fn native_bytes(&self) -> Vec<u8> {
        decode_base64(&self.bytes_b64)
    }
}

/// Mirrors `templar_filed::index_host::wire_root_status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexRootStatus {
    pub root: i64,
    pub path_b64: String,
    pub display: String,
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub generation: u64,
    pub complete: bool,
    pub entry_count: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Mirrors `templar_file_index::Diagnostics`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexDiagnostics {
    pub schema_version: u32,
    pub generation: u64,
    pub planner: String,
    pub fts_available: bool,
    pub change_seq_floor: u64,
    pub change_seq_head: u64,
    pub cursor_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexRootsResponse {
    pub roots: Vec<IndexRootStatus>,
    pub diagnostics: IndexDiagnostics,
}

pub fn parse_index_roots(roots_json: &str) -> Result<IndexRootsResponse> {
    serde_json::from_str(roots_json).context("parsing Files.Index1.Roots JSON")
}

pub fn render_index_roots(response: &IndexRootsResponse) -> String {
    if response.roots.is_empty() {
        return "no indexed roots".to_owned();
    }
    let mut lines: Vec<String> = response
        .roots
        .iter()
        .map(|root| {
            let mut line = format!(
                "{:<4} {:<18} {:>10} entries  {}{}",
                root.root,
                root.state,
                root.entry_count,
                root.display,
                if root.complete { "" } else { "  (incomplete)" },
            );
            if let Some(reason) = &root.reason {
                let _ = write!(line, "  — {reason}");
            }
            line
        })
        .collect();
    lines.push(format!(
        "index: schema v{} generation {} — {} cursor(s) open, change log {}..{}",
        response.diagnostics.schema_version,
        response.diagnostics.generation,
        response.diagnostics.cursor_count,
        response.diagnostics.change_seq_floor,
        response.diagnostics.change_seq_head,
    ));
    lines.join("\n")
}

/// Mirrors `templar_filed::index_host::wire_entry_row`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexEntryRow {
    pub entry_id: i64,
    pub root: i64,
    pub path: IndexWirePath,
    pub name: IndexWirePath,
    pub display: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub mime: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexHit {
    pub row: IndexEntryRow,
    pub rank: String,
    pub present: bool,
}

/// Mirrors `templar_filed::index_host::wire_search_result` (the `page_json` `Files.Index1.Search`
/// returns).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexSearchResult {
    pub cursor: String,
    pub generation: u64,
    pub total: Option<u64>,
    pub truncated: bool,
    pub strategy: String,
    pub hits: Vec<IndexHit>,
}

pub fn parse_index_search(page_json: &str) -> Result<IndexSearchResult> {
    serde_json::from_str(page_json).context("parsing Files.Index1.Search page JSON")
}

pub fn render_index_hits(result: &IndexSearchResult) -> String {
    if result.hits.is_empty() {
        return "no matches".to_owned();
    }
    let mut lines: Vec<String> = result
        .hits
        .iter()
        .map(|hit| {
            format!(
                "{:<10} {:>10}  {}",
                hit.rank,
                human_bytes(hit.row.size_bytes),
                hit.row.path.display
            )
        })
        .collect();
    if let Some(total) = result.total
        && total > result.hits.len() as u64
    {
        lines.push(format!("({} of {total} matches shown)", result.hits.len()));
    }
    lines.join("\n")
}

/// NUL-delimited native paths, byte-exact — decoded from `bytes_b64`, never the lossy `display`
/// string — so a consumer piping this into `xargs -0` opens exactly the file that matched.
pub fn print0_index_hits(result: &IndexSearchResult, out: &mut impl std::io::Write) -> Result<()> {
    for hit in &result.hits {
        out.write_all(&hit.row.path.native_bytes())?;
        out.write_all(b"\0")?;
    }
    Ok(())
}

/// A small, dependency-free base64 decoder — the inverse of `templar_file_index::query::base64`,
/// kept local rather than pulling the whole index crate into the CLI for one function.
fn decode_base64(text: &str) -> Vec<u8> {
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        bits = (bits << 6) | value;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(((bits >> held) & 0xff) as u8);
        }
    }
    out
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
            r#"[{"path":"/a/one.pdf","app_id":"org.templaros.Viewer","at_ms":1787401920000,
                 "mime":"application/pdf","action":"opened"}]"#,
        )
        .unwrap();
        let rendered = render_recents(&entries);
        assert!(rendered.contains("opened"));
        assert!(rendered.contains("org.templaros.Viewer"));
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
    fn a_trash_listing_shows_when_it_went_and_where_it_came_from() {
        let entries = parse_trash(
            r#"[{"name":"report.pdf",
                 "trashed_path":"/home/u/.local/share/Trash/files/report.pdf",
                 "info_path":"/home/u/.local/share/Trash/info/report.pdf.trashinfo",
                 "original_path":"/home/u/work/report.pdf",
                 "deletion_date":"2026-08-30T18:04:11",
                 "trash_root":"/home/u/.local/share/Trash",
                 "is_directory":false,"size_bytes":2048},
                {"name":"old project",
                 "trashed_path":"/media/stick/.Trash-1000/files/old project",
                 "info_path":"/media/stick/.Trash-1000/info/old project.trashinfo",
                 "original_path":"/media/stick/work/old project",
                 "deletion_date":"2026-08-01T09:00:00",
                 "trash_root":"/media/stick/.Trash-1000",
                 "is_directory":true,"size_bytes":4096}]"#,
        )
        .unwrap();
        let rendered = render_trash(&entries);
        assert!(rendered.contains("2026-08-30T18:04:11"));
        assert!(rendered.contains("was /home/u/work/report.pdf"));
        // A trashed folder says "folder" rather than a size that means nothing.
        assert!(rendered.contains("folder"));
        assert!(!rendered.contains("4.0 kB"), "{rendered}");
        // The volume trash is in the same listing as the home trash.
        assert!(rendered.contains("/media/stick/.Trash-1000/files/old project"));
    }

    #[test]
    fn index_roots_render_state_and_diagnostics() {
        let response = parse_index_roots(
            r#"{"roots":[{"root":1,"path_b64":"L2hvbWUvdQ==","display":"/home/u","state":"current",
                 "reason":null,"generation":4,"complete":true,"entry_count":120,"last_error":null}],
                "diagnostics":{"schema_version":1,"generation":4,"planner":"fts5-trigram",
                 "fts_available":true,"change_seq_floor":0,"change_seq_head":12,"cursor_count":0}}"#,
        )
        .unwrap();
        let rendered = render_index_roots(&response);
        assert!(rendered.contains("current"));
        assert!(rendered.contains("/home/u"));
        assert!(rendered.contains("generation 4"));
    }

    #[test]
    fn a_stale_root_shows_its_reason_and_incompleteness() {
        let response = parse_index_roots(
            r#"{"roots":[{"root":2,"path_b64":"L21udA==","display":"/mnt","state":"stale",
                 "reason":"budget:entries","generation":1,"complete":false,"entry_count":5,
                 "last_error":null}],
                "diagnostics":{"schema_version":1,"generation":1,"planner":"like-fallback",
                 "fts_available":false,"change_seq_floor":0,"change_seq_head":0,"cursor_count":0}}"#,
        )
        .unwrap();
        let rendered = render_index_roots(&response);
        assert!(rendered.contains("incomplete"));
        assert!(rendered.contains("budget:entries"));
    }

    #[test]
    fn index_search_print0_decodes_native_bytes_not_the_lossy_display() {
        // "L2hvbWUvdQ==" / "dQ==" are the base64 of "/home/u" and "u".
        let result = parse_index_search(
            r#"{"cursor":"c1","generation":4,"total":1,"truncated":false,"strategy":"fts5-trigram",
                "hits":[{"row":{"entry_id":1,"root":1,
                  "path":{"bytes_b64":"L2hvbWUvdQ==","display":"/home/u"},
                  "name":{"bytes_b64":"dQ==","display":"u"},
                  "display":"u","size_bytes":0,"mime":null},
                  "rank":"exact","present":true}]}"#,
        )
        .unwrap();
        assert!(render_index_hits(&result).contains("/home/u"));

        let mut out = Vec::new();
        print0_index_hits(&result, &mut out).unwrap();
        assert_eq!(out, b"/home/u\0");
    }

    #[test]
    fn empty_lists_say_so_plainly() {
        assert_eq!(render_volumes(&[]), "no volumes");
        assert_eq!(render_operations(&[]), "no operations");
        assert_eq!(render_recents(&[]), "no recent files");
        assert_eq!(render_trash(&[]), "the trash is empty");
    }
}
