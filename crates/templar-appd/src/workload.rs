//! Explicit child scopes: admission never moves the terminal or trusts a claimed application ID.
use crate::{Registry, identity::unified_cgroup_of};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::{path::PathBuf, sync::Arc, time::Duration};
use templar_state::Store;
use templar_types::{WindowClaims, WorkloadClass, WorkloadId, WorkloadRecord, unit};
use zbus::{
    Connection, fdo,
    zvariant::{OwnedValue, Value},
};
use zbus_systemd::systemd1::ManagerProxy;

#[derive(Debug)]
pub struct WorkloadsApi {
    registry: Arc<Registry>,
    connection: Connection,
    manager_connection: Connection,
    store: Store,
    cleanup: Option<tokio::task::JoinHandle<()>>,
}
impl WorkloadsApi {
    /// Caller credentials belong to the serving bus; systemd may use a separate connection
    /// in an isolated fixture. Neither connection changes the person's installed session owner.
    pub async fn new(
        registry: Arc<Registry>,
        connection: Connection,
        manager_connection: Connection,
        store: Store,
    ) -> Result<Self> {
        let watch = async {
            let manager = ManagerProxy::new(&manager_connection).await?;
            let units = manager.receive_unit_removed().await?;
            let bus = fdo::DBusProxy::new(&manager_connection).await?;
            let owners = bus.receive_name_owner_changed().await?;
            manager.subscribe().await?;
            Ok::<_, zbus::Error>((manager, units, owners))
        }
        .await;
        let records = {
            let connection = store.lock();
            let mut statement = connection.prepare(
                "SELECT key, value_json FROM settings_values WHERE key GLOB '_internal.workloads.*'",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (key, json) in records {
            let record: WorkloadRecord =
                serde_json::from_str(&json).context("reading durable workload identity")?;
            ensure!(
                key == format!("_internal.workloads.{}", record.id)
                    && record.systemd_unit
                        == unit::workload_unit_name(record.spawned_by, record.id, record.class)
                    && record
                        .control_group
                        .file_name()
                        .is_some_and(|name| name == record.systemd_unit.as_str())
                    && record.control_group.is_absolute(),
                "stored workload identity is inconsistent"
            );
            if is_live(&record)? {
                registry.register_workload(record);
            } else {
                store.transaction(|tx| {
                    tx.execute("DELETE FROM settings_values WHERE key = ?1", [&key])?;
                    Ok(())
                })?;
            }
        }
        let cleanup = match watch {
            Ok((manager, mut units, mut owners)) => {
                let registry = registry.clone();
                let store = store.clone();
                let serving = connection.clone();
                Some(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            removed = units.next() => {
                                let Some(removed) = removed else { break; };
                                let Ok(args) = removed.args() else { continue; };
                                if unit::parse_workload_unit_name(args.id()).is_none() { continue; }
                            }
                            owner = owners.next() => {
                                let Some(owner) = owner else { break; };
                                let Ok(args) = owner.args() else { continue; };
                                if args.name().as_str() != "org.freedesktop.systemd1" { continue; }
                                if args.new_owner().as_ref().is_some() {
                                    if let Err(error) = manager.subscribe().await {
                                        tracing::warn!(%error, "workload unit subscription unavailable");
                                    }
                                }
                            }
                        }
                        match reap_dead(&registry, &store) {
                            Ok(true) => {
                                if let Ok(emitter) = zbus::object_server::SignalEmitter::new(
                                    &serving,
                                    "/org/templaros/Session/Workloads",
                                ) {
                                    let _ = WorkloadsApi::changed(&emitter).await;
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "workload departure reconciliation failed")
                            }
                        }
                    }
                }))
            }
            Err(error) => {
                tracing::warn!(%error, "systemd workload notifications unavailable; reads reconcile kernel liveness");
                None
            }
        };
        Ok(Self {
            registry,
            connection,
            manager_connection,
            store,
            cleanup,
        })
    }

    async fn admit(
        &self,
        caller: u32,
        uid: u32,
        child: u32,
        class: WorkloadClass,
        summary: &str,
    ) -> Result<String> {
        ensure!(
            caller != child && child > 1,
            "workload must be a distinct owned child"
        );
        // Pin the selected process before reading ownership. A numeric PID reused while the
        // systemd call is in flight must never admit a different process.
        let pid =
            rustix::process::Pid::from_raw(i32::try_from(child)?).context("invalid child PID")?;
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
        validate_child(caller, uid, child)?;
        ensure!(
            !summary.is_empty() && summary.len() <= 256 && !summary.chars().any(char::is_control),
            "invalid executable summary"
        );
        let parent = self
            .registry
            .resolve_window(caller, &WindowClaims::default())?;
        let id = WorkloadId::new();
        let name = unit::workload_unit_name(parent.instance_id, id, class);
        let owned =
            |value: Value<'static>| -> OwnedValue { value.try_into().expect("owned scalar/array") };
        let properties = vec![
            (
                "Description".to_owned(),
                owned(format!("TemplarOS workload {}", class.as_str()).into()),
            ),
            ("Slice".to_owned(), owned("background.slice".into())),
            (
                "PIDFDs".to_owned(),
                OwnedValue::try_from(Value::from(vec![zbus::zvariant::Fd::from(pidfd)]))?,
            ),
            ("MemoryAccounting".to_owned(), owned(true.into())),
            ("CPUAccounting".to_owned(), owned(true.into())),
            ("IOAccounting".to_owned(), owned(true.into())),
            ("TasksAccounting".to_owned(), owned(true.into())),
            ("KillMode".to_owned(), owned("control-group".into())),
        ];
        let manager = ManagerProxy::new(&self.manager_connection).await?;
        manager
            .start_transient_unit(name.clone(), "fail".to_owned(), properties, Vec::new())
            .await
            .context("creating owned workload scope")?;
        let admitted = async {
            let path = manager.get_unit(name.clone()).await?;
            let scope = zbus::Proxy::new(&self.manager_connection, "org.freedesktop.systemd1", path, "org.freedesktop.systemd1.Scope").await?;
            for attempt in 0..40 {
                validate_child(caller, uid, child)?;
                let group: String = scope.get_property("ControlGroup").await?;
                if group.is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                ensure!(group.starts_with('/') && group.split('/').any(|part| part == "background.slice"),
                    "manager did not place workload in background.slice");
                let control_group = PathBuf::from(group);
                if unified_cgroup_of(child)? == control_group {
                    ensure!(!unified_cgroup_of(caller)?.starts_with(&control_group), "workload scope contains its parent");
                    let record = WorkloadRecord {
                        id, spawned_by: parent.instance_id, parent_application: parent.app_id,
                        systemd_unit: name.clone(), control_group, command_summary: summary.to_owned(), class,
                    };
                    let json = serde_json::to_string(&record)?;
                    self.store.transaction(|tx| {
                        tx.execute(
                            "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)",
                            rusqlite::params![format!("_internal.workloads.{id}"), json, templar_state::now_ms()],
                        )?;
                        Ok(())
                    })?;
                    return Ok((record, json));
                }
                if attempt + 1 < 40 { tokio::time::sleep(Duration::from_millis(5)).await; }
            }
            anyhow::bail!("child never entered the observed workload cgroup")
        }.await;
        if admitted.is_err() {
            let _ = manager.stop_unit(name, "replace".to_owned()).await;
        }
        let (record, json) = admitted?;
        self.registry.register_workload(record);
        Ok(json)
    }
}

