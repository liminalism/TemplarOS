//! The derived database's schema, and the one rule that makes it different from the state store.
//!
//! `templar-state` refuses to open a database it does not understand, because losing it loses the
//! user's choices. This database holds nothing but a re-derivable picture of the filesystem, so
//! the rule is inverted: **a schema it cannot understand is rebuilt, never an error**. A deleted
//! cache costs a rescan. That is the whole reason the entries left the session database.

use crate::error::{IndexError, Result};
use rusqlite::Connection;

/// Bumped only when the file layout changes incompatibly. It is in the *filename*
/// (`file-index-v1.sqlite3`), so a future v2 daemon leaves a v1 file alone rather than eating it.
pub const FORMAT_VERSION: u32 = 1;

/// The walker's identity. A changed scanner may see different entries for the same tree, which is
/// worth recording next to the rows it produced.
pub const SCANNER_VERSION: u32 = 1;

/// One schema migration. Append-only: once shipped, the SQL is frozen and a change is a new entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Migration {
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "0001-index-core",
        sql: r"
        CREATE TABLE index_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE index_migrations (
            name       TEXT PRIMARY KEY,
            applied_ms INTEGER NOT NULL
        );

        -- One indexed root: where it is, what it includes, and how much of the truth its
        -- committed generation actually holds.
        CREATE TABLE roots (
            root_id                   INTEGER PRIMARY KEY,
            path_bytes                BLOB NOT NULL UNIQUE,
            display                   TEXT NOT NULL,
            volume_dev_id             INTEGER,
            volume_uuid               TEXT,
            volume_mount              BLOB,
            volume_label              TEXT,
            cross_mount               INTEGER NOT NULL DEFAULT 0,
            policy_json               TEXT NOT NULL,
            policy_hash               TEXT NOT NULL,
            state                     TEXT NOT NULL,
            state_reason              TEXT,
            committed_generation      INTEGER NOT NULL DEFAULT 0,
            building_generation       INTEGER,
            complete                  INTEGER NOT NULL DEFAULT 0,
            entry_count               INTEGER NOT NULL DEFAULT 0,
            last_scan_ms              INTEGER,
            last_reconcile_started_ms INTEGER,
            last_reconcile_finished_ms INTEGER,
            last_error                TEXT,
            watch_json                TEXT
        );

        -- One directory entry. path_bytes is authority; path_display is what a UI draws and what
        -- the search accelerator tokenizes. deleted_generation is a tombstone, not a delete, so a
        -- cursor pinned to an older generation can still resolve the row it listed.
        CREATE TABLE entries (
            entry_id             INTEGER PRIMARY KEY,
            root_id              INTEGER NOT NULL REFERENCES roots (root_id) ON DELETE CASCADE,
            parent_id            INTEGER,
            name_bytes           BLOB NOT NULL,
            name_display         TEXT NOT NULL,
            name_fold            TEXT NOT NULL,
            path_bytes           BLOB NOT NULL,
            path_display         TEXT NOT NULL,
            kind                 INTEGER NOT NULL,
            size_bytes           INTEGER NOT NULL,
            alloc_bytes          INTEGER NOT NULL,
            mtime_ns             INTEGER NOT NULL,
            ctime_ns             INTEGER NOT NULL,
            mode                 INTEGER NOT NULL,
            hidden               INTEGER NOT NULL,
            dev                  INTEGER,
            inode                INTEGER,
            nlink                INTEGER NOT NULL DEFAULT 1,
            mime                 TEXT,
            written_generation   INTEGER NOT NULL,
            last_seen_generation INTEGER NOT NULL,
            deleted_generation   INTEGER
        );
        CREATE UNIQUE INDEX entries_path ON entries (root_id, path_bytes);
        CREATE INDEX entries_name_fold ON entries (root_id, name_fold);
        CREATE INDEX entries_object ON entries (root_id, dev, inode);
        CREATE INDEX entries_mtime ON entries (mtime_ns DESC);
        CREATE INDEX entries_seen ON entries (root_id, last_seen_generation);
        CREATE INDEX entries_parent ON entries (parent_id);

        -- Where a scan in flight puts its rows. Invisible to every reader, which is what makes a
        -- killed rebuild cost nothing: the committed generation was never touched.
        CREATE TABLE entries_staging (
            staging_id  INTEGER PRIMARY KEY,
            generation  INTEGER NOT NULL,
            root_id     INTEGER NOT NULL,
            name_bytes  BLOB NOT NULL,
            name_display TEXT NOT NULL,
            name_fold   TEXT NOT NULL,
            path_bytes  BLOB NOT NULL,
            path_display TEXT NOT NULL,
            parent_bytes BLOB,
            kind        INTEGER NOT NULL,
            size_bytes  INTEGER NOT NULL,
            alloc_bytes INTEGER NOT NULL,
            mtime_ns    INTEGER NOT NULL,
            ctime_ns    INTEGER NOT NULL,
            mode        INTEGER NOT NULL,
            hidden      INTEGER NOT NULL,
            dev         INTEGER,
            inode       INTEGER,
            nlink       INTEGER NOT NULL DEFAULT 1,
            mime        TEXT
        );
        CREATE UNIQUE INDEX entries_staging_path ON entries_staging (root_id, path_bytes);

        -- The bounded change log. Pruned oldest-first; the floor is in index_meta, and asking
        -- below it is answered with SnapshotRequired rather than invented continuity.
        CREATE TABLE changes (
            seq            INTEGER PRIMARY KEY AUTOINCREMENT,
            generation     INTEGER NOT NULL,
            root_id        INTEGER NOT NULL,
            op             TEXT NOT NULL,
            entry_id       INTEGER,
            path_bytes     BLOB NOT NULL,
            old_path_bytes BLOB,
            kind           INTEGER NOT NULL,
            size_bytes     INTEGER NOT NULL,
            mtime_ns       INTEGER NOT NULL,
            at_ms          INTEGER NOT NULL
        );
        CREATE INDEX changes_generation ON changes (generation);
        ",
    },
    Migration {
        name: "0002-coverage-tiers",
        // `DEFAULT 3` is `Coverage::Detailed`, which is exactly what survived the old
        // all-or-nothing `is_skipped`: every historical row is a row the skip list kept, and a row
        // the skip list kept was searchable. So the backfill is free, no rescan is forced, and
        // `entries_fts` needs no repair — the accelerator already holds precisely these rows.
        sql: r"
        ALTER TABLE entries ADD COLUMN coverage INTEGER NOT NULL DEFAULT 3;
        ALTER TABLE entries_staging ADD COLUMN coverage INTEGER NOT NULL DEFAULT 3;
        ",
    },
    Migration {
        name: "0003-metadata-freshness",
        // `'verified'` is what the rows in an existing database actually are: every pass before
        // this column existed either stat'd its children or was a policy-change reconcile, and the
        // former is overwhelmingly the common case. Defaulting to `'unverified'` would put every
        // root on the machine into a repair queue on first launch to relabel what is already true.
        sql: r"
        ALTER TABLE roots ADD COLUMN metadata_freshness TEXT NOT NULL DEFAULT 'verified';
        ",
    },
    Migration {
        name: "0004-generation-lookup",
        // `refresh_fts` selects a generation's rows out of `entries`, and no index covered
        // `written_generation`, so every batch commit scanned the whole root: measured at 28.5 ms
        // of a 28.7 ms one-mutation batch over 153,327 entries. The tombstone half is a partial
        // index because `deleted_generation` is NULL for very nearly every row — the index holds
        // only what is actually tombstoned, so the write cost on the hot insert path is nil.
        sql: r"
        CREATE INDEX entries_written ON entries (root_id, written_generation);
        CREATE INDEX entries_tombstoned ON entries (root_id, deleted_generation)
            WHERE deleted_generation IS NOT NULL;
        ",
    },
    Migration {
        name: "0005-entry-aggregates",
        // One row per `Opaque` directory: the recursive totals a raw filesystem walk found under
        // it, without writing a single `entries` row for what it walked — that is the whole point
        // of `Opaque`. Keyed on `entry_id` with `ON DELETE CASCADE` so a pruned tombstone (the
        // directory itself finally swept out of `entries`) takes its aggregate with it; a
        // `coverage` change away from `Opaque` (an expansion via `materialise_subtree`) leaves the
        // row behind rather than deleting it, which is harmless because every reader joins back
        // through `entries.coverage = Opaque` and simply will not see it any more.
        //
        // Nothing here forces a rescan: the table starts empty, and an `Opaque` row gets its first
        // aggregate the next time an ordinary pass actually visits it.
        sql: r"
        CREATE TABLE entry_aggregates (
            entry_id               INTEGER PRIMARY KEY REFERENCES entries (entry_id) ON DELETE CASCADE,
            root_id                INTEGER NOT NULL,
            recursive_bytes        INTEGER NOT NULL,
            recursive_alloc_bytes  INTEGER NOT NULL,
            entry_count            INTEGER NOT NULL,
            readable               INTEGER NOT NULL,
            computed_generation    INTEGER NOT NULL,
            computed_ms            INTEGER NOT NULL,
            stale                  INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX entry_aggregates_root ON entry_aggregates (root_id);
        ",
    },
];

