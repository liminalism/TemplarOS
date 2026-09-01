//! `lege-hostd` — the one privileged policy daemon (ADR-0003). System bus `org.legeos.Host`,
//! polkit actions `org.legeos.host.*`.

use anyhow::{Context, Result};
use clap::Parser;
use lege_types::dbus::host as names;
use std::{future::pending, path::PathBuf, sync::Arc};
use zbus::connection;

mod api;
mod apt;
mod clock;
mod cmd;
mod deploy;
mod drivers;
mod hardware;
mod journal;
mod polkit;
mod session_report;
mod snapshot;
mod state;
mod storage;
mod updates;

use state::{HostdConfig, HostdState};

/// LegeOS host daemon.
#[derive(Debug, Parser)]
#[command(name = "lege-hostd", version, about)]
struct Cli {
    /// Print the detected hardware profile and exit.
    #[arg(long)]
    probe: bool,
    /// Root of the managed system (tests and `--probe` on a non-LegeOS dev machine point this at
    /// a fixture tree; production always uses `/`).
    #[arg(long, default_value = "/")]
    root: PathBuf,
}

/// Wires the real backends: apt as a `systemd-run --collect` child unit, btrfs for snapshots,
/// the boot manager ADR-0004 auto-detects from the installation's actual loader, and the real
/// polkit authority over the same system-bus connection hostd serves on.
fn real_config(root: &std::path::Path, connection: zbus::Connection) -> HostdConfig {
    let commands: Arc<dyn cmd::CommandRunner> = Arc::new(cmd::RealCommandRunner::default());
    let state_dir = root.join("var/lib/legeos/hostd");
    HostdConfig {
        apt: Arc::new(apt::SystemdRunApt {
            commands: commands.clone(),
        }),
        snapshotter: Arc::new(snapshot::BtrfsSnapshotter {
            commands: commands.clone(),
        }),
        boot: deploy::detect_backend(root, commands.clone(), state_dir.join("grub")).into(),
        commands,
        clock: Arc::new(clock::SystemClock),
        polkit: Arc::new(polkit::SystemPolkit::new(connection.clone())),
        journal_dir: state_dir.join("transactions"),
        source_root: root.to_path_buf(),
        // Candidate snapshots live alongside the running deployment on the same btrfs volume;
        // where exactly that top-level subvolume is mounted is an image-layout question
        // (`distro/deploy/README.md`), not one this daemon can discover on its own, so it is a
        // configurable default rather than a hardcoded assumption about `/`.
        deployments_dir: state_dir.join("deployments"),
        esp_root: deploy::esp_root(root),
        deployments_path: state_dir.join("deployments.json"),
        sys_root: root.join("sys"),
        apt_root: root.to_path_buf(),
        etc_dir: root.join("etc/legeos/hardware"),
        // lege-hostd's own RuntimeDirectory (`session/units/system/lege-hostd.service`); the two
        // markers `lege-boot-health` reads live here, written only through Session1.
        run_dir: root.join("run/legeos"),
        cmdline_path: root.join("proc/cmdline"),
        caller_identity: Arc::new(session_report::SystemCallerIdentity::new(connection)),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();

    if cli.probe {
        let profile = hardware::probe(&cli.root.join("sys"))?;
        println!("{}", serde_json::to_string_pretty(&profile)?);
        return Ok(());
    }

    // Say which build is running, for `lege dev status`. Diagnostic only: a stamp that cannot be
    // written must never keep lege-hostd from starting.
    if let Err(error) = lege_build::write_stamp("lege-hostd") {
        tracing::warn!(%error, "could not write the build stamp");
    }
    let connection = connection::Builder::system()?
        .name(names::BUS_NAME)?
        .build()
        .await
        .context("claiming org.legeos.Host on the system bus")?;

    let state = Arc::new(
        HostdState::new(real_config(&cli.root, connection.clone()))
            .context("initializing hostd state")?,
    );
    // Before anything is served: a candidate that was staged and never booted has spent an
    // attempt, and this is the only moment userspace can tell.
    let reconciliation = state.reconcile_boot();
    tracing::info!(boot = %reconciliation.summary(), "boot reconciliation");

    state.resume();

    connection
        .object_server()
        .at(names::ROOT_PATH, api::HardwareApi(state.clone()))
        .await
        .context("serving Hardware1")?;
    connection
        .object_server()
        .at(names::ROOT_PATH, api::DriversApi(state.clone()))
        .await
        .context("serving Drivers1")?;
    connection
        .object_server()
        .at(names::ROOT_PATH, api::UpdatesApi(state.clone()))
        .await
        .context("serving Updates1")?;
    connection
        .object_server()
        .at(names::ROOT_PATH, api::StorageApi(state.clone()))
        .await
        .context("serving Storage1")?;
    connection
        .object_server()
        .at(names::ROOT_PATH, api::DeploymentsApi(state.clone()))
        .await
        .context("serving Deployments1")?;
    connection
        .object_server()
        .at(names::ROOT_PATH, api::SessionApi(state))
        .await
        .context("serving Session1")?;

    tracing::info!(name = names::BUS_NAME, "lege-hostd ready");
    pending::<()>().await;
    Ok(())
}
