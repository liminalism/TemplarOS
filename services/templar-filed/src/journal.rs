//! The durable operation journal.
//!
//! The reason `filed` exists rather than another file-manager dialog: a long copy or a
//! cross-volume move survives the crash of whatever started it. Every state change is written
//! here before it is attempted, so a restart can tell the difference between "this operation
//! finished", "this operation was half-way through step 41" and "this operation never began" —
//! and can resume or reverse accordingly.

use crate::operations::{Operation, OperationId, OperationKind};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use templar_state::Store;
use templar_types::AppId;

/// Where journalled operations live.
pub trait Journal: Send + Sync + std::fmt::Debug {
    /// Record a newly planned operation, or overwrite the record of one already known.
    fn put(&self, operation: &Operation, app_id: Option<&AppId>) -> Result<()>;

    /// Update the mutable part: state, cursor, progress, error.
    fn progress(&self, operation: &Operation) -> Result<()>;

    fn get(&self, id: OperationId) -> Result<Option<Operation>>;

    /// Every operation, newest first.
    fn recent(&self, limit: usize) -> Result<Vec<Operation>>;

    /// Operations that were running when the service stopped — the resume candidates.
    fn unfinished(&self) -> Result<Vec<Operation>>;

    /// Record what would undo a committed operation.
    fn put_undo(&self, id: OperationId, undo: &OperationKind) -> Result<()>;

    fn undo_of(&self, id: OperationId) -> Result<Option<OperationKind>>;

    fn clear_undo(&self, id: OperationId) -> Result<()>;
}

/// The real journal, in the one per-user state store.
#[derive(Debug)]
pub struct SqliteJournal {
    store: Store,
}

impl SqliteJournal {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

impl Journal for SqliteJournal {
    fn put(&self, operation: &Operation, app_id: Option<&AppId>) -> Result<()> {
        let journal = serde_json::to_string(operation)?;
        self.store
            .lock()
            .execute(
                "INSERT INTO file_operations
                   (op_id, kind, state, journal, started_ms, ended_ms,
                    total_bytes, done_bytes, total_items, done_items, app_id, summary, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (op_id) DO UPDATE SET
                    state = excluded.state, journal = excluded.journal,
                    total_bytes = excluded.total_bytes, done_bytes = excluded.done_bytes,
                    total_items = excluded.total_items, done_items = excluded.done_items,
                    summary = excluded.summary, error = excluded.error",
                rusqlite::params![
                    operation.id.to_string(),
                    kind_name(&operation.kind),
                    operation.state.as_str(),
                    journal,
                    templar_state::now_ms(),
                    operation.total_bytes as i64,
                    operation.done_bytes as i64,
                    operation.total_items as i64,
                    operation.done_items as i64,
                    app_id.map(ToString::to_string),
                    operation.kind.summary(),
                    operation.error.as_deref(),
                ],
            )
            .context("recording a planned operation")?;
        Ok(())
    }

    fn progress(&self, operation: &Operation) -> Result<()> {
        let journal = serde_json::to_string(operation)?;
        let ended = operation.state.terminal().then(templar_state::now_ms);
        self.store
            .lock()
            .execute(
                "UPDATE file_operations
                    SET state = ?2, journal = ?3, done_bytes = ?4, done_items = ?5,
                        error = ?6, ended_ms = COALESCE(?7, ended_ms)
                  WHERE op_id = ?1",
                rusqlite::params![
                    operation.id.to_string(),
                    operation.state.as_str(),
                    journal,
                    operation.done_bytes as i64,
                    operation.done_items as i64,
                    operation.error.as_deref(),
                    ended,
                ],
            )
            .context("recording operation progress")?;
        Ok(())
    }

    fn get(&self, id: OperationId) -> Result<Option<Operation>> {
        let store = self.store.lock();
        let mut statement =
            store.prepare("SELECT journal FROM file_operations WHERE op_id = ?1")?;
        let mut rows = statement.query([id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(serde_json::from_str(&row.get::<_, String>(0)?)?)),
            None => Ok(None),
        }
    }

    fn recent(&self, limit: usize) -> Result<Vec<Operation>> {
        let store = self.store.lock();
        let mut statement = store.prepare(
            "SELECT journal FROM file_operations ORDER BY started_ms DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| row.get::<_, String>(0))?;
        rows.map(|json| Ok(serde_json::from_str(&json?)?)).collect()
    }

    fn unfinished(&self) -> Result<Vec<Operation>> {
        let store = self.store.lock();
        let mut statement = store.prepare(
            "SELECT journal FROM file_operations
              WHERE state IN ('planned', 'running', 'verified')
              ORDER BY started_ms ASC",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|json| Ok(serde_json::from_str(&json?)?)).collect()
    }

    fn put_undo(&self, id: OperationId, undo: &OperationKind) -> Result<()> {
        self.store.lock().execute(
            "INSERT INTO file_undo (op_id, plan, expires_ms) VALUES (?1, ?2, NULL)
             ON CONFLICT (op_id) DO UPDATE SET plan = excluded.plan",
            rusqlite::params![id.to_string(), serde_json::to_string(undo)?],
        )?;
        Ok(())
    }

    fn undo_of(&self, id: OperationId) -> Result<Option<OperationKind>> {
        let store = self.store.lock();
        let mut statement = store.prepare("SELECT plan FROM file_undo WHERE op_id = ?1")?;
        let mut rows = statement.query([id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(serde_json::from_str(&row.get::<_, String>(0)?)?)),
            None => Ok(None),
        }
    }

    fn clear_undo(&self, id: OperationId) -> Result<()> {
        self.store
            .lock()
            .execute("DELETE FROM file_undo WHERE op_id = ?1", [id.to_string()])?;
        Ok(())
    }
}

fn kind_name(kind: &OperationKind) -> &'static str {
    match kind {
        OperationKind::Copy { .. } => "copy",
        OperationKind::Move { .. } => "move",
        OperationKind::RestoreMoveV1 { .. } => "restore-move-v1",
        OperationKind::UndoCopyV1 { .. } => "undo-copy-v1",
        OperationKind::Rename { .. } => "rename",
        OperationKind::Trash { .. } => "trash",
        OperationKind::RestoreFromTrash { .. } => "restore",
        OperationKind::Delete { .. } => "delete",
    }
}

/// An in-memory journal for tests and `--dry-run`. It has the same observable behaviour as the
/// SQLite one, so the executor tests exercise the real code path.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    operations: Mutex<BTreeMap<OperationId, (Operation, u64)>>,
    undo: Mutex<BTreeMap<OperationId, OperationKind>>,
    sequence: Mutex<u64>,
}

impl Journal for MemoryJournal {
    fn put(&self, operation: &Operation, _app_id: Option<&AppId>) -> Result<()> {
        let mut sequence = self.sequence.lock();
        *sequence += 1;
        let order = *sequence;
        drop(sequence);
        self.operations
            .lock()
            .insert(operation.id, (operation.clone(), order));
        Ok(())
    }

