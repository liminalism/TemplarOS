//! Migration `0001-initial` — the founding schema. **Shipped: never edit this string.**

use super::Migration;

pub const MIGRATIONS: &[Migration] = &[Migration::additive(
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