/// The FTS5 accelerator, created separately: it is the one part of the schema that can legitimately
/// be missing (a SQLite without FTS5), and the index must still work when it is — slower, through
/// the `LIKE` fallback, but correct.
pub const FTS_SQL: &str = r#"
    CREATE VIRTUAL TABLE entries_fts USING fts5(
        name_display,
        path_display,
        tokenize = "trigram"
    );
"#;

/// How many migrations this build ships.
#[must_use]
pub fn schema_version() -> u32 {
    MIGRATIONS.len() as u32
}

/// Applies every migration this build has that the database does not, in order.
///
/// Returns `Err(IndexError::Corrupt)` when the file is unreadable or ahead of this build, which
/// the caller answers by rebuilding rather than by failing.
pub fn migrate(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;

    let bootstrapped: bool = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'index_migrations'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|_| true)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(other),
        })?;

    if bootstrapped {
        let stored: u32 = read_meta(connection, "format_version")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if stored != FORMAT_VERSION {
            return Err(IndexError::Corrupt {
                detail: format!("format version {stored}, this build speaks {FORMAT_VERSION}"),
            });
        }
        let applied: u32 =
            connection.query_row("SELECT COUNT(*) FROM index_migrations", [], |row| {
                row.get(0)
            })?;
        if applied > schema_version() {
            return Err(IndexError::Corrupt {
                detail: format!(
                    "database has {applied} migrations, this build ships {}",
                    schema_version()
                ),
            });
        }
    }

    for migration in MIGRATIONS {
        let already: bool = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'index_migrations'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .is_ok()
            && connection
                .query_row(
                    "SELECT 1 FROM index_migrations WHERE name = ?1",
                    [migration.name],
                    |row| row.get::<_, i64>(0),
                )
                .is_ok();
        if already {
            continue;
        }
        connection.execute_batch(migration.sql)?;
        connection.execute(
            "INSERT INTO index_migrations (name, applied_ms) VALUES (?1, ?2)",
            rusqlite::params![migration.name, crate::model::now_ms()],
        )?;
    }

    write_meta(connection, "format_version", &FORMAT_VERSION.to_string())?;
    write_meta(connection, "schema_version", &schema_version().to_string())?;
    write_meta(connection, "scanner_version", &SCANNER_VERSION.to_string())?;
    if read_meta(connection, "committed_generation")?.is_none() {
        write_meta(connection, "committed_generation", "0")?;
    }
    if read_meta(connection, "min_retained_change_seq")?.is_none() {
        write_meta(connection, "min_retained_change_seq", "0")?;
    }
    Ok(())
}

/// Creates the FTS table if this SQLite can. Returns whether the accelerator is available.
pub fn ensure_fts(connection: &Connection) -> bool {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE name = 'entries_fts'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok();
    if exists {
        return true;
    }
    match connection.execute_batch(FTS_SQL) {
        Ok(()) => true,
        Err(error) => {
            tracing::info!(%error, "no FTS5 trigram tokenizer; substring search falls back to a bounded scan");
            false
        }
    }
}

pub fn read_meta(connection: &Connection, key: &str) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(value)
}

pub fn write_meta(connection: &Connection, key: &str, value: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO index_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}
