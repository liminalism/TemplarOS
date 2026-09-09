//! System-wide change history: every important system decision can be explained, and many can
//! be undone. File operations, installs, default-handler changes, settings changes, autostart
//! decisions, driver changes, updates, pressure actions and crashes all land here.
//!
//! [`SqliteHistory`] is the persisted [`HistoryStore`]; [`append_within`] is the same insert
//! usable inside another domain's own transaction, so "change the default PDF handler *and*
//! record the history entry" is one commit (ADR-0001) without nesting a second connection lock
//! inside [`templar_state::Store::transaction`].

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use templar_types::{AppId, InstanceId};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub Uuid);

impl EventId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for EventId {
    type Err = uuid::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(text).map(Self)
    }
}

/// Coarse category, for filtering and icons.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
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
    /// Stable string stored in the `history.kind` column (not JSON: the column is read
    /// directly in hand-written SQL by other domains' combined transactions).
    pub fn as_str(self) -> &'static str {
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

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "file-operation" => Self::FileOperation,
            "association-change" => Self::AssociationChange,
            "setting-change" => Self::SettingChange,
            "provider-change" => Self::ProviderChange,
            "autostart-decision" => Self::AutostartDecision,
            "resource-action" => Self::ResourceAction,
            "crash" => Self::Crash,
            "update" => Self::Update,
            "driver-change" => Self::DriverChange,
            "notification" => Self::Notification,
            "launch" => Self::Launch,
            _ => return None,
        })
    }
}

/// A user-invocable follow-up ("Undo", "Restore", "Open folder").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAction {
    pub label: String,
    /// Opaque, interpreted by the originating domain (e.g. `assoc:undo:<change-id>`).
    pub command: String,
}

/// One entry in the history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    /// Unix time, milliseconds.
    pub at_ms: i64,
    pub kind: EventKind,
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    /// Human sentence, e.g. "Waterfox was terminated at 14:32 after its group reached 13.4 GiB".
    pub message: String,
    /// Machine-readable cause, e.g. `pressure:critical:full-psi=2.4`.
    pub cause: Option<String>,
    pub actions: Vec<EventAction>,
    /// True while an "Undo" action is still valid.
    pub reversible: bool,
}

/// Storage boundary; implemented over `templar-state`.
pub trait HistoryStore: Send + Sync {
    fn append(&self, event: Event) -> anyhow::Result<()>;
    fn recent(&self, limit: usize) -> anyhow::Result<Vec<Event>>;
    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Event>>;
}

/// In-memory store for tests and observe-only runs.
#[derive(Debug, Default)]
pub struct MemoryHistory(std::sync::Mutex<Vec<Event>>);

impl HistoryStore for MemoryHistory {
    fn append(&self, event: Event) -> anyhow::Result<()> {
        self.0.lock().expect("history poisoned").push(event);
        Ok(())
    }

    fn recent(&self, limit: usize) -> anyhow::Result<Vec<Event>> {
        let events = self.0.lock().expect("history poisoned");
        Ok(events.iter().rev().take(limit).cloned().collect())
    }

    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Event>> {
        let needle = query.to_lowercase();
        let events = self.0.lock().expect("history poisoned");
        Ok(events
            .iter()
            .rev()
            .filter(|event| event.message.to_lowercase().contains(&needle))
            .take(limit)
            .cloned()
            .collect())
    }
}

/// Insert one event as part of an already-open transaction. Used by [`SqliteHistory::append`]
/// and by other domains (associations, settings, providers) that must persist their own row
/// *and* this history row as one commit.
pub fn append_within(tx: &Transaction<'_>, event: &Event) -> Result<()> {
    tx.execute(
        "INSERT INTO history
            (event_id, at_ms, kind, app_id, instance_id, message, cause, actions, reversible)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            event.id.to_string(),
            event.at_ms,
            event.kind.as_str(),
            event.app_id.as_ref().map(AppId::as_str),
            event.instance_id.as_ref().map(InstanceId::to_string),
            event.message,
            event.cause,
            serde_json::to_string(&event.actions).context("serializing event actions")?,
            event.reversible,
        ],
    )
    .context("inserting history event")?;
    Ok(())
}

/// Look an event up by ID (used by `History1.Undo`).
pub fn find_within(tx: &Transaction<'_>, id: EventId) -> Result<Option<Event>> {
    tx.query_row(
        "SELECT event_id, at_ms, kind, app_id, instance_id, message, cause, actions, reversible
         FROM history WHERE event_id = ?1",
        params![id.to_string()],
        row_to_event,
    )
    .optional()
    .context("looking up history event")
}

