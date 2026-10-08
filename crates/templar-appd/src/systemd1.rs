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
use std::{collections::BTreeSet, path::PathBuf, time::Duration};
use templar_types::{AppId, AppManifest, InstanceId, dbus::env, unit};
use zbus::{
    Connection,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};
use zbus_systemd::systemd1::{ManagerProxy, ServiceProxy, UnitProxy};

use crate::identity::InstanceRecord;

fn owned(value: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(value.into())
        .expect("scalar and array zvariant values always convert to OwnedValue")
}

/// The variables that decide *which desktop* a launched application joins, in the order a
/// launcher should pass them on.
///
/// These are read from the launching daemon's own environment and stated explicitly on every
/// transient unit, rather than left to the systemd user manager's global environment block. The
/// manager is one per user and outlives any single session — `session/templar-session` says so in its
/// own comment, "a user manager can survive logout" — so its `WAYLAND_DISPLAY` is a single mutable
/// global shared with every other desktop that user logs into. Relying on it means an application
/// launched from TemplarOS can be handed the socket of a session that ended hours ago, connect to
/// nothing, and exit without a window or a word. That is not hypothetical: it is what happened on
/// the first hands-on run, twenty times in a row, with the journal recording nothing but twenty
/// successful starts.
///
/// `sessiond` knows its own display because the session started it. Saying so per launch makes the
/// answer local, correct for a nested or development session as much as an installed one, and
/// impossible for another desktop to change underneath.
pub const SESSION_ENVIRONMENT: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
];

/// [`SESSION_ENVIRONMENT`] as `NAME=value`, read from this process. Absent and empty variables are
/// skipped: passing `WAYLAND_DISPLAY=` would be worse than passing nothing, because it overrides
/// the manager's value with one that cannot work.
pub fn session_environment() -> Vec<String> {
    SESSION_ENVIRONMENT
        .iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok()?;
            (!value.is_empty()).then(|| format!("{name}={value}"))
        })
        .collect()
}

/// Build the `StartTransientUnit` property list for one launch. Never runs `Exec=` (or anything
/// else) through a shell: `ExecStartEx` is `(executable, argv, flags)` with the resolved
/// executable as argv\[0\] and `no-env-expand` preserving every already-expanded argument.
///
/// `session_environment` is [`session_environment()`] in the daemon and an explicit list in tests.
pub fn transient_unit_properties(
    manifest: &AppManifest,
    instance_id: InstanceId,
    arguments: &[String],
    activation_token: Option<&str>,
    session_environment: &[String],
) -> Vec<(String, OwnedValue)> {
    transient_unit_properties_in_directory(manifest, instance_id,
        manifest.executable.to_str().expect("native executable requires broker adapter"), arguments,
        activation_token, session_environment, None)
}

/// Pure property builder with the actual broker command (including the native argv adapter) and
/// optional Place working directory. The original manifest still supplies managed identity.
pub fn transient_unit_properties_in_directory(
    manifest: &AppManifest,
    instance_id: InstanceId,
    executable: &str,
    arguments: &[String],
    activation_token: Option<&str>,
    session_environment: &[String],
    directory: Option<&str>,
) -> Vec<(String, OwnedValue)> {
    let executable = executable.to_owned();
    let mut argv = vec![executable.clone()];
    argv.extend(arguments.iter().cloned());

    let mut environment = vec![
        format!("{}={}", env::INSTANCE_ID, instance_id),
        format!("{}={}", env::APP_ID, manifest.id),
    ];
    environment.extend(session_environment.iter().cloned());
    if let Some(token) = activation_token {
        environment.push(format!("XDG_ACTIVATION_TOKEN={token}"));
    }

    let mut properties = Vec::with_capacity(10 + usize::from(directory.is_some()));
    properties.extend([
        ("Type".to_owned(), owned("exec".to_owned())),
        ("Slice".to_owned(), owned("app.slice".to_owned())),
        ("MemoryAccounting".to_owned(), owned(true)),
        ("IOAccounting".to_owned(), owned(true)),
        ("TasksAccounting".to_owned(), owned(true)),
        // Whole-application termination if the kernel OOM killer ever fires inside the group.
        ("OOMPolicy".to_owned(), owned("kill".to_owned())),
        // An application that dies on startup must be able to say why. Without these the unit
        // inherits the manager's stdio, and a launch that failed instantly left no record at all
        // anywhere — which is exactly how a broken launch came to look like an unresponsive click.
        ("StandardOutput".to_owned(), owned("journal".to_owned())),
        ("StandardError".to_owned(), owned("journal".to_owned())),
        ("Environment".to_owned(), owned(environment)),
        (
            "ExecStartEx".to_owned(),
            owned(vec![(executable, argv, vec!["no-env-expand".to_owned()])]),
        ),
    ]);
    if let Some(directory) = directory {
        properties.push(("WorkingDirectory".to_owned(), owned(directory.to_owned())));
    }
    properties
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
    start_transient_unit_in_directory(connection, manifest, instance_id,
        manifest.executable.to_str().context("native executable requires broker adapter")?,
        arguments, activation_token, None).await
}

