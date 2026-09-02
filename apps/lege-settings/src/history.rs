//! History page (`org.legeos.History1`): every important system decision, with its cause,
//! its follow-up actions, and whether it can still be undone.
//!
//! [`Event`] mirrors the wire shape of `crates/lege-session-core/src/history.rs`'s `Event`
//! (`id, at_ms, kind, app_id, instance_id, message, cause, actions, reversible`), which is what
//! `Recent`/`Search` return, one JSON document per string.

use anyhow::{Context, Result};
use lege_types::{AppId, InstanceId};
use serde::Deserialize;

/// Mirrors `EventKind` (`crates/lege-session-core/src/history.rs`) — a fieldless enum, so it
/// serializes to a bare kebab-case string, not a tagged object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EventKind {
    FileOperation,
    AssociationChange,
    SettingChange,
    ProviderChange,
    AutostartDecision,
    ResourceAction,
    Crash,
    Update,
    DriverChange,
    Notification,
    Launch,
}

impl EventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::FileOperation => "file-operation",
            Self::AssociationChange => "association-change",
            Self::SettingChange => "setting-change",
            Self::ProviderChange => "provider-change",
            Self::AutostartDecision => "autostart-decision",
            Self::ResourceAction => "resource-action",
            Self::Crash => "crash",
            Self::Update => "update",
            Self::DriverChange => "driver-change",
            Self::Notification => "notification",
            Self::Launch => "launch",
        }
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Mirrors `EventAction` (`{"label": .., "command": ..}`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EventAction {
    pub(crate) label: String,
    pub(crate) command: String,
}

/// Mirrors `Event`'s exact field set and names — the JSON `Recent`/`Search` hand back.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Event {
    pub(crate) id: String,
    pub(crate) at_ms: i64,
    pub(crate) kind: EventKind,
    pub(crate) app_id: Option<AppId>,
    pub(crate) instance_id: Option<InstanceId>,
    pub(crate) message: String,
    pub(crate) cause: Option<String>,
    pub(crate) actions: Vec<EventAction>,
    pub(crate) reversible: bool,
}

impl Event {
    /// One line: timestamp, kind, subject, message, cause, actions, reversibility — everything
    /// the brief asks a history entry to show.
    pub(crate) fn render(&self) -> String {
        let subject = match (&self.app_id, &self.instance_id) {
            (Some(app_id), Some(instance_id)) => format!(" [{app_id}/{instance_id}]"),
            (Some(app_id), None) => format!(" [{app_id}]"),
            (None, Some(instance_id)) => format!(" [{instance_id}]"),
            (None, None) => String::new(),
        };
        let cause = self
            .cause
            .as_deref()
            .map_or(String::new(), |cause| format!(" cause={cause}"));
        let actions = if self.actions.is_empty() {
            String::new()
        } else {
            let joined = self
                .actions
                .iter()
                .map(|action| format!("{}:{}", action.label, action.command))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" actions=[{joined}]")
        };
        format!(
            "[{}ms] {} ({}){subject} — {}{cause}{actions} reversible={}",
            self.at_ms, self.kind, self.id, self.message, self.reversible
        )
    }

    /// The headline a person reads: what happened, in the event's own words.
    ///
    /// The kind is not repeated here — the graphical page shows it as its own badge, and a line
    /// that begins "setting-change" before saying anything is a line that spends its first words
    /// on the filing system rather than on the event.
    pub(crate) fn headline(&self) -> &str {
        &self.message
    }

    /// The line under the headline: when, to whom, and why.
    pub(crate) fn detail(&self, now_ms: i64, names: &crate::names::AppNames) -> String {
        let mut parts = vec![relative_time(now_ms - self.at_ms)];
        if let Some(app_id) = &self.app_id {
            parts.push(names.name(app_id.as_str()).to_owned());
        }
        if let Some(cause) = &self.cause {
            parts.push(cause.clone());
        }
        parts.join(" · ")
    }
}

