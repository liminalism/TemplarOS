//! What a database written by the previous release reads like after `0006-app-variants`.
//!
//! The migration adds tables and nullable columns and touches no existing row. That claim is only
//! worth as much as a test that seeds real pre-0006 rows, applies the migration, and reads them
//! back — including through the exact `SELECT` lists the shipped repositories use, so a column
//! added in the wrong place would be caught rather than merely hoped against.

use rusqlite::Connection;

/// Every migration up to but not including `0006-app-variants`, applied by hand, so the fixture is
/// genuinely a pre-0006 database rather than a current one with rows deleted.
fn pre_0006() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_ms INTEGER NOT NULL);",
        )
        .unwrap();
    for migration in lege_state::all_migrations() {
        if migration.name == "0006-app-variants" {
            continue;
        }
        connection.execute_batch(migration.sql).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations (name, applied_ms) VALUES (?1, 0)",
                [migration.name],
            )
            .unwrap();
    }
    connection
}

fn seed(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO mime_defaults (mime, app_id)
                VALUES ('application/pdf', 'org.legeos.Viewer');
             INSERT INTO extension_defaults (extension, app_id, override_specific_mime)
                VALUES ('md', 'org.legeos.Editor', 1);
             INSERT INTO project_mime_defaults (project_id, mime, app_id)
                VALUES ('thesis', 'application/pdf', 'org.legeos.Editor');
             INSERT INTO provider_declarations (app_id, role, priority)
                VALUES ('org.legeos.Shell', 'notification-server', 100);
             INSERT INTO autostart_approvals (app_id, approval)
                VALUES ('org.legeos.Sync', 'approved');
             INSERT INTO desktop_import (desktop_id, path, mtime_ms, app_id, imported_ms)
                VALUES ('firefox.desktop', '/usr/share/applications/firefox.desktop', 42,
                        'desktop-import.firefox.desktop', 43);
             INSERT INTO instances (instance_id, app_id, systemd_unit, control_group, started_ms)
                VALUES ('i', 'org.legeos.Files', 'u.service', '/app.slice/u.service', 7);
             INSERT INTO projects (project_id, json)
                VALUES ('thesis', '{\"id\":\"thesis\",\"name\":\"Thesis\",\"root\":\"/home/p/t\"}');",
        )
        .unwrap();
}

/// Every read the migration could plausibly disturb, as a (query, expected single row) pair.
fn readback(connection: &Connection) -> Vec<Vec<String>> {
    let queries = [
        "SELECT mime, app_id FROM mime_defaults",
        "SELECT extension, app_id, override_specific_mime FROM extension_defaults",
        "SELECT project_id, mime, app_id FROM project_mime_defaults",
        "SELECT app_id, role, priority FROM provider_declarations",
        "SELECT app_id, approval FROM autostart_approvals",
        "SELECT desktop_id, path, mtime_ms, app_id FROM desktop_import",
        "SELECT instance_id, app_id, systemd_unit, control_group FROM instances",
        "SELECT json FROM projects ORDER BY project_id",
    ];
    queries
        .iter()
        .map(|query| {
            let mut statement = connection.prepare(query).unwrap();
            let columns = statement.column_count();
            let mut rows = statement.query([]).unwrap();
            let row = rows.next().unwrap().expect("the seeded row is still there");
            (0..columns)
                .map(|index| {
                    row.get_ref(index)
                        .map(|value| format!("{value:?}"))
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect()
}

#[test]
fn a_pre_0006_database_reads_identically_after_the_migration() {
    let connection = pre_0006();
    seed(&connection);
    let before = readback(&connection);

    let applied = lege_state::migrate(&connection).unwrap();
    assert_eq!(applied, 1, "exactly 0006 was outstanding");

    let after = readback(&connection);
    assert_eq!(
        before, after,
        "0006 must add tables and nullable columns and disturb no stored row"
    );
}

#[test]
fn the_new_columns_are_null_and_the_new_tables_are_empty() {
    let connection = pre_0006();
    seed(&connection);
    lege_state::migrate(&connection).unwrap();

    for (table, column) in [
        ("mime_defaults", "logical_app_id"),
        ("extension_defaults", "logical_app_id"),
        ("project_mime_defaults", "logical_app_id"),
    ] {
        let nulls: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} IS NULL"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 1, "{table}.{column} must default to unknown, not ''");
    }

    // A pre-0006 row's origin is not guessed at migration time; the next scan records it.
    let origin: String = connection
        .query_row("SELECT origin FROM desktop_import", [], |row| row.get(0))
        .unwrap();
    assert_eq!(origin, "unknown");

    for table in ["app_variants", "app_variant_aliases"] {
        let rows: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table} starts empty; the scan fills it");
    }
}

#[test]
fn an_older_binarys_inserts_still_work_after_the_migration() {
    // The one-generation rollback rule: a binary that predates 0006 must be able to keep writing.
    // Every INSERT in the tree names its columns, so the added columns are invisible to it — and
    // `desktop_import`'s upsert still conflicts on `desktop_id`, which means the primary key was
    // not changed underneath it.
    let connection = pre_0006();
    lege_state::migrate(&connection).unwrap();
    connection
        .execute_batch(
            "INSERT INTO mime_defaults (mime, app_id) VALUES ('text/plain', 'org.legeos.Editor');
             INSERT INTO desktop_import (desktop_id, path, mtime_ms, app_id, imported_ms)
                VALUES ('a.desktop', '/a', 1, 'desktop-import.a.desktop', 1)
                ON CONFLICT (desktop_id) DO UPDATE SET path = excluded.path;",
        )
        .unwrap();
}
