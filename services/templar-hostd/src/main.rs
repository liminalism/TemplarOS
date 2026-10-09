//! `templar-hostd` — the one privileged policy daemon (ADR-0003). System bus `org.templaros.Host`,
//! polkit actions `org.templaros.host.*`.

mod accounts;
use anyhow::{Context, Result};
use clap::Parser;
use std::{future::pending, path::PathBuf, sync::Arc};
use templar_types::dbus::host as names;
use zbus::connection;

mod api;
mod apt;
mod boot_entries;
mod clock;
mod cmd;
mod deploy;
mod display_devices;
mod display_recovery;
mod drivers;
mod hardware;
mod index_checkpoint;
mod journal;
mod layout;
mod persistence;
mod polkit;
mod session_report;
mod snapshot;
mod state;
mod storage;
mod updates;

use state::{HostdConfig, HostdState};

/// TemplarOS host daemon.
#[derive(Debug, Parser)]
#[command(name = "templar-hostd", version, about)]
struct Cli {
    /// Print the detected hardware profile and exit.
    #[arg(long)]
    probe: bool,
    /// Root of the managed system (tests and `--probe` on a non-TemplarOS dev machine point this at
    /// a fixture tree; production always uses `/`).
    #[arg(long, default_value = "/")]
    root: PathBuf,
}

/// Wires the real backends: apt as a `systemd-run --collect` child unit, btrfs for snapshots,
/// the boot manager ADR-0004 auto-detects from the installation's actual loader, and the real
/// polkit authority over the same system-bus connection hostd serves on.
fn real_config(root: &std::path::Path, connection: zbus::Connection) -> Result<HostdConfig> {
    let commands: Arc<dyn cmd::CommandRunner> = Arc::new(cmd::RealCommandRunner::default());
    let state_dir = root.join("var/lib/templaros/hostd");
    let deployment_layout = layout::DeploymentLayout::read(root)?;
    Ok(HostdConfig {
        apt: Arc::new(apt::SystemdRunApt {
            commands: commands.clone(),
            source_root: root.to_path_buf(),
        }),
        snapshotter: Arc::new(snapshot::BtrfsSnapshotter {
            commands: commands.clone(),
        }),
        boot: deploy::detect_backend(
            root,
            commands.clone(),
            state_dir.join("grub"),
            deployment_layout.read_only,
            deployment_layout.mounted_esp.clone(),
        )
        .into(),
        commands,
        clock: Arc::new(clock::SystemClock),
        polkit: Arc::new(polkit::SystemPolkit::new(connection.clone())),
        journal_dir: state_dir.join("transactions"),
        source_root: root.to_path_buf(),
        deployment_layout,
        deployments_path: state_dir.join("deployments.json"),
        sys_root: root.join("sys"),
        apt_root: root.to_path_buf(),
        hardware_state_dir: root.join("var/lib/templaros/hardware"),
        // templar-hostd's own RuntimeDirectory (`session/units/system/templar-hostd.service`); the two
        // markers `templar-boot-health` reads live here, written only through Session1.
        run_dir: root.join("run/templaros"),
        cmdline_path: root.join("proc/cmdline"),
        caller_identity: Arc::new(session_report::SystemCallerIdentity::new(connection)),
    })
}

/// The system bus, by its real path rather than the `/var/run` compatibility symlink.
///
/// zbus's built-in default is the address the D-Bus specification writes down,
/// `unix:path=/var/run/dbus/system_bus_socket`, which reaches the socket through the
/// `/var/run -> /run` symlink. Every other system daemon on this machine connects to
/// `/run/dbus/system_bus_socket` directly. Taking the short way removes a path component whose
/// resolution a service's mount namespace can change, and costs nothing where the two names are
/// the same file.
const SYSTEM_BUS_ADDRESS: &str = "unix:path=/run/dbus/system_bus_socket";

/// `DBUS_SYSTEM_BUS_ADDRESS` still wins, so a test harness or a container can point this daemon
/// somewhere else exactly as it could before.
fn system_bus_address() -> String {
    std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap_or_else(|_| SYSTEM_BUS_ADDRESS.to_owned())
}

/// What this process can actually see of the bus socket, for the error message.
///
/// `connect(2)` answering `Permission denied` names neither who was refused nor what they were
/// refused on, and those two facts are most of the difference between a policy denial, a socket
/// that is not in this process's mount namespace, and a socket whose mode really does exclude us.
/// Stating them next to the failure makes the journal line diagnose itself instead of starting an
/// investigation.
fn describe_bus_reachability(address: &str) -> String {
    let Some(path) = address.strip_prefix("unix:path=") else {
        return format!("  address {address} is not a unix socket path");
    };
    let socket = match std::fs::metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
            format!(
                "{path}: mode {:o}, uid {}, gid {}, is a socket: {}",
                metadata.permissions().mode() & 0o7777,
                metadata.uid(),
                metadata.gid(),
                metadata.file_type().is_socket(),
            )
        }
        Err(error) => format!("{path}: {error}"),
    };
    let mount_namespace = std::fs::read_link("/proc/self/ns/mnt")
        .map_or_else(|_| "unreadable".to_owned(), |ns| ns.display().to_string());
    format!(
        "  as: {}\n  socket: {socket}\n  confinement: {}\n  mount namespace: {mount_namespace}",
        proc_status_field("Uid").map_or_else(
            || "unknown identity".to_owned(),
            |uid| format!("uid {uid} (real/effective/saved/fs)")
        ),
        read_trimmed("/proc/self/attr/current").unwrap_or_else(|| "unreadable".to_owned()),
    )
}

