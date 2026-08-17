//! One state store per user: `$XDG_STATE_HOME/legeos/session.db`.
//!
//! Identity, associations, providers, autostart decisions, history, projects and file-operation
//! journals live in one SQLite database so cross-domain updates are transactions ("change the
//! default PDF handler *and* record the history entry" is one commit). Settings *values* stay in
//! text (TOML) so they are diffable; only their change history lives here.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Ordered schema migrations. Append-only; never edit a shipped entry.
pub const MIGRATIONS: &[(&str, &str)] = &[(
    "0001-initial",
    r"
        CREATE TABLE instances (
            instance_id   TEXT PRIMARY KEY,
            app_id        TEXT NOT NULL,
            systemd_unit  TEXT NOT NULL,
            control_group TEXT NOT NULL,
            started_ms    INTEGER NOT NULL
        );
        CREATE TABLE mime_defaults (
            mime   TEXT PRIMARY KEY,
            app_id TEXT NOT NULL
        );
        CREATE TABLE extension_defaults (
            extension              TEXT PRIMARY KEY,
            app_id                 TEXT NOT NULL,
            override_specific_mime INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE project_mime_defaults (
            project_id TEXT NOT NULL,
            mime       TEXT NOT NULL,
            app_id     TEXT NOT NULL,
            PRIMARY KEY (project_id, mime)
        );
        CREATE TABLE provider_selections (
            role   TEXT PRIMARY KEY,
            app_id TEXT NOT NULL
        );
        CREATE TABLE autostart_approvals (
            app_id   TEXT PRIMARY KEY,
            approval TEXT NOT NULL
        );
        CREATE TABLE history (
            event_id    TEXT PRIMARY KEY,
            at_ms       INTEGER NOT NULL,
            kind        TEXT NOT NULL,
            app_id      TEXT,
            instance_id TEXT,
            message     TEXT NOT NULL,
            cause       TEXT,
            actions     TEXT NOT NULL,
            reversible  INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX history_at ON history (at_ms DESC);
        CREATE TABLE projects (
            project_id TEXT PRIMARY KEY,
            json       TEXT NOT NULL
        );
        CREATE TABLE settings_changes (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            at_ms    INTEGER NOT NULL,
            key      TEXT NOT NULL,
            old_json TEXT,
            new_json TEXT NOT NULL,
            origin   TEXT NOT NULL
        );
        CREATE TABLE last_directories (
            scope     TEXT NOT NULL,
            key       TEXT NOT NULL,
            directory TEXT NOT NULL,
            at_ms     INTEGER NOT NULL,
            PRIMARY KEY (scope, key)
        );
        CREATE TABLE file_operations (
            op_id      TEXT PRIMARY KEY,
            kind       TEXT NOT NULL,
            state      TEXT NOT NULL,
            journal    TEXT NOT NULL,
            started_ms INTEGER NOT NULL,
            ended_ms   INTEGER
        );
        ",
)];

/// Default database path for the current user.
pub fn default_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    state_home.join("legeos").join("session.db")
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
    migrate(&connection)?;
    Ok(connection)
}

/// Apply every migration not yet recorded in `schema_migrations`.
pub fn migrate(connection: &Connection) -> Result<usize> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_ms INTEGER NOT NULL);",
    )?;
    let mut applied = 0;
    for (name, sql) in MIGRATIONS {
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
}
