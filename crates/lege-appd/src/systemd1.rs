//! Direct calls to the user session's systemd manager (`org.freedesktop.systemd1`, reachable on
//! the *session* bus a user's `systemd --user` instance runs on) via the generated
//! [`zbus_systemd::systemd1`] bindings.
//!
//! `StartTransientUnit()` makes launch and unit registration one D-Bus transaction: by the time
//! the call returns, systemd has already created the unit's cgroup (it does so before the first
//! `ExecStart` process is forked), so the launcher can look up `ControlGroup` and register the
//! instance essentially synchronously — closing the registration race the `systemd-run`
//! subprocess adapter has (a fast client could map a window before the launcher had queried and
//! stored the cgroup; see [`crate::launcher`]).
//!
//! [`transient_unit_properties`] and [`reconcile`] are pure and unit-tested. The rest is thin
//! D-Bus glue that needs a real session bus and so is not covered by the crate's unit tests
//! (rule: no session bus, no root, no real hardware) — it is exercised by `--dry-run` staying
//! silent about it and by manual/integration testing instead.

use anyhow::{Context, Result};
use lege_types::{AppId, AppManifest, InstanceId, dbus::env, unit};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};
use zbus::{
    Connection,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};
use zbus_systemd::systemd1::{ManagerProxy, ServiceProxy};

use crate::identity::InstanceRecord;

fn owned(value: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(value.into())
        .expect("scalar and array zvariant values always convert to OwnedValue")
}

/// Build the `StartTransientUnit` property list for one launch. Never runs `Exec=` (or anything
/// else) through a shell: `ExecStart` is `(executable, argv, ignore_failure)` with the resolved
/// executable as argv\[0\] and the caller's arguments appended verbatim.
pub fn transient_unit_properties(
    manifest: &AppManifest,
    instance_id: InstanceId,
    arguments: &[String],
    activation_token: Option<&str>,
) -> Vec<(String, OwnedValue)> {
    let executable = manifest.executable.to_string_lossy().into_owned();
    let mut argv = vec![executable.clone()];
    argv.extend(arguments.iter().cloned());

    let mut environment = vec![
        format!("{}={}", env::INSTANCE_ID, instance_id),
        format!("{}={}", env::APP_ID, manifest.id),
    ];
    if let Some(token) = activation_token {
        environment.push(format!("XDG_ACTIVATION_TOKEN={token}"));
    }

    vec![
        ("Type".to_owned(), owned("exec".to_owned())),
        ("Slice".to_owned(), owned("app.slice".to_owned())),
        ("MemoryAccounting".to_owned(), owned(true)),
        ("IOAccounting".to_owned(), owned(true)),
        ("TasksAccounting".to_owned(), owned(true)),
        // Whole-application termination if the kernel OOM killer ever fires inside the group.
        ("OOMPolicy".to_owned(), owned("kill".to_owned())),
        ("Environment".to_owned(), owned(environment)),
        (
            "ExecStart".to_owned(),
            owned(vec![(executable, argv, false)]),
        ),
    ]
}

/// Start `manifest` as a transient unit and wait for its `ControlGroup` to appear. Systemd
/// creates the cgroup before running `ExecStart`, so this is normally immediate; the short
/// retry loop only covers the rare case where the property read races the manager's internal
/// bookkeeping.
pub async fn start_transient_unit(
    connection: &Connection,
    manifest: &AppManifest,
    instance_id: InstanceId,
    arguments: &[String],
    activation_token: Option<&str>,
) -> Result<InstanceRecord> {
    let unit_name = unit::unit_name(&manifest.id, instance_id);
    let manager = ManagerProxy::new(connection)
        .await
        .context("connecting to org.freedesktop.systemd1.Manager")?;
    let properties = transient_unit_properties(manifest, instance_id, arguments, activation_token);
    manager
        .start_transient_unit(unit_name.clone(), "fail".to_owned(), properties, Vec::new())
        .await
        .with_context(|| format!("StartTransientUnit({unit_name})"))?;

    let control_group = wait_for_control_group(connection, &unit_name).await?;
    Ok(InstanceRecord {
        id: instance_id,
        app_id: manifest.id.clone(),
        systemd_unit: unit_name,
        control_group,
    })
}