/// One field of `/proc/self/status` — how this daemon asks who it is without an `unsafe` call
/// into libc, which the crate forbids.
fn proc_status_field(name: &str) -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        Some(
            line.strip_prefix(name)?
                .strip_prefix(':')?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("/"),
        )
    })
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();

    if cli.probe {
        let profile = hardware::probe(&cli.root.join("sys"))?;
        println!("{}", serde_json::to_string_pretty(&profile)?);
        return Ok(());
    }

    // Say which build is running, for `templar dev status`. Diagnostic only: a stamp that cannot be
    // written must never keep templar-hostd from starting.
    if let Err(error) = templar_build::write_stamp("templar-hostd") {
        tracing::warn!(%error, "could not write the build stamp");
    }
    let address = system_bus_address();
    let connection = connection::Builder::address(address.as_str())?
        .build()
        .await
        .with_context(|| {
            format!(
                "connecting to the system bus at {address}\n{}",
                describe_bus_reachability(&address)
            )
        })?;

    let state = Arc::new(
        HostdState::new(real_config(&cli.root, connection.clone())?)
            .context("initializing hostd state")?,
    );
    // Prepared account maintenance is recovered before boot selection or any API resumes.
    accounts::recover(&state).context("recovering root-paired account maintenance")?;
    // Before anything is served: a candidate that was staged and never booted has spent an
    // attempt, and this is the only moment userspace can tell.
    let reconciliation = state.reconcile_boot();
    tracing::info!(boot = %reconciliation.summary(), "boot reconciliation");
    // And then the identities: whatever the bootloader did while hostd was not running, the
    // deployment that actually booted is what the registry must call current.
    if let Err(error) = state.reconcile_registry() {
        tracing::warn!(%error, "could not reconcile the deployment registry");
    }

    state.resume();
    connection
        .object_server()
        .at(
            names::ROOT_PATH,
            display_recovery::DisplayRecoveryApi::new(&cli.root, connection.clone())?,
        )
        .await
        .context("serving DisplayRecovery1")?;
    connection
        .object_server()
        .at(names::ROOT_PATH, accounts::AccountsApi(state.clone()))
        .await
        .context("serving Accounts1")?;

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
    // `SessionApi` consumes the last `Arc`, so the checkpoint surface takes its handle first.
    let checkpoint_state = state.clone();
    let index_checkpoints = Arc::new(index_checkpoint::IndexCheckpoints::new(
        Arc::new(snapshot::BtrfsSnapshotter {
            commands: Arc::new(cmd::RealCommandRunner::default()),
        }),
        Arc::new(cmd::RealCommandRunner::default()),
        Arc::new(clock::SystemClock),
        // `installer/provision.sh` mounts @snapshots at /.snapshots; a checkpoint has to live on
        // the same btrfs filesystem as the subvolume it snapshots, so this is not a state-dir path.
        cli.root.join(".snapshots/index"),
        cli.root.join("var/lib/templaros/hostd/index-deltas"),
        // @home specifically: btrfs snapshots stop at a nested subvolume, so a snapshot of @ would
        // hold an empty home/ and the delta would read as "the whole home directory is gone".
        cli.root.join("home"),
    ));
    connection
        .object_server()
        .at(names::ROOT_PATH, api::SessionApi(state))
        .await
        .context("serving Session1")?;
    // Unadopted (docs/measurements/): read-only @home checkpoints and the metadata delta between
    // two of them. Served so the idea can be measured on a real filesystem; nothing calls it.
    connection
        .object_server()
        .at(
            names::ROOT_PATH,
            api::IndexCheckpointApi {
                state: checkpoint_state,
                checkpoints: index_checkpoints,
            },
        )
        .await
        .context("serving IndexCheckpoint1")?;

    // Publish the well-known name only after recovery and all interfaces are ready.
    // Calls sent to an early name owner can otherwise be lost during initialization.
    connection
        .request_name(names::BUS_NAME)
        .await
        .context("claiming org.templaros.Host after interface readiness")?;

    tracing::info!(name = names::BUS_NAME, "templar-hostd ready");
    pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_factory_consumes_installed_mounts_and_boot_identities() {
        let root = tempfile::tempdir().unwrap();
        let text = include_str!("../../../distro/deploy/layout.toml")
            .replace("@deployments", "@candidates")
            .replace("/var/lib/templaros/deployments", "/candidates")
            .replace("/boot/efi", "/esp");
        let installed = root.path().join(layout::INSTALLED_LAYOUT);
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(installed, text).unwrap();
        std::fs::create_dir_all(root.path().join("esp/loader")).unwrap();
        std::fs::write(
            root.path().join("esp/loader/loader.conf"),
            "default templaros-base.conf\n",
        )
        .unwrap();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (connection, _server) = tokio::try_join!(
            zbus::connection::Builder::unix_stream(client).p2p().build(),
            zbus::connection::Builder::unix_stream(server)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .build(),
        )
        .unwrap();
        let config = real_config(root.path(), connection).unwrap();
        assert_eq!(
            config.deployment_layout.candidate_mount("txn-1"),
            root.path().join("candidates/txn-1")
        );
        assert_eq!(
            config.deployment_layout.subvolume("txn-1"),
            "@candidates/txn-1"
        );
        assert_eq!(
            config.deployment_layout.mounted_esp,
            root.path().join("esp")
        );
        assert_eq!(config.boot.name(), "systemd-boot");
        let host = state::HostdState::new(config).unwrap();
        assert_eq!(
            host.engine.mounted_deployment("@candidates/txn-1").unwrap(),
            root.path().join("candidates/txn-1")
        );
        assert_eq!(
            host.engine.mounted_deployment("@").unwrap(),
            root.path().join("var/lib/templaros/roots/bootstrap")
        );
    }
}
