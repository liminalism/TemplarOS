//! Operations drawer: `org.legeos.Files.Operations1.Recent` — file copies, moves and deletions
//! as journalled transactions, with their progress.
//!
//! The service, not this drawer, owns the journal (`docs/architecture/files.md`), which is what
//! lets a long copy survive the shell restarting underneath it: this module re-reads the current
//! state on every render rather than accumulating anything of its own.
//!
//! [`Operation`] deliberately decodes only the fields the drawer shows. The real record also
//! carries the step list, the resume cursor and the conflict options; a text drawer that printed
//! them would be a debugger, not a drawer, and `lege files operations` is where that belongs.

use crate::drawers::DrawerFeed;
use crate::format;
use anyhow::{Context, Result};
use async_trait::async_trait;
use lege_client::proxies::FilesOperationsProxy;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Mirrors the `kind` tag of `services/lege-filed/src/operations.rs::OperationKind`. Only the
/// fields used to name the operation are decoded.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum OperationKind {
    Copy {
        sources: Vec<PathBuf>,
        destination_dir: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        destination_dir: PathBuf,
    },
    Rename {
        path: PathBuf,
        new_name: String,
    },
    Trash {
        paths: Vec<PathBuf>,
    },
    RestoreFromTrash {
        paths: Vec<PathBuf>,
    },
    Delete {
        paths: Vec<PathBuf>,
    },
}

impl OperationKind {
    /// One phrase naming what this operation does, matching `OperationKind::summary`'s register
    /// in the service.
    pub fn summary(&self) -> String {
        fn names(paths: &[PathBuf]) -> String {
            match paths {
                [] => "nothing".to_owned(),
                [one] => display_name(one),
                many => format!("{} items", many.len()),
            }
        }
        match self {
            Self::Copy {
                sources,
                destination_dir,
            } => format!("Copy {} to {}", names(sources), destination_dir.display()),
            Self::Move {
                sources,
                destination_dir,
            } => format!("Move {} to {}", names(sources), destination_dir.display()),
            Self::Rename { path, new_name } => {
                format!("Rename {} to {new_name}", display_name(path))
            }
            Self::Trash { paths } => format!("Move {} to Trash", names(paths)),
            Self::RestoreFromTrash { paths } => format!("Restore {} from Trash", names(paths)),
            Self::Delete { paths } => format!("Delete {} permanently", names(paths)),
        }
    }
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Mirrors `services/lege-filed/src/operations.rs::Operation`, decoding only what the drawer
/// renders. `#[serde(default)]` on the counters keeps a record written by an older or newer
/// service from failing the whole listing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Operation {
    pub id: String,
    pub kind: OperationKind,
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

impl Operation {
    /// Progress as a percentage, by bytes when the operation moves any and by items otherwise —
    /// a trash or rename operation moves no bytes at all, and would otherwise always read 0%.
    pub fn percent(&self) -> u8 {
        if self.total_bytes > 0 {
            format::percent(self.done_bytes, self.total_bytes)
        } else {
            format::percent(self.done_items, self.total_items)
        }
    }
}

/// What the Operations drawer needs from `Files.Operations1`, behind a trait so tests never touch
/// a session bus.
#[async_trait]
pub trait OperationsClient: Send + Sync {
    /// Raw `operations_json`, newest first.
    async fn recent(&self, limit: u32) -> Result<String>;
}

#[async_trait]
impl OperationsClient for FilesOperationsProxy<'_> {
    async fn recent(&self, limit: u32) -> Result<String> {
        self.recent(limit).await.context("Files.Operations1.Recent")
    }
}

/// Parse `Files.Operations1.Recent`'s payload. Shared with the Trash drawer, which filters the
/// same list rather than asking the service a second time.
pub fn parse(json: &str) -> Result<Vec<Operation>> {
    serde_json::from_str(json).context("parsing Files.Operations1.Recent operations_json")
}

pub async fn render(client: &impl OperationsClient, limit: u32) -> Result<String> {
    Ok(feed(client, limit).await?.to_text())
}

pub async fn feed(client: &impl OperationsClient, limit: u32) -> Result<DrawerFeed> {
    Ok(feed_operations(&parse(&client.recent(limit).await?)?))
}

pub fn render_operations(operations: &[Operation]) -> String {
    feed_operations(operations).to_text()
}

