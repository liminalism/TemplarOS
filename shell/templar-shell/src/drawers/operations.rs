//! Operations drawer: `org.templaros.Files.Operations1.Recent` — file copies, moves and deletions
//! as journalled transactions, with their progress.
//!
//! The service, not this drawer, owns the journal (`docs/architecture/files.md`), which is what
//! lets a long copy survive the shell restarting underneath it: this module re-reads the current
//! state on every render rather than accumulating anything of its own.
//!
//! [`Operation`] deliberately decodes only the fields the drawer shows. The real record also
//! carries the step list, the resume cursor and the conflict options; a text drawer that printed
//! them would be a debugger, not a drawer, and `templar files operations` is where that belongs.

use crate::drawers::DrawerFeed;
use crate::format;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use templar_client::{files::OperationRequest, proxies::FilesOperationsProxy};

/// Mirrors `services/templar-filed/src/operations.rs::Operation`, decoding only what the drawer
/// renders. `#[serde(default)]` on the counters keeps a record written by an older or newer
/// service from failing the whole listing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Operation {
    pub id: String,
    pub kind: OperationRequest,
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
        return DrawerFeed::vacant("nothing running or recent");
    }
    // What matters at a glance is whether anything is *still going*, not how many have ever run.
    let in_flight: Vec<&Operation> = operations
        .iter()
        .filter(|operation| operation.percent() < 100 && operation.error.is_none())
        .collect();
    let headline = if in_flight.is_empty() {
        format!("{} finished", operations.len())
    } else {
        format!("{} running of {}", in_flight.len(), operations.len())
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
    let feed = DrawerFeed::new(headline.clone(), lines);
    match in_flight.iter().map(|operation| operation.percent()).min() {
        Some(percent) => feed.with_progress(headline, percent),
        None => feed,
    }
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
        assert!(matches!(operations[0].kind, OperationRequest::Copy { .. }));
        assert!(matches!(operations[1].kind, OperationRequest::Trash { .. }));
        assert!(matches!(
            operations[2].kind,
            OperationRequest::Delete { .. }
        ));
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
        assert_eq!(
            feed.progress
                .as_ref()
                .map(|(label, percent)| (label.as_str(), *percent)),
            Some(("1 running of 3", 50)),
            "progress is a field, not a word recovered from the listing"
        );
        assert!(!feed.empty);
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
        let empty = feed(&FakeOperations("[]"), 10).await.unwrap();
        assert!(empty.empty);
        assert!(empty.progress.is_none());
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