    fn progress(&self, operation: &Operation) -> Result<()> {
        let mut operations = self.operations.lock();
        let order = operations.get(&operation.id).map_or(0, |(_, order)| *order);
        operations.insert(operation.id, (operation.clone(), order));
        Ok(())
    }

    fn get(&self, id: OperationId) -> Result<Option<Operation>> {
        Ok(self
            .operations
            .lock()
            .get(&id)
            .map(|(operation, _)| operation.clone()))
    }

    fn recent(&self, limit: usize) -> Result<Vec<Operation>> {
        let operations = self.operations.lock();
        let mut all: Vec<&(Operation, u64)> = operations.values().collect();
        all.sort_by_key(|(_, order)| std::cmp::Reverse(*order));
        Ok(all
            .into_iter()
            .take(limit)
            .map(|(operation, _)| operation.clone())
            .collect())
    }

    fn unfinished(&self) -> Result<Vec<Operation>> {
        let operations = self.operations.lock();
        let mut all: Vec<&(Operation, u64)> = operations
            .values()
            .filter(|(operation, _)| !operation.state.terminal())
            .collect();
        all.sort_by_key(|(_, order)| *order);
        Ok(all
            .into_iter()
            .map(|(operation, _)| operation.clone())
            .collect())
    }

    fn put_undo(&self, id: OperationId, undo: &OperationKind) -> Result<()> {
        self.undo.lock().insert(id, undo.clone());
        Ok(())
    }

    fn undo_of(&self, id: OperationId) -> Result<Option<OperationKind>> {
        Ok(self.undo.lock().get(&id).cloned())
    }

    fn clear_undo(&self, id: OperationId) -> Result<()> {
        self.undo.lock().remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::{OperationState, Options, Survey, plan};
    use std::path::PathBuf;

    fn sample() -> Operation {
        plan(
            OperationId::new(),
            OperationKind::Rename {
                path: PathBuf::from("/a/x.txt"),
                new_name: "y.txt".into(),
            },
            &Survey::default(),
            Options::default(),
        )
    }

    fn round_trip(journal: &dyn Journal) {
        let mut operation = sample();
        journal.put(&operation, None).unwrap();
        assert_eq!(journal.get(operation.id).unwrap().unwrap(), operation);
        assert_eq!(journal.unfinished().unwrap().len(), 1);

        operation.state = OperationState::Committed;
        operation.cursor = 1;
        operation.done_items = 1;
        journal.progress(&operation).unwrap();
        assert_eq!(
            journal.get(operation.id).unwrap().unwrap().state,
            OperationState::Committed
        );
        assert!(journal.unfinished().unwrap().is_empty());
        assert_eq!(journal.recent(10).unwrap().len(), 1);

        let undo = OperationKind::Rename {
            path: PathBuf::from("/a/y.txt"),
            new_name: "x.txt".into(),
        };
        journal.put_undo(operation.id, &undo).unwrap();
        assert_eq!(journal.undo_of(operation.id).unwrap(), Some(undo));
        journal.clear_undo(operation.id).unwrap();
        assert_eq!(journal.undo_of(operation.id).unwrap(), None);
    }

    #[test]
    fn the_sqlite_journal_round_trips() {
        round_trip(&SqliteJournal::new(Store::in_memory().unwrap()));
    }

    #[test]
    fn the_memory_journal_round_trips() {
        round_trip(&MemoryJournal::default());
    }

    #[test]
    fn recent_is_newest_first() {
        let journal = SqliteJournal::new(Store::in_memory().unwrap());
        let first = sample();
        let second = sample();
        journal.put(&first, None).unwrap();
        journal.put(&second, None).unwrap();
        let recent = journal.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, second.id);
    }
}
