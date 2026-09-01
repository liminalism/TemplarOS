//! Migrations owned by `lege-filed` (operation journals, recents, last-used directories,
//! trash bookkeeping, the search index).
//!
//! `0001-initial` already carries `file_operations` and `last_directories`; this adds what the
//! service needs to survive its own crash: undo records that outlive a committed operation, the
//! recent-file list the Home Surface reads, and the search index.

use super::Migration;

pub const MIGRATIONS: &[Migration] = &[Migration::additive(
    "0003-files",
    r"
        -- Progress and provenance for a journalled operation. The journal itself stays in
        -- file_operations.journal; these columns are what a UI lists without parsing it.
        ALTER TABLE file_operations ADD COLUMN total_bytes INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE file_operations ADD COLUMN done_bytes INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE file_operations ADD COLUMN total_items INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE file_operations ADD COLUMN done_items INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE file_operations ADD COLUMN app_id TEXT;
        ALTER TABLE file_operations ADD COLUMN summary TEXT NOT NULL DEFAULT '';
        ALTER TABLE file_operations ADD COLUMN error TEXT;
        CREATE INDEX file_operations_started ON file_operations (started_ms DESC);

        -- What an undo would do, recorded at commit time so it survives a restart. Deleting is
        -- irreversible and never gets a row here.
        CREATE TABLE file_undo (
            op_id      TEXT PRIMARY KEY REFERENCES file_operations (op_id) ON DELETE CASCADE,
            plan       TEXT NOT NULL,
            expires_ms INTEGER
        );

        -- Recently used files, the Recent drawer's source. Kept per canonical application so
        -- 'recent in this app' and 'recent anywhere' are both answerable.
        CREATE TABLE recents (
            path      TEXT NOT NULL,
            app_id    TEXT,
            at_ms     INTEGER NOT NULL,
            mime      TEXT,
            action    TEXT NOT NULL,
            PRIMARY KEY (path, app_id, action)
        );
        CREATE INDEX recents_at ON recents (at_ms DESC);

        -- Search index over indexed roots. Content indexing is out of scope for the first
        -- release; this is names, paths and metadata, which is what the drawers search.
        CREATE TABLE search_roots (
            root        TEXT PRIMARY KEY,
            indexed_ms  INTEGER
        );
        CREATE TABLE search_entries (
            path       TEXT PRIMARY KEY,
            root       TEXT NOT NULL REFERENCES search_roots (root) ON DELETE CASCADE,
            name_fold  TEXT NOT NULL,
            is_dir     INTEGER NOT NULL,
            size_bytes INTEGER NOT NULL,
            mtime_ms   INTEGER NOT NULL,
            mime       TEXT
        );
        CREATE INDEX search_entries_name ON search_entries (name_fold);
        CREATE INDEX search_entries_root ON search_entries (root);

        -- Saved queries a drawer can be. A drawer is a directory, a mount, a saved query, a tag
        -- or a project (planning doc 01 section 1).
        CREATE TABLE saved_queries (
            name   TEXT PRIMARY KEY,
            query  TEXT NOT NULL,
            roots  TEXT NOT NULL,
            at_ms  INTEGER NOT NULL
        );
        ",
)];
