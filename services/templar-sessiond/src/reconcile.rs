//! ADR-0001: "instances persisted and re-derived from `ListUnitsByPatterns app-templaros-*` on
//! restart". The in-memory registry starts empty every process start; this brings it (and the
//! persisted `instances` table) back in sync with what systemd actually has running, once, at
//! startup.

use std::{collections::BTreeSet, sync::Arc};
use templar_appd::{Registry, identity::InstanceRecord, store as appd_store, systemd1};
use templar_state::Store;

pub async fn reconcile_instances(
    connection: &zbus::Connection,
    registry: &Arc<Registry>,
    store: &Store,
) {
    let live = match systemd1::list_live_units(connection).await {
        Ok(live) => live,
        Err(error) => {
            tracing::warn!(%error, "could not list live app-templaros-* units; instance state starts empty");
            return;
        }
    };
    let persisted = appd_store::load_instances(store).unwrap_or_else(|error| {
        tracing::warn!(%error, "failed to load persisted instances; treating none as known");
        Vec::new()
    });
    let tracked: BTreeSet<_> = persisted.iter().map(|record| record.id).collect();
    let plan = systemd1::reconcile(&live, &tracked);
    let to_forget: BTreeSet<_> = plan.to_forget.iter().copied().collect();

    let mut restored = 0;
    for record in persisted {
        if to_forget.contains(&record.id) {
            if let Err(error) =
                store.transaction(|tx| appd_store::forget_instance_within(tx, record.id))
            {
                tracing::warn!(%error, instance = %record.id, "failed to forget a dead instance");
            }
        } else {
            // Persistent paths describe an earlier daemon lifetime, not current authority.
            // Only the manager's fresh record may restore an existing instance.
            if let Some(unit) = live.iter().find(|unit| unit.instance_id == record.id) {
                let current = InstanceRecord {
                    id: unit.instance_id,
                    app_id: unit.app_id.clone(),
                    systemd_unit: unit.unit_name.clone(),
                    control_group: unit.control_group.clone(),
                };
                if current.has_managed_identity() {
                    registry.register_instance(current.clone());
                    if let Err(error) =
                        store.transaction(|tx| appd_store::save_instance_within(tx, &current))
                    {
                        tracing::warn!(%error, instance = %current.id, "failed to refresh restored instance");
                    }
                    restored += 1;
                }
            }
        }
    }

    let mut adopted = 0;
    for unit in plan.to_register {
        let record = InstanceRecord {
            id: unit.instance_id,
            app_id: unit.app_id,
            systemd_unit: unit.unit_name.clone(),
            control_group: unit.control_group,
        };
        if !record.has_managed_identity() {
            tracing::warn!(unit = %record.systemd_unit, "systemd inventory returned an inconsistent managed identity");
            continue;
        }
        registry.register_instance(record.clone());
        if let Err(error) = store.transaction(|tx| appd_store::save_instance_within(tx, &record)) {
            tracing::warn!(%error, unit = %unit.unit_name, "failed to persist a newly adopted instance");
        }
        adopted += 1;
    }

    tracing::info!(
        live = live.len(),
        restored,
        adopted,
        forgotten = to_forget.len(),
        "reconciled instances with systemd"
    );
}
