//! Migrations owned by `lege-sessiond` (identity, associations, providers, autostart, history,
//! notifications, settings, projects, power, location).
//!
//! `0001-initial` (in [`super::initial`]) already carries `instances`, `mime_defaults`,
//! `extension_defaults`, `project_mime_defaults`, `provider_selections`, `autostart_approvals`,
//! `history`, `projects` and `settings_changes`. This slice adds what M1 needs on top: current
//! settings *values* (as opposed to their change log), provider *declarations* (as opposed to
//! the single selection per role), a small notification-ID ledger so a fast crash/restart never
//! reissues a live ID, one shared undo log for every domain that offers "Undo", desktop-entry
//! import bookkeeping (so re-scans are incremental and a later native manifest can be detected),
//! and the `systemd-coredump` journal cursor. `0003` adds persisted background-permission
//! declarations, so `org.legeos.Background1` can answer "what does this app want and why is it
//! running" without a fresh `.desktop` scan.

pub const MIGRATIONS: &[(&str, &str)] = &[(
    "0002-session-domains",
    r"
        CREATE TABLE settings_values (
            key        TEXT PRIMARY KEY,
            value_json TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
        CREATE TABLE provider_declarations (
            app_id   TEXT NOT NULL,
            role     TEXT NOT NULL,
            priority INTEGER NOT NULL,
            PRIMARY KEY (app_id, role)
        );
        CREATE TABLE notifications_active (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id   TEXT NOT NULL,
            app_name   TEXT NOT NULL,
            app_id     TEXT,
            created_ms INTEGER NOT NULL
        );
        -- One shared undo log for every domain that can reverse a change (associations,
        -- settings, provider selection). `domain` picks the applier; `key` is the row identity
        -- inside that domain (a MIME type, an extension, `<project_id>\x1f<mime>`, a setting key
        -- or a provider role). `event_id` is the history event whose 'Undo' action names this
        -- row (`undo:<id>`).
        CREATE TABLE change_log (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            at_ms     INTEGER NOT NULL,
            domain    TEXT NOT NULL,
            key       TEXT NOT NULL,
            old_value TEXT,
            new_value TEXT,
            event_id  TEXT NOT NULL,
            undone    INTEGER NOT NULL DEFAULT 0
        );
        -- One row per imported .desktop file, so a re-scan is incremental (mtime-keyed) and a
        -- later-installed native manifest can be detected and preferred.
        CREATE TABLE desktop_import (
            desktop_id  TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            mtime_ms    INTEGER NOT NULL,
            app_id      TEXT NOT NULL,
            imported_ms INTEGER NOT NULL
        );
        -- systemd-coredump journal ingestion progress (ADR-0011): the journal entry cursor of
        -- the last crash event turned into history, so a restart never re-ingests or misses one.
        CREATE TABLE coredump_cursor (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            cursor     TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
",
), (
    "0003-background-declarations",
    r"
        -- What the last autostart scan found this application declaring (planning doc 01 §7):
        -- the background permissions it requests and, if any, the exclusive role it ties its
        -- autostart to. Separate from `autostart_approvals` (the user's yes/no on top of it) so
        -- org.legeos.Background1 can answer 'what does it want, and did we say yes' without a
        -- fresh .desktop scan.
        CREATE TABLE background_declarations (
            app_id    TEXT PRIMARY KEY,
            role      TEXT,
            requested TEXT NOT NULL
        );
",
)];
