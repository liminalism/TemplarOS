//! One state store per user: `$XDG_STATE_HOME/legeos/session.db`.
//!
//! Identity, associations, providers, autostart decisions, history, projects and file-operation
//! journals live in one SQLite database so cross-domain updates are transactions ("change the
//! default PDF handler *and* record the history entry" is one commit). Settings *values* stay in
//! text (TOML) so they are diffable; only their change history lives here.
//!
//! [`Store`] is the handle every domain repository takes: a connection behind a mutex, cheap to
//! clone, `Send + Sync` so it can live inside a zbus interface. `lege-resourced` deliberately
//! does **not** link this crate — the governor must never block on SQLite or fsync (ADR-0002).

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub mod migrations;

/// Ordered schema migrations. Append-only; never edit a shipped entry.
pub fn all_migrations() -> Vec<(&'static str, &'static str)> {
    migrations::all()
}

/// Default database path for the current user.
pub fn default_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    state_home.join("legeos").join("session.db")
}

/// Milliseconds since the Unix epoch, the time unit every `*_ms` column uses.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Open (creating if needed) and migrate the store at `path`.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&connection)?;
    Ok(connection)
}

/// In-memory store (tests, dry runs).
pub fn open_in_memory() -> Result<Connection> {
    let connection = Connection::open_in_memory()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&connection)?;
    Ok(connection)
}

/// Apply every migration not yet recorded in `schema_migrations`.
pub fn migrate(connection: &Connection) -> Result<usize> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_ms INTEGER NOT NULL);",
    )?;
    let mut applied = 0;
    for (name, sql) in all_migrations() {
        let done: bool = connection.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
            [name],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )?;
        if done {
            continue;
        }
        tracing::info!(migration = name, "applying");
        connection.execute_batch("BEGIN;")?;
        if let Err(error) = connection.execute_batch(sql) {
            let _ = connection.execute_batch("ROLLBACK;");
            return Err(error).with_context(|| format!("migration {name}"));
        }
        connection.execute(
            "INSERT INTO schema_migrations (name, applied_ms) VALUES (?1, strftime('%s','now') * 1000)",
            [name],
        )?;
        connection.execute_batch("COMMIT;")?;
        applied += 1;
    }
    Ok(applied)
}

/// A migrated connection shared by every domain repository in a daemon.
///
/// `rusqlite::Connection` is `Send` but not `Sync`; one mutex around it keeps the whole store
/// `Send + Sync` and makes cross-domain writes ("set the default handler *and* append the
/// history event") one transaction by construction. Contention is irrelevant at desktop rates,
/// and the alternative — a connection per domain — loses exactly that property.
#[derive(Clone, Debug)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (creating and migrating if needed) the store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self::from_connection(open(path)?))
    }

    /// A migrated in-memory store, for tests and `--dry-run`.
    pub fn in_memory() -> Result<Self> {
        Ok(Self::from_connection(open_in_memory()?))
    }

    /// Wrap an already-migrated connection.
    pub fn from_connection(connection: Connection) -> Self {
        Self {
            connection: Arc::new(Mutex::new(connection)),
        }
    }

    /// Borrow the connection. Hold the guard only for the length of one statement or
    /// transaction — never across an `.await`.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock()
    }

    /// Run `body` inside one transaction, committing on `Ok` and rolling back on `Err`.
    pub fn transaction<T>(
        &self,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let value = body(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_are_idempotent() {
        let connection = open_in_memory().unwrap();
        assert_eq!(migrate(&connection).unwrap(), 0);
        let tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='history'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn migration_names_are_unique_and_ordered() {
        let names: Vec<&str> = all_migrations().iter().map(|(name, _)| *name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate migration name");
        assert_eq!(names.first(), Some(&"0001-initial"));
    }

    #[test]
    fn transactions_roll_back_on_error() {
        let store = Store::in_memory().unwrap();
        let result: Result<()> = store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO mime_defaults (mime, app_id) VALUES ('application/pdf', 'org.legeos.Viewer')",
                [],
            )?;
            anyhow::bail!("deliberate failure")
        });
        assert!(result.is_err());
        let rows: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM mime_defaults", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
}
