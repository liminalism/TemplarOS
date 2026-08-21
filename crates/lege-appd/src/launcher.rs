//! Start applications in dedicated systemd units.
//!
//! [`LaunchBackend::Systemd1`] — a direct `org.freedesktop.systemd1.Manager.StartTransientUnit()`
//! call (`crate::systemd1`) — is the backend that ships: it makes launch and unit registration
//! one D-Bus transaction, so the instance is registered before its process can map a window.
//! [`LaunchBackend::SystemdRun`] (the `systemd-run --user` subprocess adapter) is kept as the
//! fallback for a session bus where systemd does not answer; [`LaunchBackend::detect`] picks
//! automatically. Neither ever runs `Exec=` through a shell: the registered executable plus an
//! argument vector.

use crate::identity::{InstanceRecord, Registry};
use crate::systemd1;
use anyhow::{Context, Result, bail};
use lege_client::proxies::ResourcesProxy;
use lege_types::{AppManifest, InstanceId, ResourceClass, dbus::env, unit};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{process::Command, time::sleep};

/// How the launcher starts processes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LaunchBackend {
    /// `StartTransientUnit()` on the session bus. Ships as the default.
    Systemd1,
    /// `systemd-run --user`. Fallback when systemd does not answer on the session bus.
    #[default]
    SystemdRun,
}

impl LaunchBackend {
    /// Probe the session bus once at startup and pick the backend to use for the rest of the
    /// process's life. `None` (no session bus, e.g. `--dry-run`) always yields `SystemdRun`,
    /// which is never actually invoked in that mode either.
    pub async fn detect(connection: Option<&zbus::Connection>) -> Self {
        match connection {
            Some(connection) if systemd1::is_available(connection).await => Self::Systemd1,
            _ => Self::SystemdRun,
        }
    }
}

/// Launch `manifest` with `arguments` and register the resulting instance.
pub async fn launch(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    arguments: &[String],
    activation_token: Option<&str>,
    backend: LaunchBackend,
    connection: Option<&zbus::Connection>,
) -> Result<InstanceRecord> {
    let instance_id = InstanceId::new();
    let record = match (backend, connection) {
        (LaunchBackend::Systemd1, Some(connection)) => systemd1::start_transient_unit(
            connection,
            &manifest,
            instance_id,
            arguments,
            activation_token,
        )
        .await
        .context("launching via StartTransientUnit")?,
        _ => launch_via_systemd_run(&manifest, instance_id, arguments, activation_token).await?,
    };
    registry.register_instance(record.clone());
    if let Some(connection) = connection {
        push_resource_hints(connection, instance_id, &manifest).await;
    }
    Ok(record)
}

/// Best-effort: tell `lege-resourced` this instance's manifest-derived protection before the
/// governor ever needs to classify it (`org.legeos.Resources1.SetInstanceHints`). Not fatal —
/// the governor treats an instance it has no hints for as an ordinary, unprotected one, and will
/// get these same hints the next time this instance's checkpoint state changes.
async fn push_resource_hints(
    connection: &zbus::Connection,
    instance_id: InstanceId,
    manifest: &AppManifest,
) {
    let Ok(proxy) = ResourcesProxy::new(connection).await else {
        return;
    };
    let protected = manifest.resource_class == ResourceClass::SessionCritical;
    let disposable = manifest.resource_class == ResourceClass::Disposable;
    if let Err(error) = proxy
        .set_instance_hints(
            &instance_id.to_string(),
            protected,
            disposable,
            manifest.supports_checkpoint,
            false,
        )
        .await
    {
        tracing::debug!(%error, %instance_id, "could not push resource hints (lege-resourced may not be running yet)");
    }
}

async fn launch_via_systemd_run(
    manifest: &AppManifest,
    instance_id: InstanceId,
    arguments: &[String],
    activation_token: Option<&str>,
) -> Result<InstanceRecord> {
    let unit_name = unit::unit_name(&manifest.id, instance_id);

    let mut command = Command::new("systemd-run");
    command
        .arg("--user")
        .arg("--collect")
        .arg("--quiet")
        .arg("--service-type=exec")
        .arg(format!("--unit={unit_name}"))
        .arg("--property=Slice=app.slice")
        .arg("--property=MemoryAccounting=yes")
        .arg("--property=IOAccounting=yes")
        .arg("--property=TasksAccounting=yes")
        // Whole-application termination if the kernel OOM killer ever fires inside the group.
        .arg("--property=OOMPolicy=kill")
        .arg(format!("--setenv={}={instance_id}", env::INSTANCE_ID))
        .arg(format!("--setenv={}={}", env::APP_ID, manifest.id));

    if let Some(token) = activation_token {
        command.arg(format!("--setenv=XDG_ACTIVATION_TOKEN={token}"));
    }

    // Never run `Exec=` through a shell: the registered executable plus an argument vector.
    command.arg("--").arg(&manifest.executable).args(arguments);

    let status = command
        .status()
        .await
        .context("starting application through systemd-run")?;
    if !status.success() {
        bail!("systemd-run failed with {status}");
    }

    let control_group = wait_for_control_group(&unit_name).await?;

    Ok(InstanceRecord {
        id: instance_id,
        app_id: manifest.id.clone(),
        systemd_unit: unit_name,
        control_group,
    })
}

async fn wait_for_control_group(unit: &str) -> Result<PathBuf> {
    for _ in 0..100 {
        let output = Command::new("systemctl")
            .args(["--user", "show", unit, "--property=ControlGroup", "--value"])
            .output()
            .await
            .context("querying systemd ControlGroup")?;

        if output.status.success() {
            let value = String::from_utf8(output.stdout)?;
            let value = value.trim();
            if !value.is_empty() {
                return Ok(PathBuf::from(value));
            }
        }

        sleep(Duration::from_millis(10)).await;
    }

    bail!("systemd did not expose a ControlGroup for {unit}")
}