pub async fn start_transient_unit_in_directory(
    connection: &Connection,
    manifest: &AppManifest,
    instance_id: InstanceId,
    executable: &str,
    arguments: &[String],
    activation_token: Option<&str>,
    directory: Option<&std::path::Path>,
) -> Result<InstanceRecord> {
    let unit_name = unit::unit_name(&manifest.id, instance_id);
    let manager = ManagerProxy::new(connection)
        .await
        .context("connecting to org.freedesktop.systemd1.Manager")?;
    let properties = transient_unit_properties_in_directory(
        manifest,
        instance_id,
        executable,
        arguments,
        activation_token,
        &session_environment(),
        directory.map(|directory| directory.to_str().context("launch directory must be UTF-8")).transpose()?,
    );
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

/// Poll briefly for the actual ControlGroup after the manager confirms the created unit.
/// An unavailable property cannot authorize a guessed path, even when start succeeded.
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
    anyhow::bail!("systemd accepted {unit_name} but did not expose its actual ControlGroup")
}

/// Whether `org.freedesktop.systemd1` answers on `connection` (used to pick the launch backend
/// automatically: direct `StartTransientUnit` when available, `systemd-run` otherwise).
pub async fn is_available(connection: &Connection) -> bool {
    ManagerProxy::new(connection).await.is_ok()
}

/// One `app-templaros-*` unit systemd currently knows about, ground truth for reconciling the
/// in-memory registry after a restart (ADR-0001: "instances persisted and re-derived from
/// `ListUnitsByPatterns app-templaros-*` on restart").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveUnit {
    pub app_id: AppId,
    pub instance_id: InstanceId,
    pub unit_name: String,
    pub control_group: PathBuf,
}

/// Ask systemd for every live `app-templaros-*` unit.
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
        let control_group = match service_control_group(connection, &unit_path).await {
            Ok(group) if !group.as_os_str().is_empty() => group,
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(unit = %name, %error, "live unit has no trustworthy ControlGroup");
                continue;
            }
        };
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

/// One `app-templaros-*` unit systemd currently knows about, with the outcome the
/// launch-failure watchdog needs: the high-level state, the result, and the main
/// process's waitid reading plus its start/exit timestamps. Ground truth for telling
/// "failed to start" from "ran and quit" without asking the application, which by
/// definition cannot answer any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppUnitState {
    pub app_id: AppId,
    pub instance_id: InstanceId,
    pub unit_name: String,
    /// `ActiveState`: `active`, `failed`, `inactive`, …
    pub active_state: String,
    /// `Result`: `success`, `exit-code`, `signal`, `core-dump`, `oom-kill`, …
    pub result: String,
    /// `ExecMainCode` / `ExecMainStatus`: the waitid code and the exit status or
    /// signal number. 0/0 when the process never ran.
    pub exec_code: i32,
    pub exec_status: i32,
    /// `ExecMainPID`, retained after the process exits: the journal key for the
    /// application's own output. 0 when nothing ever ran.
    pub exec_main_pid: u32,
    /// `ExecMainStartTimestamp` / `ExecMainExitTimestamp`, microseconds since the epoch.
    pub exec_start_us: u64,
    pub exec_exit_us: u64,
    /// `InactiveEnterTimestamp`, microseconds since the epoch: when the unit last went
    /// quiet, so a failure from before this watcher started reads as stale news.
    pub inactive_enter_us: u64,
}