impl Drop for WorkloadsApi {
    fn drop(&mut self) {
        if let Some(cleanup) = &self.cleanup {
            cleanup.abort();
        }
    }
}

pub(crate) fn is_live(record: &WorkloadRecord) -> Result<bool> {
    let events = std::path::Path::new("/sys/fs/cgroup")
        .join(record.control_group.strip_prefix("/")?)
        .join("cgroup.events");
    let text = match std::fs::read_to_string(&events) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("reading {}", events.display())),
    };
    let populated = text
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
        .context("cgroup.events has no populated state")?;
    match populated {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => anyhow::bail!("invalid cgroup populated state"),
    }
}

fn reap_dead(registry: &Registry, store: &Store) -> Result<bool> {
    let dead = registry.dead_workloads()?;
    if dead.is_empty() {
        return Ok(false);
    }
    store.transaction(|tx| {
        for id in &dead {
            tx.execute(
                "DELETE FROM settings_values WHERE key = ?1",
                [format!("_internal.workloads.{id}")],
            )?;
        }
        Ok(())
    })?;
    registry.remove_workloads(&dead);
    Ok(true)
}

fn validate_child(caller: u32, uid: u32, child: u32) -> Result<()> {
    let status = std::fs::read_to_string(format!("/proc/{child}/status"))
        .context("reading child ownership")?;
    let parent = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:\t"))
        .and_then(|value| value.trim().parse::<u32>().ok());
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:\t"))
        .context("missing child UID")?;
    ensure!(
        parent == Some(caller)
            && ids
                .split_whitespace()
                .all(|id| id.parse::<u32>() == Ok(uid)),
        "PID is not this caller's same-UID child"
    );
    Ok(())
}

#[zbus::interface(name = "org.templaros.Workloads1")]
impl WorkloadsApi {
    async fn register_child(
        &self,
        child_pid: u32,
        class: &str,
        command_summary: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<String> {
        let class = match class {
            "interactive" => WorkloadClass::Interactive,
            "background-compute" => WorkloadClass::BackgroundCompute,
            _ => {
                return Err(fdo::Error::InvalidArgs(
                    "class must be interactive or background-compute".into(),
                ));
            }
        };
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing unique sender".into()))?;
        let bus = fdo::DBusProxy::new(&self.connection).await?;
        let caller = bus
            .get_connection_unix_process_id(sender.clone().into())
            .await?;
        let uid = bus.get_connection_unix_user(sender.clone().into()).await?;
        let json = self
            .admit(caller, uid, child_pid, class, command_summary)
            .await
            .map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        reap_dead(&self.registry, &self.store)
            .map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        Self::changed(&emitter).await?;
        Ok(json)
    }

    async fn resolve(&self, pid: u32) -> fdo::Result<String> {
        reap_dead(&self.registry, &self.store)
            .map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        let group =
            unified_cgroup_of(pid).map_err(|error| fdo::Error::Failed(error.to_string()))?;
        serde_json::to_string(&self.registry.resolve_workload(&group))
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn list(&self) -> fdo::Result<String> {
        reap_dead(&self.registry, &self.store)
            .map_err(|error| fdo::Error::Failed(format!("{error:#}")))?;
        serde_json::to_string(&self.registry.list_workloads())
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    #[zbus(signal)]
    pub async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}