pub fn feed_operations(operations: &[Operation]) -> DrawerFeed {
    if operations.is_empty() {
        return DrawerFeed::new("nothing running or recent", Vec::new());
    }
    // What matters at a glance is whether anything is *still going*, not how many have ever run.
    let running = operations
        .iter()
        .filter(|operation| operation.percent() < 100 && operation.error.is_none())
        .count();
    let headline = if running == 0 {
        format!("{} finished", operations.len())
    } else {
        format!("{running} running of {}", operations.len())
    };
    let lines = operations
        .iter()
        .map(|operation| {
            let mut line = format!(
                "{:<10} {:>3}%  {:<20} {}",
                operation.state,
                operation.percent(),
                format!(
                    "{}/{}",
                    format::bytes(operation.done_bytes),
                    format::bytes(operation.total_bytes)
                ),
                operation.kind.summary(),
            );
            if let Some(error) = &operation.error {
                line.push_str(" — ");
                line.push_str(error);
            }
            line
        })
        .collect();
    DrawerFeed::new(headline, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeOperations(&'static str);

    #[async_trait]
    impl OperationsClient for FakeOperations {
        async fn recent(&self, _limit: u32) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    const FIXTURE: &str = r#"[
        {"id":"11111111-1111-4111-8111-111111111111",
         "kind":{"kind":"copy","sources":["/home/u/a.iso"],"destination_dir":"/media/stick"},
         "state":"running","total_bytes":1048576,"done_bytes":524288,"total_items":1,"done_items":0,
         "error":null},
        {"id":"22222222-2222-4222-8222-222222222222",
         "kind":{"kind":"trash","paths":["/home/u/old.txt","/home/u/older.txt"]},
         "state":"committed","total_bytes":0,"done_bytes":0,"total_items":2,"done_items":2,
         "error":null},
        {"id":"33333333-3333-4333-8333-333333333333",
         "kind":{"kind":"delete","paths":["/home/u/gone.bin"]},
         "state":"failed","total_bytes":0,"done_bytes":0,"total_items":1,"done_items":0,
         "error":"permission denied"}
    ]"#;

    #[test]
    fn a_service_shaped_fixture_parses_every_kind() {
        let operations = parse(FIXTURE).unwrap();
        assert_eq!(operations.len(), 3);
        assert!(matches!(operations[0].kind, OperationKind::Copy { .. }));
        assert!(matches!(operations[1].kind, OperationKind::Trash { .. }));
        assert!(matches!(operations[2].kind, OperationKind::Delete { .. }));
    }

    #[test]
    fn every_kind_summarises_the_way_the_service_words_it() {
        let operations = parse(FIXTURE).unwrap();
        assert_eq!(operations[0].kind.summary(), "Copy a.iso to /media/stick");
        assert_eq!(operations[1].kind.summary(), "Move 2 items to Trash");
        assert_eq!(operations[2].kind.summary(), "Delete gone.bin permanently");
    }

    #[test]
    fn a_byteless_operation_reports_progress_by_items_rather_than_always_zero() {
        let operations = parse(FIXTURE).unwrap();
        // The copy has bytes, so it uses them: 512 KiB of 1 MiB.
        assert_eq!(operations[0].percent(), 50);
        // The trash moves no bytes at all; by items it is complete.
        assert_eq!(operations[1].percent(), 100);
    }

    #[tokio::test]
    async fn render_shows_state_progress_and_a_failure_reason() {
        let feed = feed(&FakeOperations(FIXTURE), 10).await.unwrap();
        assert!(
            feed.headline.contains("running"),
            "the glance says whether anything is still going: {}",
            feed.headline
        );
        let lines = &feed.lines;
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("running"));
        assert!(lines[0].contains("50%"));
        assert!(lines[0].contains("512.0 KiB/1.0 MiB"));
        assert!(lines[2].contains("failed"));
        assert!(lines[2].contains("permission denied"));
    }

    #[tokio::test]
    async fn nothing_recent_is_said_plainly() {
        assert_eq!(
            render(&FakeOperations("[]"), 10).await.unwrap(),
            "nothing running or recent"
        );
    }

    #[tokio::test]
    async fn malformed_json_is_a_clear_error_not_a_panic() {
        let error = render(&FakeOperations("not json"), 10).await.unwrap_err();
        assert!(error.to_string().contains("Files.Operations1.Recent"));
    }
}
