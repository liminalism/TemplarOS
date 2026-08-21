//! The shared undo log every domain that offers "Undo" writes to.
//!
//! `History1.Undo(event_id)` does not know how to reverse a MIME default, a setting or a
//! provider selection — each domain does. An event's undo [`crate::history::EventAction`]
//! names a `change_log` row (`undo:<id>`); the row's `domain` field tells the dispatcher which
//! applier to call, and `old_value`/`new_value` (opaque JSON to this module) carry what to
//! restore. Recording a change and appending its history event happen in the same
//! [`lege_state::Store::transaction`] call as the domain's own row write, so an undo action
//! never outlives the change it describes.

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::history::EventId;

/// One row in `change_log`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeLogEntry {
    pub id: i64,
    pub at_ms: i64,
    pub domain: String,
    pub key: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub event_id: EventId,
    pub undone: bool,
}

/// Record one reversible change; returns its `change_log.id` (used to build the `undo:<id>`
/// command on the event's [`crate::history::EventAction`]).
pub fn record_within(
    tx: &Transaction<'_>,
    domain: &str,
    key: &str,
    old_value: Option<&str>,
    new_value: Option<&str>,
    event_id: EventId,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO change_log (at_ms, domain, key, old_value, new_value, event_id, undone)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
        params![
            lege_state::now_ms(),
            domain,
            key,
            old_value,
            new_value,
            event_id.to_string(),
        ],
    )
    .context("recording change_log entry")?;
    Ok(tx.last_insert_rowid())
}

/// Fetch one entry (used by the undo dispatcher).
pub fn find_within(tx: &Transaction<'_>, id: i64) -> Result<Option<ChangeLogEntry>> {
    tx.query_row(
        "SELECT id, at_ms, domain, key, old_value, new_value, event_id, undone
         FROM change_log WHERE id = ?1",
        params![id],
        |row| {
            let event_id: String = row.get(6)?;
            Ok(ChangeLogEntry {
                id: row.get(0)?,
                at_ms: row.get(1)?,
                domain: row.get(2)?,
                key: row.get(3)?,
                old_value: row.get(4)?,
                new_value: row.get(5)?,
                event_id: event_id.parse().unwrap_or_default(),
                undone: row.get(7)?,
            })
        },
    )
    .optional()
    .context("looking up change_log entry")
}

/// Mark an entry as undone so it cannot be replayed twice.
pub fn mark_undone_within(tx: &Transaction<'_>, id: i64) -> Result<()> {
    tx.execute(
        "UPDATE change_log SET undone = 1 WHERE id = ?1",
        params![id],
    )
    .context("marking change_log entry undone")?;
    Ok(())
}

/// The command an [`crate::history::EventAction`] uses to name a change_log row.
pub fn undo_command(id: i64) -> String {
    format!("undo:{id}")
}

/// Parse an `undo:<id>` command back into a `change_log.id`.
pub fn parse_undo_command(command: &str) -> Option<i64> {
    command.strip_prefix("undo:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{Event, EventAction, EventKind, append_within};

    #[test]
    fn record_find_and_mark_undone_round_trip() {
        let store = lege_state::Store::in_memory().unwrap();
        let event_id = EventId::new();
        let change_id = store
            .transaction(|tx| {
                let id = record_within(
                    tx,
                    "associations.mime",
                    "application/pdf",
                    Some("\"org.legeos.Viewer\""),
                    Some("\"org.example.Browser\""),
                    event_id,
                )?;
                append_within(
                    tx,
                    &Event {
                        id: event_id,
                        at_ms: lege_state::now_ms(),
                        kind: EventKind::AssociationChange,
                        app_id: None,
                        instance_id: None,
                        message: "changed default".into(),
                        cause: None,
                        actions: vec![EventAction {
                            label: "Undo".into(),
                            command: undo_command(id),
                        }],
                        reversible: true,
                    },
                )?;
                Ok(id)
            })
            .unwrap();

        assert_eq!(
            parse_undo_command(&undo_command(change_id)),
            Some(change_id)
        );

        store
            .transaction(|tx| {
                let entry = find_within(tx, change_id)?.expect("entry recorded");
                assert_eq!(entry.domain, "associations.mime");
                assert_eq!(entry.old_value.as_deref(), Some("\"org.legeos.Viewer\""));
                assert!(!entry.undone);
                mark_undone_within(tx, change_id)
            })
            .unwrap();

        store
            .transaction(|tx| {
                let entry = find_within(tx, change_id)?.expect("entry still present");
                assert!(entry.undone);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn unknown_command_does_not_parse() {
        assert_eq!(parse_undo_command("notify:action:default"), None);
        assert_eq!(parse_undo_command("undo:abc"), None);
    }
}