/// Mark an event no longer reversible (its one undo action was used).
pub fn mark_not_reversible_within(tx: &Transaction<'_>, id: EventId) -> Result<()> {
    tx.execute(
        "UPDATE history SET reversible = 0 WHERE event_id = ?1",
        params![id.to_string()],
    )
    .context("marking history event as no longer reversible")?;
    Ok(())
}

fn row_to_event(row: &Row<'_>) -> rusqlite::Result<Event> {
    let event_id: String = row.get(0)?;
    let kind: String = row.get(2)?;
    let app_id: Option<String> = row.get(3)?;
    let instance_id: Option<String> = row.get(4)?;
    let actions: String = row.get(7)?;
    Ok(Event {
        id: event_id
            .parse()
            .map_err(|_| invalid_column(0, "event_id is not a UUID"))?,
        at_ms: row.get(1)?,
        kind: EventKind::parse(&kind)
            .ok_or_else(|| invalid_column(2, "unknown history event kind"))?,
        app_id: app_id
            .map(|value| value.parse::<AppId>())
            .transpose()
            .map_err(|_| invalid_column(3, "app_id is not a valid AppId"))?,
        instance_id: instance_id
            .map(|value| InstanceId::parse(&value).ok_or(()))
            .transpose()
            .map_err(|()| invalid_column(4, "instance_id is not a UUID"))?,
        message: row.get(5)?,
        cause: row.get(6)?,
        actions: serde_json::from_str(&actions)
            .map_err(|_| invalid_column(7, "actions is not valid JSON"))?,
        reversible: row.get(8)?,
    })
}

fn invalid_column(index: usize, message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(message)),
    )
}

/// The persisted [`HistoryStore`]: one row per event in `templar-state`'s `history` table.
#[derive(Clone, Debug)]
pub struct SqliteHistory {
    store: templar_state::Store,
}

impl SqliteHistory {
    pub fn new(store: templar_state::Store) -> Self {
        Self { store }
    }
}

impl HistoryStore for SqliteHistory {
    fn append(&self, event: Event) -> Result<()> {
        self.store.transaction(|tx| append_within(tx, &event))
    }

    fn recent(&self, limit: usize) -> Result<Vec<Event>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare(
            "SELECT event_id, at_ms, kind, app_id, instance_id, message, cause, actions, reversible
             FROM history ORDER BY at_ms DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading recent history events")
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Event>> {
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let connection = self.store.lock();
        let mut statement = connection.prepare(
            "SELECT event_id, at_ms, kind, app_id, instance_id, message, cause, actions, reversible
             FROM history WHERE message LIKE ?1 ESCAPE '\\' ORDER BY at_ms DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![pattern, limit as i64], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("searching history events")
    }
}

#[cfg(test)]
mod sqlite_tests {
    use super::*;

    fn event(message: &str) -> Event {
        Event {
            id: EventId::new(),
            at_ms: templar_state::now_ms(),
            kind: EventKind::AssociationChange,
            app_id: Some("org.templaros.Viewer".parse().unwrap()),
            instance_id: None,
            message: message.to_owned(),
            cause: Some("test".into()),
            actions: vec![EventAction {
                label: "Undo".into(),
                command: "undo:1".into(),
            }],
            reversible: true,
        }
    }

    #[test]
    fn round_trips_through_sqlite() {
        let store = templar_state::Store::in_memory().unwrap();
        let history = SqliteHistory::new(store);
        history
            .append(event("PDF default changed to Viewer"))
            .unwrap();
        history
            .append(event("PDF default changed to Browser"))
            .unwrap();

        let recent = history.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].message, "PDF default changed to Browser");
        assert!(recent[0].reversible);
        assert_eq!(recent[0].actions[0].command, "undo:1");

        let found = history.search("Browser", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message, "PDF default changed to Browser");
    }

    #[test]
    fn append_within_shares_a_transaction_with_another_domain_write() {
        let store = templar_state::Store::in_memory().unwrap();
        let event = event("combined write");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO mime_defaults (mime, app_id) VALUES ('application/pdf', 'org.templaros.Viewer')",
                    [],
                )?;
                append_within(tx, &event)
            })
            .unwrap();

        let history = SqliteHistory::new(store.clone());
        assert_eq!(history.recent(10).unwrap().len(), 1);
        let mimes: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM mime_defaults", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mimes, 1);
    }

    #[test]
    fn find_and_mark_not_reversible() {
        let store = templar_state::Store::in_memory().unwrap();
        let history = SqliteHistory::new(store.clone());
        let event = event("undo me");
        let id = event.id;
        history.append(event).unwrap();

        store
            .transaction(|tx| {
                assert!(find_within(tx, id).unwrap().is_some());
                mark_not_reversible_within(tx, id)
            })
            .unwrap();

        let recent = history.recent(1).unwrap();
        assert!(!recent[0].reversible);
    }
}
