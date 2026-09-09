//! `templar history` — the system-wide change history (`docs/architecture/history.md`): every
//! important decision as a searchable event with a cause and, where possible, an undo.
//!
//! Every D-Bus call goes through [`HistoryClient`] so the rendering below is testable with a
//! fixture, never a live `org.templaros.History1`.

use std::fmt::Write as _;

use crate::humanize::timestamp_ms;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What `templar history` needs from `org.templaros.History1`.
#[async_trait]
pub trait HistoryClient {
    async fn recent(&self, limit: u32) -> Result<Vec<String>>;
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<String>>;
    async fn undo(&self, event_id: &str) -> Result<bool>;
}

#[async_trait]
impl HistoryClient for templar_client::proxies::HistoryProxy<'_> {
    async fn recent(&self, limit: u32) -> Result<Vec<String>> {
        self.recent(limit).await.context("History1.Recent")
    }
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<String>> {
        self.search(query, limit).await.context("History1.Search")
    }
    async fn undo(&self, event_id: &str) -> Result<bool> {
        self.undo(event_id).await.context("History1.Undo")
    }
}

/// Mirrors `templar_session_core::history::EventAction`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventAction {
    pub label: String,
    pub command: String,
}

/// Mirrors `templar_session_core::history::Event`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub at_ms: i64,
    pub kind: String,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub instance_id: Option<String>,
    pub message: String,
    #[serde(default)]
    pub cause: Option<String>,
    #[serde(default)]
    pub actions: Vec<EventAction>,
    pub reversible: bool,
}

/// Parse one event JSON string from `Recent`/`Search`'s `events_json` array; a malformed entry
/// is dropped rather than failing the whole listing.
pub fn parse_events(events_json: &[String]) -> Vec<Event> {
    events_json
        .iter()
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect()
}

pub fn render_events(events: &[Event]) -> String {
    if events.is_empty() {
        return "no history events".to_owned();
    }
    let mut out = String::new();
    for event in events {
        let _ = writeln!(
            out,
            "{}  [{}]{} {}",
            timestamp_ms(event.at_ms),
            event.kind,
            event
                .app_id
                .as_ref()
                .map_or_else(String::new, |id| format!(" {id}")),
            event.message,
        );
        if !event.actions.is_empty() {
            let labels: Vec<&str> = event.actions.iter().map(|a| a.label.as_str()).collect();
            let _ = writeln!(
                out,
                "  id={}  actions: {}{}",
                event.id,
                labels.join(", "),
                if event.reversible {
                    ""
                } else {
                    " (no longer reversible)"
                },
            );
        }
    }
    out.pop();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"id":"11112222-3333-4444-5555-666677778888","at_ms":1787322720000,
        "kind":"association-change","app_id":"org.templaros.Viewer","instance_id":null,
        "message":"The default application for application/pdf changed to Templar Viewer.",
        "cause":"user","actions":[{"label":"Undo","command":"assoc:undo:1"}],"reversible":true}"#;

    #[test]
    fn events_parse_and_render() {
        let events = parse_events(&[SAMPLE.to_owned()]);
        assert_eq!(events.len(), 1);
        let rendered = render_events(&events);
        assert!(rendered.contains("association-change"));
        assert!(rendered.contains("Templar Viewer"));
        assert!(rendered.contains("Undo"));
        assert!(rendered.contains("2026-08-21"));
    }

    #[test]
    fn malformed_entries_are_dropped_not_fatal() {
        let events = parse_events(&["not json".to_owned(), SAMPLE.to_owned()]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn empty_history_says_so_plainly() {
        assert_eq!(render_events(&[]), "no history events");
    }
}