/// How long ago, in the words a person would use. Coarse on purpose: a history page is read to
/// place an event in the day, not to time it, and "3 minutes ago" that is really 3 minutes and
/// 40 seconds is the right answer for that question.
///
/// A negative age — an event stamped in the future by a clock that has since been corrected — is
/// reported as such rather than being shown as a very old event, which is what wrapping the
/// subtraction into an unsigned duration would produce.
pub(crate) fn relative_time(age_ms: i64) -> String {
    if age_ms < 0 {
        return "in the future".to_owned();
    }
    let seconds = age_ms / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    match (days, hours, minutes) {
        (0, 0, 0) => "just now".to_owned(),
        (0, 0, 1) => "1 minute ago".to_owned(),
        (0, 0, minutes) => format!("{minutes} minutes ago"),
        (0, 1, _) => "1 hour ago".to_owned(),
        (0, hours, _) => format!("{hours} hours ago"),
        (1, _, _) => "yesterday".to_owned(),
        (days, _, _) => format!("{days} days ago"),
    }
}

/// Milliseconds since the Unix epoch, for [`Event::detail`]. Saturates rather than panicking on
/// a clock before the epoch: a settings page has nothing useful to do with that failure.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Parse what `Recent`/`Search` returned, dropping only the documents that will not decode.
///
/// The graphical page needs the events themselves, not a rendering of them; an undecodable one
/// is left out because there is no row to draw for an event whose id, kind and message are all
/// unknown — the text renderer, which can say "could not parse", keeps reporting it.
pub(crate) fn parse_events(raw_events: &[String]) -> Vec<Event> {
    raw_events
        .iter()
        .filter_map(|raw| serde_json::from_str(raw).ok())
        .collect()
}

/// The bus operations this page needs.
pub(crate) trait HistorySource {
    async fn recent(&self, limit: u32) -> Result<Vec<String>>;
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<String>>;
    async fn undo(&self, event_id: &str) -> Result<bool>;
}

impl HistorySource for lege_client::proxies::HistoryProxy<'_> {
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

pub(crate) async fn render_with<S: HistorySource>(
    source: &S,
    query: Option<&str>,
    limit: u32,
    undo: Option<&str>,
) -> String {
    let mut sections = Vec::new();
    if let Some(event_id) = undo {
        sections.push(render_undo(source, event_id).await);
    }

    let fetched = match query {
        Some(query) => source.search(query, limit).await,
        None => source.recent(limit).await,
    };
    let raw_events = match fetched {
        Ok(events) => events,
        Err(error) => {
            sections.push(format!("service unavailable: {error:#}"));
            return sections.join("\n");
        }
    };
    sections.push(render_events(&raw_events));
    sections.join("\n")
}

async fn render_undo<S: HistorySource>(source: &S, event_id: &str) -> String {
    match source.undo(event_id).await {
        Ok(true) => format!("undo {event_id}: done."),
        Ok(false) => {
            format!("undo {event_id}: nothing to undo (already undone, or not reversible).")
        }
        Err(error) => format!("undo {event_id}: service unavailable: {error:#}"),
    }
}