/// Poll for the unit's `ControlGroup` property once `GetUnit` has confirmed the unit
/// `StartTransientUnit` just created actually exists. Systemd sets `ControlGroup` as part of
/// creating the unit, before the first process starts, so this is normally immediate; a handful
/// of short retries is a safety margin, not the primary mechanism. If the property still is not
/// populated once that budget is spent, that is a polling problem, not a launch failure — the
/// unit is confirmed to exist and is running — so a derived guess is returned instead of failing
/// the whole launch (identity still resolves correctly through `unit::parse_cgroup_path`'s
/// self-describing match either way).
async fn wait_for_control_group(connection: &Connection, unit_name: &str) -> Result<PathBuf> {
    let manager = ManagerProxy::new(connection).await?;
    let unit_path = manager
        .get_unit(unit_name.to_owned())
        .await
        .with_context(|| format!("GetUnit({unit_name})"))?;

    // Systemd sets ControlGroup as part of creating the unit, before the first process starts;
    // a handful of short retries is a safety margin, not the primary mechanism (contrast with
    // the `systemd-run` adapter's 100 x 10ms poll for the *whole* launch-to-cgroup path).
    for attempt in 0..20u32 {
        let service = ServiceProxy::builder(connection)
            .path(unit_path.clone())?
            .destination("org.freedesktop.systemd1")?
            .build()
            .await
            .context("building org.freedesktop.systemd1.Service proxy")?;
        match service.control_group().await {
            Ok(value) if !value.is_empty() => return Ok(PathBuf::from(value)),
            _ if attempt + 1 < 20 => tokio::time::sleep(Duration::from_millis(5)).await,
            _ => {}
        }
    }
    tracing::warn!(
        unit_name,
        "systemd did not expose a ControlGroup within the retry budget; registering with a \
         derived path (the unit is running — identity still resolves through its \
         self-describing name)"
    );
    Ok(PathBuf::from(format!("app.slice/{unit_name}")))
}

/// Whether `org.freedesktop.systemd1` answers on `connection` (used to pick the launch backend
/// automatically: direct `StartTransientUnit` when available, `systemd-run` otherwise).
pub async fn is_available(connection: &Connection) -> bool {
    ManagerProxy::new(connection).await.is_ok()
}

/// One `app-legeos-*` unit systemd currently knows about, ground truth for reconciling the
/// in-memory registry after a restart (ADR-0001: "instances persisted and re-derived from
/// `ListUnitsByPatterns app-legeos-*` on restart").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveUnit {
    pub app_id: AppId,
    pub instance_id: InstanceId,
    pub unit_name: String,
    pub control_group: PathBuf,
}

/// Ask systemd for every live `app-legeos-*` unit.
pub async fn list_live_units(connection: &Connection) -> Result<Vec<LiveUnit>> {
    let manager = ManagerProxy::new(connection).await?;
    let units = manager
        .list_units_by_patterns(Vec::new(), vec![format!("{}*", unit::UNIT_PREFIX)])
        .await
        .context("ListUnitsByPatterns")?;

    let mut live = Vec::with_capacity(units.len());
    for (
        name,
        _description,
        _load,
        _active,
        _sub,
        _following,
        unit_path,
        _job_id,
        _job_type,
        _job_path,
    ) in units
    {
        let Some((app_id, instance_id)) = unit::parse_unit_name(&name) else {
            continue; // not one of ours despite matching the glob (shouldn't happen).
        };
        let control_group = service_control_group(connection, &unit_path)
            .await
            .unwrap_or_default();
        live.push(LiveUnit {
            app_id,
            instance_id,
            unit_name: name,
            control_group,
        });
    }
    Ok(live)
}

async fn service_control_group(
    connection: &Connection,
    unit_path: &OwnedObjectPath,
) -> Result<PathBuf> {
    let service = ServiceProxy::builder(connection)
        .path(unit_path.clone())?
        .destination("org.freedesktop.systemd1")?
        .build()
        .await?;
    Ok(PathBuf::from(service.control_group().await?))
}

/// What to do with the in-memory/persisted registry given what systemd reports live right now.
/// Pure: everything above is glue to produce `live`; everything below is policy.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Units systemd has that the registry does not (yet) — register these.
    pub to_register: Vec<LiveUnit>,
    /// Instances the registry has that systemd no longer does — forget these.
    pub to_forget: Vec<InstanceId>,
}

