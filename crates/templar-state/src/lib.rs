//! One state store per user: `$XDG_STATE_HOME/templaros/session.db`.
//!
//! Identity, associations, providers, autostart decisions, history, projects and file-operation
//! journals live in one SQLite database so cross-domain updates are transactions ("change the
//! default PDF handler *and* record the history entry" is one commit). Settings *values* stay in
//! text (TOML) so they are diffable; only their change history lives here.
//!
//! [`Store`] is the handle every domain repository takes: a connection behind a mutex, cheap to
//! clone, `Send + Sync` so it can live inside a zbus interface. `templar-resourced` deliberately
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

pub use migrations::Migration;

/// Ordered schema migrations. Append-only; never edit a shipped entry.
#[must_use]
pub fn all_migrations() -> Vec<Migration> {
    migrations::all()
}

/// The schema version this build understands: how many migrations it ships.
///
/// A plain count rather than a hand-maintained constant, so it cannot drift from the list. It is
/// what a database's recorded reader floor is compared against.
#[must_use]
pub fn schema_version() -> u32 {
    u32::try_from(all_migrations().len()).unwrap_or(u32::MAX)
}

/// The oldest schema version that can still read a fully migrated database.
///
/// The highest floor any applied migration raises; `0` while every migration is additive, which
/// is the state today and the state the policy tests keep it in for as long as possible.
#[must_use]
pub fn min_reader_version() -> u32 {
    all_migrations()
        .iter()
        .map(|migration| migration.min_reader_version)
        .max()
        .unwrap_or(0)
}

/// Default database path for the current user.
pub fn default_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    state_home.join("templaros").join("session.db")
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
    // One store per user, opened by more than one daemon: templar-sessiond and templar-filed are
    // separate crash units (ADR-0001, ADR-0002) over the same file, and systemd starts them at
    // once. Without a busy timeout the loser of any write overlap gets SQLITE_BUSY immediately
    // and fails to start, which on a first login is the whole session. Installed before the
    // pragmas: switching a fresh database to WAL takes an exclusive lock, and that is the first
    // overlap two daemons opening one new file ever have.
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .context("setting the busy timeout")?;
    retry_while_locked(|| connection.pragma_update(None, "journal_mode", "WAL"))
        .context("switching to WAL")?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .context("setting synchronous=NORMAL")?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .context("enabling foreign keys")?;
    refuse_if_too_new(&connection, &path.display().to_string())?;
    migrate(&connection)?;
    Ok(connection)
}