/// Ask systemd for every loaded `app-templaros-*` unit and how it ended. The
/// `ListUnitsByPatterns` tuple already carries the active state, so only units that
/// are *not* running pay for the extra property reads — a healthy session costs one
/// D-Bus call per sweep.
pub async fn list_app_unit_states(connection: &Connection) -> Result<Vec<AppUnitState>> {
    let manager = ManagerProxy::new(connection).await?;
    let units = manager
        .list_units_by_patterns(Vec::new(), vec![format!("{}*", unit::UNIT_PREFIX)])
        .await
        .context("ListUnitsByPatterns")?;

    let mut states = Vec::with_capacity(units.len());
    for (
        name,
        _description,
        _load,
        active_state,
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
        if active_state == "active" || active_state == "reloading" {
            continue; // Running: nothing for the watchdog to say about it.
        }
        let service = ServiceProxy::builder(connection)
            .path(unit_path.clone())
            .context("service unit path")?
            .destination("org.freedesktop.systemd1")
            .context("systemd1 destination")?
            .build()
            .await
            .context("building org.freedesktop.systemd1.Service proxy")?;
        // `InactiveEnterTimestamp` is a Unit property, not a Service one.
        let unit_proxy = UnitProxy::builder(connection)
            .path(unit_path.clone())
            .context("service unit path")?
            .destination("org.freedesktop.systemd1")
            .context("systemd1 destination")?
            .build()
            .await
            .context("building org.freedesktop.systemd1.Unit proxy")?;
        states.push(AppUnitState {
            app_id,
            instance_id,
            unit_name: name,
            active_state,
            result: service.result().await.unwrap_or_default(),
            exec_code: service.exec_main_code().await.unwrap_or_default(),
            exec_status: service.exec_main_status().await.unwrap_or_default(),
            exec_main_pid: service.exec_main_pid().await.unwrap_or_default(),
            exec_start_us: service
                .exec_main_start_timestamp()
                .await
                .unwrap_or_default(),
            exec_exit_us: service.exec_main_exit_timestamp().await.unwrap_or_default(),
            inactive_enter_us: unit_proxy
                .inactive_enter_timestamp()
                .await
                .unwrap_or_default(),
        });
    }
    Ok(states)
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

/// Diff systemd's live `app-templaros-*` units against the registry's tracked instances.
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
    use std::collections::BTreeSet;
    use templar_types::{AppAliases, ResourceClass};

    fn manifest() -> AppManifest {
        AppManifest {
            id: "org.templaros.Viewer".parse().unwrap(),
            display_name: "Templar Viewer".into(),
            primary_desktop_file_id: "org.templaros.Viewer.desktop".into(),
            executable: "/usr/bin/templar-viewer".into(),
            aliases: AppAliases::default(),
            mime_types: BTreeSet::new(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
            icon: None,
            categories: BTreeSet::new(),
            keywords: BTreeSet::new(),
            no_display: false,
            provenance: None,
            provides: Vec::new(),
        }
    }

    /// An unset or empty variable is passed as nothing at all. `WAYLAND_DISPLAY=` would be worse
    /// than silence: it overrides whatever the manager holds with a value that cannot connect.
    #[test]
    fn an_absent_display_is_omitted_rather_than_passed_empty() {
        let instance_id = InstanceId::new();
        let properties =
            transient_unit_properties(&manifest(), instance_id, &[], None, &Vec::new());
        let environment = properties
            .iter()
            .find(|(name, _)| name == "Environment")
            .map(|(_, value)| Vec::<String>::try_from(value.clone()).unwrap())
            .unwrap();
        assert!(
            !environment
                .iter()
                .any(|entry| entry.starts_with("WAYLAND_DISPLAY")),
            "{environment:?}"
        );
    }

    #[test]
    fn project_launch_sets_service_cwd_without_inventing_application_arguments() {
        let properties = transient_unit_properties_in_directory(
            &manifest(), InstanceId::new(), "/usr/bin/templar-viewer", &[], None, &[], Some("/home/test/video-project"),
        );
        let directory = properties.iter().find(|(name, _)| name == "WorkingDirectory").unwrap();
        assert_eq!(String::try_from(directory.1.clone()).unwrap(), "/home/test/video-project");
        let exec = properties.iter().find(|(name, _)| name == "ExecStartEx").unwrap();
        let exec: Vec<(String, Vec<String>, Vec<String>)> = exec.1.clone().try_into().unwrap();
        assert_eq!(exec[0].1, ["/usr/bin/templar-viewer"]);
        let unit = properties.iter().find(|(name, _)| name == "Slice").unwrap();
        assert_eq!(String::try_from(unit.1.clone()).unwrap(), "app.slice");
    }

    #[test]
    fn native_adapter_command_preserves_original_app_identity_and_real_cwd() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
        let manifest = manifest();
        let instance = InstanceId::new();
        let native_arguments = vec![OsString::from("--reader"), OsString::from_vec(b"/tmp/name-\xff.pdf".to_vec())];
        let payload = crate::native_exec::encode(&manifest.executable, &native_arguments, None).unwrap();
        let properties = transient_unit_properties_in_directory(
            &manifest, instance, crate::native_exec::ADAPTER_PATH, &[payload],
            None, &[], Some("/home/test/project"),
        );
        let property = |name: &str| properties.iter().find(|(key, _)| key == name).unwrap().1.clone();
        let exec: Vec<(String, Vec<String>, Vec<String>)> = property("ExecStartEx").try_into().unwrap();
        assert_eq!(exec[0].2, ["no-env-expand"]);
        assert_eq!(exec[0].0, crate::native_exec::ADAPTER_PATH);
        assert_eq!(exec[0].1[0], crate::native_exec::ADAPTER_PATH);
        assert_eq!(exec[0].1.len(), 2);
        let decoded = crate::native_exec::decode(&exec[0].1[1]).unwrap();
        assert_eq!(decoded.executable, manifest.executable);
        assert_eq!(decoded.arguments, native_arguments);
        assert!(decoded.directory.is_none());
        let environment: Vec<String> = property("Environment").try_into().unwrap();
        assert!(environment.contains(&format!("{}={}", env::APP_ID, manifest.id)));
        assert!(environment.contains(&format!("{}={instance}", env::INSTANCE_ID)));
        assert_eq!(String::try_from(property("WorkingDirectory")).unwrap(), "/home/test/project");
        assert_eq!(String::try_from(property("Slice")).unwrap(), "app.slice");
    }

    #[test]
    fn transient_unit_properties_never_go_through_a_shell() {
        let instance_id = InstanceId::new();
        let properties = transient_unit_properties(
            &manifest(),
            instance_id,
            &["/tmp/${HOME}-$VAR-$$.pdf".to_owned(), "--fixed=${HOME}:$VAR:$$".to_owned()],
            Some("activation-token"),
            &["WAYLAND_DISPLAY=wayland-0".to_owned()],
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
        // The launched application is *told* which compositor to join. Leaving this to the systemd
        // user manager's global environment is what sent twenty terminals to a socket belonging to
        // a session that had already ended.
        assert!(environment.contains(&"WAYLAND_DISPLAY=wayland-0".to_owned()));

        // A launch that dies immediately must leave something behind to read.
        assert_eq!(
            String::try_from(by_name("StandardError").unwrap()).unwrap(),
            "journal"
        );

        assert!(by_name("ExecStart").is_none());
        let exec_start = by_name("ExecStartEx").unwrap();
        let entries = Vec::<(String, Vec<String>, Vec<String>)>::try_from(exec_start).unwrap();
        // Exactly one literal (path, argv, flags) entry, never a shell wrapper.
        assert_eq!(
            entries,
            vec![(
                "/usr/bin/templar-viewer".to_owned(),
                vec![
                    "/usr/bin/templar-viewer".to_owned(),
                    "/tmp/${HOME}-$VAR-$$.pdf".to_owned(),
                    "--fixed=${HOME}:$VAR:$$".to_owned(),
                ],
                vec!["no-env-expand".to_owned()],
            )]
        );
    }

    #[test]
    fn reconcile_registers_new_units_and_forgets_dead_ones() {
        let alive_id = InstanceId::new();
        let dead_id = InstanceId::new();
        let new_id = InstanceId::new();
        let app: AppId = "org.templaros.Viewer".parse().unwrap();

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
        let app: AppId = "org.templaros.Viewer".parse().unwrap();
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