fn render_events(raw_events: &[String]) -> String {
    if raw_events.is_empty() {
        return "No matching events.".to_owned();
    }
    raw_events
        .iter()
        .map(|raw| match serde_json::from_str::<Event>(raw) {
            Ok(event) => event.render(),
            Err(error) => format!("(could not parse event: {error}): {raw}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn render(
    connection: &zbus::Connection,
    query: Option<&str>,
    limit: u32,
    undo: Option<&str>,
) -> String {
    match lege_client::proxies::HistoryProxy::new(connection).await {
        Ok(proxy) => render_with(&proxy, query, limit, undo).await,
        Err(error) => format!("service unavailable: could not reach History1 ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "id": "5f5e1b1a-0000-4000-8000-000000000001",
        "at_ms": 1700000000000,
        "kind": "autostart-decision",
        "app_id": "org.legeos.NetworkTray",
        "instance_id": null,
        "message": "org.legeos.NetworkTray was allowed to autostart as network-status-ui's selected provider.",
        "cause": "providers:selected",
        "actions": [{"label": "Deny", "command": "autostart:deny:org.legeos.NetworkTray"}],
        "reversible": true
    }"#;

    #[test]
    fn relative_time_speaks_in_the_units_a_person_would_use() {
        assert_eq!(relative_time(0), "just now");
        assert_eq!(relative_time(59_000), "just now");
        assert_eq!(relative_time(60_000), "1 minute ago");
        assert_eq!(relative_time(59 * 60_000), "59 minutes ago");
        assert_eq!(relative_time(60 * 60_000), "1 hour ago");
        assert_eq!(relative_time(5 * 60 * 60_000), "5 hours ago");
        assert_eq!(relative_time(25 * 60 * 60_000), "yesterday");
        assert_eq!(relative_time(3 * 24 * 60 * 60_000), "3 days ago");
    }

    #[test]
    fn a_future_timestamp_is_named_rather_than_shown_as_an_ancient_event() {
        assert_eq!(relative_time(-5_000), "in the future");
    }

    #[test]
    fn detail_reads_when_who_and_why_without_repeating_the_message() {
        let event: Event = serde_json::from_str(SAMPLE).unwrap();
        let detail = event.detail(event.at_ms + 2 * 60_000, &crate::names::AppNames::default());
        assert_eq!(
            detail,
            "2 minutes ago · org.legeos.NetworkTray · providers:selected"
        );
        assert_eq!(event.headline(), event.message);
        assert!(!detail.contains(&event.message));
    }

    #[test]
    fn parse_events_keeps_the_readable_ones_and_drops_only_the_broken() {
        let events = parse_events(&[SAMPLE.to_owned(), "not json".to_owned()]);
        assert_eq!(events.len(), 1);
        assert!(events[0].reversible);
    }

    #[test]
    fn event_deserializes_from_the_real_wire_shape() {
        let event: Event = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(event.kind, EventKind::AutostartDecision);
        assert_eq!(
            event.app_id.as_ref().map(AppId::as_str),
            Some("org.legeos.NetworkTray")
        );
        assert!(event.instance_id.is_none());
        assert_eq!(event.actions.len(), 1);
    }

    #[test]
    fn render_includes_every_field_the_brief_asks_for() {
        let event: Event = serde_json::from_str(SAMPLE).unwrap();
        let rendered = event.render();
        assert!(rendered.contains("autostart-decision"));
        assert!(rendered.contains("org.legeos.NetworkTray"));
        assert!(rendered.contains("was allowed to autostart"));
        assert!(rendered.contains("cause=providers:selected"));
        assert!(rendered.contains("Deny:autostart:deny:org.legeos.NetworkTray"));
        assert!(rendered.contains("reversible=true"));
    }

    struct FakeHistory {
        events: Vec<String>,
        undo_result: Result<bool>,
    }

    impl HistorySource for FakeHistory {
        async fn recent(&self, _limit: u32) -> Result<Vec<String>> {
            Ok(self.events.clone())
        }

        async fn search(&self, query: &str, _limit: u32) -> Result<Vec<String>> {
            Ok(self
                .events
                .iter()
                .filter(|event| event.contains(query))
                .cloned()
                .collect())
        }

        async fn undo(&self, _event_id: &str) -> Result<bool> {
            match &self.undo_result {
                Ok(value) => Ok(*value),
                Err(error) => Err(anyhow::anyhow!("{error}")),
            }
        }
    }

    #[tokio::test]
    async fn render_with_shows_recent_events_by_default() {
        let fake = FakeHistory {
            events: vec![SAMPLE.to_owned()],
            undo_result: Ok(true),
        };
        let rendered = render_with(&fake, None, 20, None).await;
        assert!(rendered.contains("autostart-decision"));
    }

    #[tokio::test]
    async fn render_with_reports_no_matches_plainly() {
        let fake = FakeHistory {
            events: vec![],
            undo_result: Ok(true),
        };
        let rendered = render_with(&fake, Some("nothing-matches-this"), 20, None).await;
        assert!(rendered.contains("No matching events."));
    }

    #[tokio::test]
    async fn render_with_wires_undo_into_the_output() {
        let fake = FakeHistory {
            events: vec![],
            undo_result: Ok(false),
        };
        let rendered = render_with(&fake, None, 20, Some("some-event-id")).await;
        assert!(rendered.contains("undo some-event-id: nothing to undo"));
    }

    #[tokio::test]
    async fn render_with_degrades_on_a_fetch_failure() {
        struct Failing;
        impl HistorySource for Failing {
            async fn recent(&self, _limit: u32) -> Result<Vec<String>> {
                anyhow::bail!("no reply from History1")
            }
            async fn search(&self, _query: &str, _limit: u32) -> Result<Vec<String>> {
                anyhow::bail!("no reply from History1")
            }
            async fn undo(&self, _event_id: &str) -> Result<bool> {
                anyhow::bail!("no reply from History1")
            }
        }
        let rendered = render_with(&Failing, None, 20, None).await;
        assert!(rendered.contains("service unavailable:"));
    }
}
