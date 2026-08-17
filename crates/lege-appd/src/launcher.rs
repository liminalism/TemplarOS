//! Start applications in dedicated systemd units.
//!
//! For the prototype `systemd-run` is acceptable. Before shipping, replace this command adapter
//! with a direct call to the user manager's `StartTransientUnit()` so launch, unit registration
//! and error handling are one D-Bus transaction (and the registration race disappears).

use crate::identity::{InstanceRecord, Registry};
use anyhow::{Context, Result, bail};
use lege_types::{AppManifest, InstanceId, dbus::env, unit};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{process::Command, time::sleep};

/// How the launcher starts processes. Swappable for tests and for the future D-Bus adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LaunchBackend {
    /// `systemd-run --user` (prototype).
    #[default]
    SystemdRun,
}

/// Launch `manifest` with `arguments` and register the resulting instance.
pub async fn launch(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    arguments: &[String],
    activation_token: Option<&str>,
) -> Result<InstanceRecord> {
    let instance_id = InstanceId::new();
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

    let record = InstanceRecord {
        id: instance_id,
        app_id: manifest.id.clone(),
        systemd_unit: unit_name,
        control_group,
    };

    registry.register_instance(record.clone());
    Ok(record)
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