/// Retry `op` while SQLite reports the database busy or locked, for up to five seconds.
///
/// The busy handler does not cover this case: switching a fresh database's journal mode needs an
/// exclusive lock, and when another connection holds a shared lock on the same file SQLite
/// returns SQLITE_BUSY at once instead of waiting — which is exactly what two daemons opening one
/// new store do on a first login. Each retry asks again, so once the winner has switched the file
/// to WAL the losers find it already there and return.
fn retry_while_locked<T>(mut op: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match op() {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

/// Refuse a database a newer build migrated past this one's reach.
///
/// This is the case a rollback creates: the previous binaries are reinstalled against a database
/// the newer ones already migrated. Opening it anyway would have SQLite happily run queries
/// against a schema that no longer means what this build thinks it means, which is how a rollback
/// turns a broken shell into lost data. Refusing costs a session; the alternative does not stop
/// at a session.
fn refuse_if_too_new(connection: &Connection, path: &str) -> Result<()> {
    let Some(required) = read_meta(connection, "min_reader_version") else {
        // No `schema_meta` at all: either a fresh database or one from before this bookkeeping
        // existed, and in both cases there is nothing here a build of this generation cannot read.
        return Ok(());
    };
    let ours = schema_version();
    if required <= ours {
        return Ok(());
    }
    let written_by = read_meta(connection, "schema_version").unwrap_or(required);
    anyhow::bail!(
        "{path} was migrated by a newer TemplarOS (schema version {written_by}) and needs a reader \
         that understands at least schema version {required}; this build understands {ours}. \
         Nothing has been changed. Install the newer build set again — `templar dev status` says \
         which one — or restore the snapshot taken before that upgrade."
    )
}

/// One integer out of `schema_meta`, or `None` if the table or key is not there.
fn read_meta(connection: &Connection, key: &str) -> Option<u32> {
    connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, i64>(0),
        )
        .ok()
        .and_then(|value| u32::try_from(value).ok())
}

/// Copies a consistent snapshot of the database to `path` — `VACUUM INTO`, which SQLite performs
/// as one read transaction, so it is safe against a live connection and needs no separate lock.
///
/// This is the other half of the backward-readability policy: a migration that raises the reader
/// floor makes a rollback past it impossible, so the upgrade that carries one has to leave
/// something behind to roll back *to*.
///
/// # Errors
/// If `path` already exists, or the copy fails.
pub fn snapshot_to(connection: &Connection, path: &Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!(
            "{} already exists; refusing to overwrite a snapshot",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    connection
        .execute("VACUUM INTO ?1", [path.display().to_string()])
        .with_context(|| format!("snapshotting the state store to {}", path.display()))?;
    Ok(())
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
        "CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
    )?;
    let mut applied = 0;
    for migration in all_migrations() {
        let name = migration.name;
        let done: bool = connection.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
            [name],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )?;
        if done {
            continue;
        }
        tracing::info!(migration = name, "applying");
        // IMMEDIATE, and the check repeated inside it: the read above holds no lock, so two
        // daemons opening a fresh database both saw this migration as unapplied and both went on
        // to run it. The loser reached `CREATE TABLE instances` after the winner had committed
        // one and died with "table instances already exists" — a first login where whichever of
        // templar-sessiond and templar-filed started second did not start at all.
        connection
            .execute_batch("BEGIN IMMEDIATE;")
            .with_context(|| format!("beginning the transaction for migration {name}"))?;
        let applied_meanwhile: bool = connection.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE name = ?1",
            [name],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )?;
        if applied_meanwhile {
            connection.execute_batch("ROLLBACK;")?;
            continue;
        }
        if let Err(error) = connection.execute_batch(migration.sql) {
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
    write_meta(connection)?;
    Ok(applied)
}

/// Records what this build made of the database, so the next build to open it can tell whether it
/// is allowed to.
///
/// Written on every `migrate`, not only when something was applied: a database created before
/// this bookkeeping existed gets its `schema_meta` filled in the first time a build that has this
/// code opens it, rather than staying silent until the next migration happens to land.
fn write_meta(connection: &Connection) -> Result<()> {
    for (key, value) in [
        ("schema_version", schema_version()),
        ("min_reader_version", min_reader_version()),
    ] {
        connection.execute(
            "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, i64::from(value)],
        )?;
    }
    Ok(())
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

    /// Copy a consistent snapshot of this store to `path`.
    ///
    /// # Errors
    /// If `path` already exists, or the copy fails.
    pub fn snapshot_to(&self, path: &Path) -> Result<()> {
        snapshot_to(&self.lock(), path)
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

    /// Two daemons open one store at once, which is what a first login does.
    ///
    /// `templar-sessiond` and `templar-filed` are separate units over the same `session.db`, started
    /// together. Before `BEGIN IMMEDIATE`, both read the migration table as empty and both ran
    /// `0001-initial`; the loser died on `CREATE TABLE instances`, so whichever started second
    /// did not start at all. Only the first login was affected, which is the worst one to lose.
    #[test]
    fn a_fresh_store_survives_several_daemons_opening_it_at_once() {
        let dir = std::env::temp_dir().join(format!(
            "templar-state-race-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("templaros").join("session.db");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let openers: Vec<_> = (0..4)
            .map(|_| {
                let (path, barrier) = (path.clone(), std::sync::Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    open(&path).map(|_| ())
                })
            })
            .collect();
        let failures: Vec<String> = openers
            .into_iter()
            .filter_map(|opener| opener.join().expect("opener panicked").err())
            .map(|error| format!("{error:#}"))
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            failures.is_empty(),
            "opening one fresh store from four threads failed: {failures:?}"
        );
    }

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
        let names: Vec<&str> = all_migrations().iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate migration name");
        assert_eq!(names.first(), Some(&"0001-initial"));
    }

    #[test]
    fn the_schema_stays_readable_by_the_previous_generation() {
        // The one-generation rollback rule, as a build-time assertion rather than a convention.
        // A floor equal to the current version would mean the newest schema can only be read by
        // the newest binary — which is exactly when a rollback is most likely to be needed.
        assert!(
            min_reader_version() < schema_version(),
            "the reader floor is {} against schema version {}: rolling back one build set would \
             leave the previous binaries unable to open the database",
            min_reader_version(),
            schema_version()
        );
    }

    #[test]
    fn a_migration_that_raises_the_floor_names_its_own_position() {
        // A breaking migration may only demand a reader that has reached *it* — never some later
        // version it has no way to know about, and never an earlier one, which would be a floor
        // that does not actually protect the change it belongs to.
        for (index, migration) in all_migrations().iter().enumerate() {
            let position = u32::try_from(index + 1).unwrap();
            assert!(
                migration.min_reader_version == 0 || migration.min_reader_version == position,
                "{} declares min_reader_version {} but sits at position {position}",
                migration.name,
                migration.min_reader_version
            );
        }
    }

    #[test]
    fn declared_reader_floors_never_go_backwards() {
        let mut highest = 0;
        for migration in all_migrations() {
            assert!(
                migration.min_reader_version == 0 || migration.min_reader_version >= highest,
                "{} lowers the reader floor",
                migration.name
            );
            highest = highest.max(migration.min_reader_version);
        }
    }

    #[test]
    fn migrating_records_what_this_build_made_of_the_database() {
        let connection = open_in_memory().unwrap();
        assert_eq!(
            read_meta(&connection, "schema_version"),
            Some(schema_version())
        );
        assert_eq!(
            read_meta(&connection, "min_reader_version"),
            Some(min_reader_version())
        );
    }

    #[test]
    fn a_database_that_needs_a_newer_reader_is_refused_untouched() {
        // What a rollback looks like from the old binary's side. Opening it anyway would run
        // queries against a schema that no longer means what this build thinks it means.
        let connection = open_in_memory().unwrap();
        connection
            .execute(
                "UPDATE schema_meta SET value = ?1 WHERE key = 'min_reader_version'",
                [i64::from(schema_version() + 5)],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                [i64::from(schema_version() + 5)],
            )
            .unwrap();
        let error = refuse_if_too_new(&connection, "session.db").unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("newer TemplarOS"), "{text}");
        assert!(text.contains("Nothing has been changed"), "{text}");
        assert!(
            text.contains("templar dev status") && text.contains("snapshot"),
            "the refusal must say how to get out of it: {text}"
        );
    }

    #[test]
    fn a_database_from_before_this_bookkeeping_opens_normally() {
        let connection = open_in_memory().unwrap();
        connection.execute_batch("DROP TABLE schema_meta;").unwrap();
        refuse_if_too_new(&connection, "session.db").expect("an older database is readable");
    }

    #[test]
    fn a_snapshot_is_a_complete_readable_copy() {
        let store = Store::in_memory().unwrap();
        store
            .lock()
            .execute(
                "INSERT INTO mime_defaults (mime, app_id) VALUES ('application/pdf', 'org.templaros.Viewer')",
                [],
            )
            .unwrap();

        let dir =
            std::env::temp_dir().join(format!("templar-state-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("before-upgrade.db");
        store.snapshot_to(&path).unwrap();

        let restored = Store::open(&path).unwrap();
        let app: String = restored
            .lock()
            .query_row(
                "SELECT app_id FROM mime_defaults WHERE mime = 'application/pdf'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(app, "org.templaros.Viewer");

        // A snapshot that silently overwrote the previous one would destroy the very thing it
        // exists to preserve.
        assert!(store.snapshot_to(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transactions_roll_back_on_error() {
        let store = Store::in_memory().unwrap();
        let result: Result<()> = store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO mime_defaults (mime, app_id) VALUES ('application/pdf', 'org.templaros.Viewer')",
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