/// Diff systemd's live `app-legeos-*` units against the registry's tracked instances.
pub fn reconcile(live: &[LiveUnit], tracked: &BTreeSet<InstanceId>) -> Reconciliation {
    let live_ids: BTreeSet<InstanceId> = live.iter().map(|unit| unit.instance_id).collect();
    Reconciliation {
        to_register: live
            .iter()
            .filter(|unit| !tracked.contains(&unit.instance_id))
            .cloned()
            .collect(),
        to_forget: tracked.difference(&live_ids).copied().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{AppAliases, ResourceClass};
    use std::collections::BTreeSet;

    fn manifest() -> AppManifest {
        AppManifest {
            id: "org.legeos.Viewer".parse().unwrap(),
            display_name: "Lege Viewer".into(),
            primary_desktop_file_id: "org.legeos.Viewer.desktop".into(),
            executable: "/usr/bin/lege-viewer".into(),
            aliases: AppAliases::default(),
            mime_types: BTreeSet::new(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
        }
    }

    #[test]
    fn transient_unit_properties_never_go_through_a_shell() {
        let instance_id = InstanceId::new();
        let properties = transient_unit_properties(
            &manifest(),
            instance_id,
            &["/tmp/report.pdf".to_owned()],
            Some("activation-token"),
        );
        let by_name = |key: &str| {
            properties
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(String::try_from(by_name("Type").unwrap()).unwrap(), "exec");
        assert_eq!(
            String::try_from(by_name("Slice").unwrap()).unwrap(),
            "app.slice"
        );
        assert!(bool::try_from(by_name("MemoryAccounting").unwrap()).unwrap());
        assert_eq!(
            String::try_from(by_name("OOMPolicy").unwrap()).unwrap(),
            "kill"
        );

        let environment = Vec::<String>::try_from(by_name("Environment").unwrap()).unwrap();
        assert!(environment.contains(&format!("{}={instance_id}", env::INSTANCE_ID)));
        assert!(environment.contains(&"XDG_ACTIVATION_TOKEN=activation-token".to_owned()));

        let exec_start = by_name("ExecStart").unwrap();
        let entries = Vec::<(String, Vec<String>, bool)>::try_from(exec_start).unwrap();
        // Exactly one (path, argv, ignore_failure) entry — the resolved executable as argv[0],
        // the caller's argument appended verbatim, never a shell wrapper.
        assert_eq!(
            entries,
            vec![(
                "/usr/bin/lege-viewer".to_owned(),
                vec![
                    "/usr/bin/lege-viewer".to_owned(),
                    "/tmp/report.pdf".to_owned()
                ],
                false,
            )]
        );
    }

    #[test]
    fn reconcile_registers_new_units_and_forgets_dead_ones() {
        let alive_id = InstanceId::new();
        let dead_id = InstanceId::new();
        let new_id = InstanceId::new();
        let app: AppId = "org.legeos.Viewer".parse().unwrap();

        let live = vec![
            LiveUnit {
                app_id: app.clone(),
                instance_id: alive_id,
                unit_name: unit::unit_name(&app, alive_id),
                control_group: PathBuf::from("/user.slice/app.slice/x"),
            },
            LiveUnit {
                app_id: app.clone(),
                instance_id: new_id,
                unit_name: unit::unit_name(&app, new_id),
                control_group: PathBuf::from("/user.slice/app.slice/y"),
            },
        ];
        let tracked = BTreeSet::from([alive_id, dead_id]);

        let plan = reconcile(&live, &tracked);
        assert_eq!(plan.to_register.len(), 1);
        assert_eq!(plan.to_register[0].instance_id, new_id);
        assert_eq!(plan.to_forget, vec![dead_id]);
    }

    #[test]
    fn reconcile_is_a_no_op_when_nothing_changed() {
        let id = InstanceId::new();
        let app: AppId = "org.legeos.Viewer".parse().unwrap();
        let live = vec![LiveUnit {
            app_id: app.clone(),
            instance_id: id,
            unit_name: unit::unit_name(&app, id),
            control_group: PathBuf::from("/user.slice/app.slice/x"),
        }];
        let tracked = BTreeSet::from([id]);
        assert_eq!(reconcile(&live, &tracked), Reconciliation::default());
    }
}
