//! Persistence for `templar-appd`'s two stateful tables: live `instances` (re-derived from systemd
//! on restart, but tracked so a lookup never needs a D-Bus round-trip) and `desktop_import`
//! bookkeeping (so a `.desktop` re-scan is incremental and can tell a stale entry from a fresh
//! one).

use crate::identity::InstanceRecord;
use crate::variants::{LogicalSource, PersistedLogical};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::{collections::BTreeMap, path::PathBuf};
use templar_state::Store;
use templar_types::{AppId, AppProvenance, AppVariantId, InstanceId, LogicalAppId};

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
            templar_state::now_ms(),
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
///
/// Keyed on the desktop id alone, as it has always been. Two origins that export the same desktop
/// id therefore share one bookkeeping row (last scan wins) even though they are two applications in
/// the registry — the row records only "when did we last read this file", and widening its primary
/// key would break an older binary's `ON CONFLICT (desktop_id)` upsert, which the one-generation
/// rollback rule forbids. `origin` says which copy the row describes.
pub fn record_desktop_import_within(
    tx: &Transaction<'_>,
    desktop_id: &str,
    path: &std::path::Path,
    mtime_ms: i64,
    app_id: &AppId,
    origin: templar_types::AppOriginKind,
) -> Result<()> {
    tx.execute(
        "INSERT INTO desktop_import (desktop_id, path, mtime_ms, app_id, imported_ms, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (desktop_id) DO UPDATE SET
            path = excluded.path, mtime_ms = excluded.mtime_ms, app_id = excluded.app_id,
            imported_ms = excluded.imported_ms, origin = excluded.origin",
        params![
            desktop_id,
            path.to_string_lossy(),
            mtime_ms,
            app_id.as_str(),
            templar_state::now_ms(),
            origin.as_str(),
        ],
    )
    .context("recording desktop_import")?;
    Ok(())
}

/// Every variant the registry has recorded, with the logical application it was grouped under.
pub fn load_variants(store: &Store) -> Result<BTreeMap<AppVariantId, PersistedLogical>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT variant, logical, logical_source FROM app_variants")
        .context("preparing app_variants query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .context("reading app_variants")?;
    let mut variants = BTreeMap::new();
    for row in rows {
        let (variant, logical, source) = row?;
        let (Ok(variant), Ok(logical)) = (
            variant.parse::<AppVariantId>(),
            logical.parse::<LogicalAppId>(),
        ) else {
            // An unreadable row is re-derived on this scan rather than failing the daemon's
            // startup: grouping is presentation, and losing it must never cost a session.
            tracing::warn!(%variant, %logical, "unreadable app_variants row; re-deriving");
            continue;
        };
        variants.insert(
            variant,
            PersistedLogical {
                logical,
                source: LogicalSource::from_str_or_derived(&source),
            },
        );
    }
    Ok(variants)
}

/// Record one variant: its logical application, its origin, and the provenance behind both.
///
/// `first_seen_ms` is written once and never updated — it is what makes "the copy that has been
/// here longest" answerable, and a re-scan must not reset it.
pub fn save_variant_within(
    tx: &Transaction<'_>,
    variant: &AppVariantId,
    logical: &LogicalAppId,
    source: LogicalSource,
    provenance: &AppProvenance,
) -> Result<()> {
    let provenance_json = serde_json::to_string(provenance).context("serializing provenance")?;
    tx.execute(
        "INSERT INTO app_variants
            (variant, logical, origin, trust, provenance, logical_source, first_seen_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (variant) DO UPDATE SET
            logical = excluded.logical, origin = excluded.origin, trust = excluded.trust,
            provenance = excluded.provenance, logical_source = excluded.logical_source",
        params![
            variant.as_str(),
            logical.as_str(),
            provenance.origin.as_str(),
            provenance.effective_trust().as_str(),
            provenance_json,
            source.as_str(),
            if provenance.first_seen_ms > 0 {
                provenance.first_seen_ms
            } else {
                templar_state::now_ms()
            },
        ],
    )
    .context("persisting app variant")?;
    Ok(())
}

/// The trust tier recorded for this variant the last time it was scanned, if it was.
///
/// Used to notice a variant's provenance *moving* — the case where a `.desktop` file in the
/// person's own data directory takes over a desktop id a package used to own, and would otherwise
/// silently inherit that package's provider declarations and autostart approval.
pub fn variant_trust_within(connection: &Connection, variant: &AppVariantId) -> Result<Option<templar_types::TrustTier>> {
    Ok(connection.query_row(
        "SELECT trust FROM app_variants WHERE variant = ?1",
        params![variant.as_str()],
        |row| row.get::<_, String>(0),
    ).optional()?.map(|value| templar_types::TrustTier::from_str_or_user(&value)))
}

/// When this variant was first recorded, if it has been seen before.
pub fn variant_first_seen_within(connection: &Connection, variant: &AppVariantId) -> Result<Option<i64>> {
    Ok(connection.query_row(
        "SELECT first_seen_ms FROM app_variants WHERE variant = ?1",
        params![variant.as_str()],
        |row| row.get::<_, i64>(0),
    ).optional()?)
}

/// Old variant id -> current one. Read on every lookup of a stored id; stored ids themselves are
/// never rewritten, which is what keeps a rollback readable.
pub fn load_variant_aliases(store: &Store) -> Result<BTreeMap<AppVariantId, AppVariantId>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT alias, variant FROM app_variant_aliases")
        .context("preparing app_variant_aliases query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("reading app_variant_aliases")?;
    let mut aliases = BTreeMap::new();
    for row in rows {
        let (alias, variant) = row?;
        if let (Ok(alias), Ok(variant)) = (alias.parse(), variant.parse()) {
            aliases.insert(alias, variant);
        }
    }
    Ok(aliases)
}

/// Record that `alias` used to name what is now `variant`.
pub fn save_variant_alias_within(
    tx: &Transaction<'_>,
    alias: &AppVariantId,
    variant: &AppVariantId,
    reason: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO app_variant_aliases (alias, variant, reason, created_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (alias) DO UPDATE SET variant = excluded.variant, reason = excluded.reason",
        params![
            alias.as_str(),
            variant.as_str(),
            reason,
            templar_state::now_ms()
        ],
    )
    .context("persisting app variant alias")?;
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
            "org.templaros.Viewer",
            "app-templaros-org.templaros.Viewer@1.service",
        );
        let b = record(
            "org.templaros.Files",
            "app-templaros-org.templaros.Files@2.service",
        );

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
                    templar_types::AppOriginKind::DistroPackage,
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
