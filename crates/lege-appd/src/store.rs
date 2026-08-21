//! Persistence for `lege-appd`'s two stateful tables: live `instances` (re-derived from systemd
//! on restart, but tracked so a lookup never needs a D-Bus round-trip) and `desktop_import`
//! bookkeeping (so a `.desktop` re-scan is incremental and can tell a stale entry from a fresh
//! one).

use crate::identity::InstanceRecord;
use anyhow::{Context, Result};
use lege_state::Store;
use lege_types::{AppId, InstanceId};
use rusqlite::{Transaction, params};
use std::path::PathBuf;

/// Load every instance row (used at startup, before reconciling against systemd's live units).
pub fn load_instances(store: &Store) -> Result<Vec<InstanceRecord>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT instance_id, app_id, systemd_unit, control_group FROM instances")
        .context("preparing instances query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .context("reading instances")?;
    let mut instances = Vec::new();
    for row in rows {
        let (id, app_id, systemd_unit, control_group) = row?;
        instances.push(InstanceRecord {
            id: InstanceId::parse(&id).with_context(|| format!("invalid instance id {id:?}"))?,
            app_id: app_id
                .parse()
                .with_context(|| format!("invalid app id in instances: {app_id:?}"))?,
            systemd_unit,
            control_group: PathBuf::from(control_group),
        });
    }
    Ok(instances)
}

/// Insert or replace one instance row (a launch, or a restart-time reconciliation adding a unit
/// the registry did not know about yet).
pub fn save_instance_within(tx: &Transaction<'_>, record: &InstanceRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO instances (instance_id, app_id, systemd_unit, control_group, started_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (instance_id) DO UPDATE SET
            app_id = excluded.app_id, systemd_unit = excluded.systemd_unit,
            control_group = excluded.control_group",
        params![
            record.id.to_string(),
            record.app_id.as_str(),
            record.systemd_unit,
            record.control_group.to_string_lossy(),
            lege_state::now_ms(),
        ],
    )
    .context("persisting instance")?;
    Ok(())
}

/// Remove an instance row (its unit is gone: exited, crashed and collected, or killed).
pub fn forget_instance_within(tx: &Transaction<'_>, id: InstanceId) -> Result<()> {
    tx.execute(
        "DELETE FROM instances WHERE instance_id = ?1",
        params![id.to_string()],
    )
    .context("forgetting instance")?;
    Ok(())
}

/// One `.desktop` file already imported, keyed by its merged desktop ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopImportRecord {
    pub desktop_id: String,
    pub path: PathBuf,
    pub mtime_ms: i64,
    pub app_id: AppId,
}

/// Every desktop-entry import recorded so far, keyed by desktop ID.
pub fn load_desktop_imports(store: &Store) -> Result<Vec<DesktopImportRecord>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT desktop_id, path, mtime_ms, app_id FROM desktop_import")
        .context("preparing desktop_import query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .context("reading desktop_import")?;
    let mut records = Vec::new();
    for row in rows {
        let (desktop_id, path, mtime_ms, app_id) = row?;
        records.push(DesktopImportRecord {
            desktop_id,
            path: PathBuf::from(path),
            mtime_ms,
            app_id: app_id
                .parse()
                .with_context(|| format!("invalid app id in desktop_import: {app_id:?}"))?,
        });
    }
    Ok(records)
}

/// Record (or update) one import.
pub fn record_desktop_import_within(
    tx: &Transaction<'_>,
    desktop_id: &str,
    path: &std::path::Path,
    mtime_ms: i64,
    app_id: &AppId,
) -> Result<()> {
    tx.execute(
        "INSERT INTO desktop_import (desktop_id, path, mtime_ms, app_id, imported_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (desktop_id) DO UPDATE SET
            path = excluded.path, mtime_ms = excluded.mtime_ms, app_id = excluded.app_id,
            imported_ms = excluded.imported_ms",
        params![
            desktop_id,
            path.to_string_lossy(),
            mtime_ms,
            app_id.as_str(),
            lege_state::now_ms()
        ],
    )
    .context("recording desktop_import")?;
    Ok(())
}

/// Drop bookkeeping for a `.desktop` file that no longer exists (or is no longer imported
/// because a native manifest now covers it).
pub fn remove_desktop_import_within(tx: &Transaction<'_>, desktop_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM desktop_import WHERE desktop_id = ?1",
        params![desktop_id],
    )
    .context("removing desktop_import bookkeeping")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(app: &str, unit: &str) -> InstanceRecord {
        InstanceRecord {
            id: InstanceId::new(),
            app_id: app.parse().unwrap(),
            systemd_unit: unit.to_owned(),
            control_group: PathBuf::from(format!("/user.slice/app.slice/{unit}")),
        }
    }

    #[test]
    fn instances_persist_and_can_be_forgotten() {
        let store = Store::in_memory().unwrap();
        let a = record(
            "org.legeos.Viewer",
            "app-legeos-org.legeos.Viewer@1.service",
        );
        let b = record("org.legeos.Files", "app-legeos-org.legeos.Files@2.service");

        store
            .transaction(|tx| {
                save_instance_within(tx, &a)?;
                save_instance_within(tx, &b)
            })
            .unwrap();

        let loaded = load_instances(&store).unwrap();
        assert_eq!(loaded.len(), 2);

        store
            .transaction(|tx| forget_instance_within(tx, a.id))
            .unwrap();
        let loaded = load_instances(&store).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, b.id);
    }

    #[test]
    fn desktop_imports_round_trip_and_are_removable() {
        let store = Store::in_memory().unwrap();
        let app_id: AppId = "desktop-import.firefox".parse().unwrap();
        store
            .transaction(|tx| {
                record_desktop_import_within(
                    tx,
                    "firefox.desktop",
                    std::path::Path::new("/usr/share/applications/firefox.desktop"),
                    12345,
                    &app_id,
                )
            })
            .unwrap();

        let loaded = load_desktop_imports(&store).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].mtime_ms, 12345);

        store
            .transaction(|tx| remove_desktop_import_within(tx, "firefox.desktop"))
            .unwrap();
        assert!(load_desktop_imports(&store).unwrap().is_empty());
    }
}
