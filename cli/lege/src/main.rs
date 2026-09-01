//! `lege` — one command line over the OS.
//!
//! `docs/architecture/accessibility.md` makes this a requirement rather than a convenience:
//! everything the shell shows must be reachable here, so the system stays usable when the shell
//! is broken. Every subcommand is human-readable by default and machine-readable with `--json`,
//! and a service that is not running produces one readable sentence — never a panic or a
//! `zbus` debug dump.
//!
//! All the rendering and parsing lives in the library half (`lege::*`), behind per-interface
//! client traits, so it is unit-tested without a session bus. This file is only argument
//! parsing, connection setup and dispatch.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lege::{
    bus, dev, doctor, files, history, open, power, project, providers, res, settings, update,
};
use lege_client::proxies::{
    AppIdentityProxy, AssociationsProxy, AudioProxy, BacklightProxy, BatteryProxy,
    FilesActionsProxy, FilesOperationsProxy, FilesRecentsProxy, FilesSearchProxy, FilesTrashProxy,
    FilesVolumesProxy, HistoryProxy, HostDeploymentsProxy, HostDriversProxy, HostHardwareProxy,
    HostStorageProxy, HostUpdatesProxy, LauncherProxy, PowerProxy, ProjectsProxy, ProvidersProxy,
    ResourcesProxy, SettingsProxy,
};
use lege_types::ProviderRole;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

/// The LegeOS command line.
#[derive(Debug, Parser)]
#[command(name = "lege", version, about, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Applications: identity, instances, launching.
    App {
        #[command(subcommand)]
        command: AppCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Open a file or URI through the association broker (never bypasses it).
    Open {
        /// Path or URI.
        target: String,
        /// Print the full resolution chain without launching.
        #[arg(long)]
        explain: bool,
        /// Resolve within this project's defaults.
        #[arg(long, default_value = "")]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Resource governor: pressure, stage, victim scores and traces.
    Res {
        #[command(subcommand)]
        command: Option<ResCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// File service: volumes, operations, recents, search and path forms.
    Files {
        #[command(subcommand)]
        command: FilesCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Volume, display brightness and battery.
    Status {
        #[command(subcommand)]
        command: Option<StatusCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// The trash: what is in it, putting something back, and emptying it.
    Trash {
        #[command(subcommand)]
        command: Option<TrashCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Host updates, deployments, drivers and the storage reserve (system bus).
    Update {
        #[command(subcommand)]
        command: Option<UpdateCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// System-wide change history.
    History {
        #[command(subcommand)]
        command: Option<HistoryCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Exclusive-role providers.
    Providers {
        #[command(subcommand)]
        command: Option<ProvidersCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Typed settings: schema, values and the change history.
    Settings {
        #[command(subcommand)]
        command: Option<SettingsCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Project sessions.
    Project {
        #[command(subcommand)]
        command: Option<ProjectCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Power policy per source.
    Power {
        #[command(subcommand)]
        command: Option<PowerCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// End, suspend or lock this session.
    Session {
        #[command(subcommand)]
        command: Option<SessionCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Developer deployment: what is installed, what is running, and the gap between them.
    Dev {
        #[command(subcommand)]
        command: Option<DevCommand>,
        /// The local archive build sets are published to and installed from.
        #[arg(long, global = true, default_value = lege::dev::repo::DEFAULT_ROOT)]
        repo: PathBuf,
        /// Act on an unpacked root instead of `/`.
        #[arg(long, global = true, default_value = "/")]
        root: PathBuf,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Integration checks (runtime, image root, or hardware matrix).
    Doctor {
        /// Check an unpacked image root instead of `/`.
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Include hardware-matrix checks.
        #[arg(long)]
        hardware: bool,
        /// Select one hardware case or built-in check (for example `gpu-mode`).
        #[arg(long, requires = "hardware")]
        check: Option<String>,
        /// Override hardware profile detection when reviewing a matrix off-device.
        #[arg(long, requires = "hardware")]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DevCommand {
    /// Per-component convergence: installed set against running builds (the default).
    Status {
        /// Compare against this build set instead of the newest one installed.
        #[arg(long)]
        target: Option<String>,
    },
    /// The local package archive this machine installs build sets from.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Install a published build set — one pinned apt transaction for all of it.
    Install {
        /// Which set; defaults to the archive's current one.
        build_set: Option<String>,
        /// Print the transaction and stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Put the previous build set back.
    Rollback {
        /// Print the transaction and stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Publish, install, activate and report — the whole deployment.
    Deploy {
        /// The manifest `cargo xtask build-set` wrote.
        #[arg(long, default_value = "target/debian/build-set.json")]
        build_set: PathBuf,
        /// Print the whole pipeline and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Put the previous set back without asking if a component fails its health gate.
        #[arg(long)]
        auto_rollback: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Create the archive and its per-machine signing key. Idempotent.
    Init {
        /// Also write apt's keyring and deb822 source. Needs root.
        #[arg(long)]
        install_source: bool,
        /// Write those under this root instead of `/`.
        #[arg(long, default_value = "/")]
        sysroot: PathBuf,
    },
    /// Publish a built set into the archive.
    Publish {
        /// The manifest `cargo xtask build-set` wrote.
        #[arg(long, default_value = "target/debian/build-set.json")]
        build_set: PathBuf,
    },
    /// What the archive holds, and what a rollback would go back to.
    Status,
    /// Drop all but the newest sets (never the current or previous one).
    Prune {
        #[arg(long, default_value_t = lege::dev::repo::DEFAULT_KEEP)]
        keep: usize,
    },
}

#[derive(Debug, Subcommand)]
enum AppCommand {
    /// List installed application manifests.
    List,
    /// List running instances.
    Instances,
    /// Launch an application by canonical ID.
    Launch {
        app_id: String,
        #[arg(trailing_var_arg = true)]
        arguments: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ResCommand {
    /// Pressure level, stage and per-instance scores (the default).
    Status,
    /// Why this instance would (or would not) be chosen as the next victim.
    Explain { instance_id: String },
    /// Move the containment stage (`observe-only|trim|throttle|checkpoint|terminate`).
    SetStage { stage: String },
    /// Recorded pressure traces, newest first.
    Traces {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Subcommand)]
enum FilesCommand {
    /// Every volume with its free space and reserve.
    Volumes,
    /// Recent file operations and their progress.
    Operations {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Stop a running operation at its next step boundary.
    Cancel { op_id: String },
    /// Reverse a committed operation.
    Undo { op_id: String },
    /// Recently used files.
    Recents {
        #[arg(long, default_value = "")]
        app: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Which directory a chooser should open at, and why.
    Suggest {
        #[arg(long, default_value = "")]
        app: String,
        /// `open`, `save`, `import` or `export`.
        #[arg(long, default_value = "open")]
        operation: String,
    },
    /// Search indexed roots by name and path.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Render a path in one of the canonical forms.
    Path {
        path: String,
        /// `canonical|as-shown|uri|shell-quoted|parent-directory|name`; omit to list the forms.
        #[arg(long)]
        form: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum StatusCommand {
    /// Volume, brightness and battery together (the default).
    All,
    /// The default sink's volume.
    Volume {
        /// Set the volume, 0..=100.
        #[arg(long)]
        set: Option<u32>,
    },
    /// Flip the default sink's mute.
    Mute,
    /// Volume up one step.
    Louder,
    /// Volume down one step.
    Quieter,
    /// The display backlight.
    Brightness {
        /// Set the brightness, 1..=100.
        #[arg(long)]
        set: Option<u32>,
    },
    /// Backlight up one step.
    Brighter,
    /// Backlight down one step.
    Dimmer,
    /// Charge, status and time remaining.
    Battery,
}

#[derive(Debug, Subcommand)]
enum TrashCommand {
    /// Everything in every trash, newest deletion first (the default).
    List,
    /// Put trashed items back where they came from, by their path under `Trash/files`.
    Restore { trashed_paths: Vec<String> },
    /// Remove trashed items for good. There is no undo.
    Delete {
        trashed_paths: Vec<String>,
        /// Required: this cannot be undone.
        #[arg(long)]
        yes: bool,
    },
    /// Empty every trash this user has. There is no undo.
    Empty {
        /// Required: this cannot be undone.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum UpdateCommand {
    /// The transaction in flight, if any (the default).
    Status,
    /// Plan a transaction from a `ChangeSet` JSON document.
    Plan { changes_json: String },
    /// Every transaction the journal remembers.
    History,
    /// Boot deployments and which one is known-good.
    Deployments,
    /// Stage a deployment for a trial boot.
    Stage {
        id: String,
        #[arg(long, default_value_t = 3)]
        tries: u8,
    },
    /// Bless the currently booted deployment.
    MarkGood,
    /// Roll back to a deployment.
    Rollback { id: String },
    /// The driver authority's plan and any support violations.
    Drivers,
    /// Just the driver support violations.
    Violations,
    /// Accept the installed NVIDIA versions as the new locked baseline.
    RepairDrivers,
    /// Storage reserve for one mount point.
    Storage {
        #[arg(long, default_value = "/")]
        mount: String,
    },
    /// Run the trim order against a mount point until the reserve is healthy.
    EnforceStorage {
        #[arg(long, default_value = "/")]
        mount: String,
    },
    /// Cache directories trimmed first when the reserve is threatened.
    TrimOrder,
    /// The detected graphics profile and the declared GPU mode.
    Hardware,
    /// Set the declarative GPU mode (`hybrid|integrated|discrete`).
    SetGpuMode { mode: String },
}

#[derive(Debug, Subcommand)]
enum HistoryCommand {
    /// Most recent events (the default).
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Search event messages.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Reverse an event by ID.
    Undo { event_id: String },
}

#[derive(Debug, Subcommand)]
enum ProvidersCommand {
    /// Every exclusive role and how it resolved (the default).
    Report,
    /// Choose the provider for a role.
    Select { role: String, app_id: String },
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Every key at its effective value (the default).
    List,
    /// The typed schema with defaults and descriptions.
    Schema,
    /// One key's effective value.
    Get { key: String },
    /// Set a key; the value is JSON (`{"type":"choice","value":"enforce"}`).
    Set {
        key: String,
        value_json: String,
        #[arg(long, default_value = "lege-cli")]
        origin: String,
    },
    /// Remove a stored override, returning to the schema default.
    Reset {
        key: String,
        #[arg(long, default_value = "lege-cli")]
        origin: String,
    },
    /// The settings change audit trail.
    Changes {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Every project and whether it is open (the default).
    List,
    Create {
        project_id: String,
        name: String,
        root: String,
    },
    Open {
        project_id: String,
    },
    Close {
        project_id: String,
    },
    Remove {
        project_id: String,
    },
    /// Everything one project holds: its root, directories, applications and importance.
    Show {
        project_id: String,
    },
    /// Change one thing about a project.
    Set {
        project_id: String,
        /// name | root | importance | state | directories | terminal_directories | mounts |
        /// references
        key: String,
        /// The new value. List-valued keys take a comma-separated list.
        value: String,
    },
    /// The project the session is working in. With no argument, print it; with one, switch to it;
    /// with `none`, clear it.
    Current {
        project_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum PowerCommand {
    /// The effective profile for a source (the default is both).
    Get {
        /// `battery` or `mains`; omit for both.
        source: Option<String>,
    },
    /// Remove a stored override for a source.
    Reset { source: String },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// What this machine will and will not do (the default).
    Status,
    /// End this session.
    Logout,
    Reboot,
    Poweroff,
    Suspend,
    /// Lock the screen. Not yet implemented — see `Lifecycle1.Lock`.
    Lock,
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Err(error) = run().await {
        // One readable sentence, with the causes that explain it — never a panic or a debug dump.
        eprintln!("lege: {error:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// Print `value` as pretty JSON when `json`, otherwise the already-rendered `human` text.
fn emit(json: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}

/// `lege dev`, whose subcommands are all synchronous — no bus, no session, just the filesystem
/// and a few external tools.
fn run_dev(command: Option<DevCommand>, repo: PathBuf, root: &Path, json: bool) -> Result<String> {
    let command = command.unwrap_or(DevCommand::Status { target: None });
    match command {
        DevCommand::Status { target } => dev::status(root, target.as_deref(), json),
        DevCommand::Repo { command } => match command {
            RepoCommand::Init {
                install_source,
                sysroot,
            } => dev::repo_init(repo, &sysroot, install_source),
            RepoCommand::Publish { build_set } => dev::repo_publish(repo, &build_set),
            RepoCommand::Status => dev::repo_status(repo),
            RepoCommand::Prune { keep } => dev::repo_prune(repo, keep),
        },
        DevCommand::Install { build_set, dry_run } => {
            dev::install_set(repo, root, build_set.as_deref(), dry_run)
        }
        DevCommand::Rollback { dry_run } => dev::rollback(repo, root, dry_run),
        DevCommand::Deploy {
            build_set,
            dry_run,
            auto_rollback,
        } => dev::deploy(
            repo,
            root,
            &dev::DeployOptions {
                build_set,
                dry_run,
                auto_rollback,
            },
        ),
    }
}

async fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Dev {
            command,
            repo,
            root,
            json,
        } => {
            print!("{}", run_dev(command, repo, &root, json)?);
            if json {
                println!();
            }
            Ok(())
        }
        Command::Doctor {
            root,
            hardware,
            check,
            profile,
            json,
        } => {
            let context = doctor::Context { root, hardware };
            let findings = doctor::run_all_with_hardware(
                &context,
                &doctor::hardware::HardwareOptions { check, profile },
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&findings)?);
            } else {
                let mut abbreviated = false;
                for finding in &findings {
                    let detail = doctor::abbreviate(&finding.detail);
                    abbreviated |= detail != finding.detail;
                    println!(
                        "{:<8} {:<20} {}{}",
                        format!("{:?}", finding.outcome).to_lowercase(),
                        finding.id,
                        finding.summary,
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!(" — {detail}")
                        }
                    );
                }
                println!("\n{}", doctor::summarise(&findings));
                if abbreviated {
                    println!("(detail shortened; `lege doctor --json` prints it in full)");
                }
            }
            std::process::exit(doctor::exit_code(&findings));
        }
        Command::App { command, json } => app(command, json).await,
        Command::Open {
            target,
            explain,
            project,
            json,
        } => open_target(&target, explain, &project, json).await,
        Command::Res { command, json } => {
            res_command(command.unwrap_or(ResCommand::Status), json).await
        }
        Command::Files { command, json } => files_command(command, json).await,
        Command::Status { command, json } => {
            status_command(command.unwrap_or(StatusCommand::All), json).await
        }
        Command::Trash { command, json } => {
            trash_command(command.unwrap_or(TrashCommand::List), json).await
        }
        Command::Update { command, json } => {
            update_command(command.unwrap_or(UpdateCommand::Status), json).await
        }
        Command::History { command, json } => {
            history_command(
                command.unwrap_or(HistoryCommand::Recent { limit: 20 }),
                json,
            )
            .await
        }
        Command::Providers { command, json } => {
            providers_command(command.unwrap_or(ProvidersCommand::Report), json).await
        }
        Command::Settings { command, json } => {
            settings_command(command.unwrap_or(SettingsCommand::List), json).await
        }
        Command::Project { command, json } => {
            project_command(command.unwrap_or(ProjectCommand::List), json).await
        }
        Command::Power { command, json } => {
            power_command(command.unwrap_or(PowerCommand::Get { source: None }), json).await
        }
        Command::Session { command, json } => {
            session_command(command.unwrap_or(SessionCommand::Status), json).await
        }
    }
}

async fn session_command(command: SessionCommand, json: bool) -> Result<()> {
    use lege::lifecycle::{Action, LifecycleClient};
    let connection = bus::session().await?;
    let proxy = lege_client::proxies::LifecycleProxy::new(&connection).await?;
    match command {
        SessionCommand::Status => {
            let mut verdicts = Vec::new();
            for action in Action::ALL {
                verdicts.push((action, LifecycleClient::can(&proxy, action).await?));
            }
            let rendered = lege::lifecycle::render_status(&verdicts);
            let structured: Vec<_> = verdicts
                .iter()
                .map(|(action, verdict)| (action.as_str(), verdict.clone()))
                .collect();
            emit(json, &structured, &rendered)
        }
        SessionCommand::Logout => {
            LifecycleClient::logout(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "logout"}),
                "logging out",
            )
        }
        SessionCommand::Reboot => {
            LifecycleClient::reboot(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "reboot"}),
                "restarting",
            )
        }
        SessionCommand::Poweroff => {
            LifecycleClient::power_off(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "poweroff"}),
                "shutting down",
            )
        }
        SessionCommand::Suspend => {
            LifecycleClient::suspend(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "suspend"}),
                "suspending",
            )
        }
        SessionCommand::Lock => {
            LifecycleClient::lock(&proxy).await?;
            emit(json, &serde_json::json!({"session": "lock"}), "locked")
        }
    }
}

async fn app(command: AppCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    match command {
        AppCommand::List => {
            let ids = AppIdentityProxy::new(&connection)
                .await?
                .list_apps()
                .await
                .context("AppIdentity1.ListApps")?;
            emit(json, &ids, &ids.join("\n"))
        }
        AppCommand::Instances => {
            let instances = AppIdentityProxy::new(&connection)
                .await?
                .list_instances()
                .await
                .context("AppIdentity1.ListInstances")?;
            let rows: Vec<Vec<String>> = instances
                .iter()
                .map(|(instance, app, unit, cgroup)| {
                    vec![instance.clone(), app.clone(), unit.clone(), cgroup.clone()]
                })
                .collect();
            emit(json, &instances, &lege::format::table(&rows))
        }
        AppCommand::Launch { app_id, arguments } => {
            let instance = LauncherProxy::new(&connection)
                .await?
                .launch(&app_id, arguments, "")
                .await
                .context("Launcher1.Launch")?;
            emit(json, &instance, &instance)
        }
    }
}

/// `lege open`'s machine-readable shape: the whole resolution chain, plus what was launched.
#[derive(serde::Serialize)]
struct OpenOutput<'a> {
    target: &'a str,
    app_id: &'a str,
    detected_mime: &'a str,
    reason: &'a str,
    explanation: Option<&'a String>,
    launched_instance: Option<String>,
}

async fn open_target(target: &str, explain: bool, project: &str, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let associations = AssociationsProxy::new(&connection).await?;
    let resolution = open::resolve(&associations, target, project, explain).await?;

    if explain {
        let output = OpenOutput {
            target: &resolution.target,
            app_id: &resolution.app_id,
            detected_mime: &resolution.detected_mime,
            reason: &resolution.reason,
            explanation: resolution.explanation.as_ref(),
            launched_instance: None,
        };
        return emit(json, &output, &open::render_explanation(&resolution));
    }

    let mut human = open::render_summary(&resolution);
    let mut launched = None;
    if resolution.is_resolved() {
        let instance = LauncherProxy::new(&connection)
            .await?
            .launch(&resolution.app_id, vec![target.to_owned()], "")
            .await
            .context("Launcher1.Launch")?;
        let _ = write!(human, "\nlaunched instance {instance}");
        launched = Some(instance);
    }
    let output = OpenOutput {
        target: &resolution.target,
        app_id: &resolution.app_id,
        detected_mime: &resolution.detected_mime,
        reason: &resolution.reason,
        explanation: resolution.explanation.as_ref(),
        launched_instance: launched,
    };
    emit(json, &output, &human)
}

async fn res_command(command: ResCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = ResourcesProxy::new(&connection).await?;
    match command {
        ResCommand::Status => {
            let report_json = res::ResourcesClient::report(&proxy).await?;
            match res::parse_report(&report_json) {
                Ok(report) => emit(json, &report, &res::render_report(&report)),
                // Before the first sampling tick the daemon answers `{}` rather than a report.
                Err(_) => emit(
                    json,
                    &serde_json::json!({"status": "no-tick-yet"}),
                    "the governor has not completed a sampling tick yet",
                ),
            }
        }
        ResCommand::Explain { instance_id } => {
            let explanation_json =
                res::ResourcesClient::explain_instance(&proxy, &instance_id).await?;
            match serde_json::from_str::<res::Explanation>(&explanation_json) {
                Ok(explanation) => emit(json, &explanation, &res::render_explanation(&explanation)),
                Err(_) => emit(
                    json,
                    &serde_json::json!({"status": "no-tick-yet", "instance_id": instance_id}),
                    "the governor has not completed a sampling tick yet",
                ),
            }
        }
        ResCommand::SetStage { stage } => {
            res::ResourcesClient::set_stage(&proxy, &stage).await?;
            emit(
                json,
                &serde_json::json!({"stage": stage}),
                &format!("containment stage set to {stage}"),
            )
        }
        ResCommand::Traces { limit } => {
            let traces_json = res::ResourcesClient::recent_traces(&proxy, limit).await?;
            let traces = res::parse_traces(&traces_json);
            emit(json, &traces, &res::render_traces(&traces))
        }
    }
}

async fn files_command(command: FilesCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    match command {
        FilesCommand::Volumes => {
            let proxy = FilesVolumesProxy::new(&connection).await?;
            let volumes = files::parse_volumes(&files::VolumesClient::list(&proxy).await?)?;
            emit(json, &volumes, &files::render_volumes(&volumes))
        }
        FilesCommand::Operations { limit } => {
            let proxy = FilesOperationsProxy::new(&connection).await?;
            let operations =
                files::parse_operations(&files::OperationsClient::recent(&proxy, limit).await?)?;
            emit(json, &operations, &files::render_operations(&operations))
        }
        FilesCommand::Cancel { op_id } => {
            let proxy = FilesOperationsProxy::new(&connection).await?;
            files::OperationsClient::cancel(&proxy, &op_id).await?;
            emit(
                json,
                &serde_json::json!({"cancelled": op_id}),
                &format!("asked {op_id} to stop at its next step boundary"),
            )
        }
        FilesCommand::Undo { op_id } => {
            let proxy = FilesOperationsProxy::new(&connection).await?;
            let (can_undo, reason) = files::OperationsClient::can_undo(&proxy, &op_id).await?;
            if !can_undo {
                // Not an error: "this cannot be undone" is a legitimate, expected answer, and the
                // reason is the whole point of asking first.
                return emit(
                    json,
                    &serde_json::json!({"undone": false, "reason": reason}),
                    &format!("{op_id} cannot be undone: {reason}"),
                );
            }
            let undo_id = files::OperationsClient::undo(&proxy, &op_id).await?;
            emit(
                json,
                &serde_json::json!({"undone": true, "undo_op_id": undo_id}),
                &format!("undoing {op_id} as operation {undo_id}"),
            )
        }
        FilesCommand::Recents { app, limit } => {
            let proxy = FilesRecentsProxy::new(&connection).await?;
            let entries =
                files::parse_recents(&files::RecentsClient::list(&proxy, &app, limit).await?)?;
            emit(json, &entries, &files::render_recents(&entries))
        }
        FilesCommand::Suggest { app, operation } => {
            let proxy = FilesRecentsProxy::new(&connection).await?;
            let suggestion = files::parse_suggestion(
                &files::RecentsClient::suggest_directory(&proxy, &app, &operation).await?,
            )?;
            emit(json, &suggestion, &files::render_suggestion(&suggestion))
        }
        FilesCommand::Search { query, limit } => {
            let proxy = FilesSearchProxy::new(&connection).await?;
            let hits_json = files::SearchClient::search(&proxy, &query, Vec::new(), limit).await?;
            let hits: serde_json::Value =
                serde_json::from_str(&hits_json).context("parsing Files.Search1.Search JSON")?;
            emit(json, &hits, &render_search_hits(&hits))
        }
        FilesCommand::Path { path, form } => {
            let proxy = FilesActionsProxy::new(&connection).await?;
            if let Some(form) = form {
                let text = files::ActionsClient::render(&proxy, &path, &form).await?;
                emit(json, &text, &text)
            } else {
                let forms = files::ActionsClient::forms(&proxy).await?;
                emit(json, &forms, &forms.join("\n"))
            }
        }
    }
}

/// How far one step moves a level.
///
/// Percentage points, and the same for both, because a person pressing a volume key and a person
/// pressing a brightness key are doing the same thing and should not have to learn two different
/// step sizes. Five is small enough to be fine-grained and large enough that reaching either end
/// is a handful of presses rather than twenty.
const STEP_PERCENT: i32 = 5;

/// `lege status` — what the hardware is doing, and the controls for the two halves that have any.
async fn status_command(command: StatusCommand, json: bool) -> Result<()> {
    use lege::status::{AudioClient, BacklightClient, BatteryClient};
    let connection = bus::session().await?;
    match command {
        StatusCommand::All => {
            let audio = lege::status::parse_audio(
                &AudioClient::get(&AudioProxy::new(&connection).await?).await?,
            )?;
            let backlight = lege::status::parse_backlight(
                &BacklightClient::get(&BacklightProxy::new(&connection).await?).await?,
            )?;
            let battery = lege::status::parse_battery(
                &BatteryClient::get(&BatteryProxy::new(&connection).await?).await?,
            )?;
            emit(
                json,
                &serde_json::json!({
                    "audio": audio, "backlight": backlight, "battery": battery
                }),
                &lege::status::render_all(&audio, &backlight, &battery),
            )
        }
        StatusCommand::Volume { set } => {
            let proxy = AudioProxy::new(&connection).await?;
            if let Some(percent) = set {
                AudioClient::set_volume(&proxy, percent).await?;
            }
            let audio = lege::status::parse_audio(&AudioClient::get(&proxy).await?)?;
            emit(json, &audio, &lege::status::render_audio(&audio))
        }
        StatusCommand::Louder | StatusCommand::Quieter => {
            let proxy = AudioProxy::new(&connection).await?;
            let delta = if matches!(command, StatusCommand::Louder) {
                STEP_PERCENT
            } else {
                -STEP_PERCENT
            };
            // One call, so the service does the read-modify-write and two clients on the same key
            // cannot each undo the other.
            let percent = AudioClient::adjust_volume(&proxy, delta).await?;
            emit(
                json,
                &serde_json::json!({"volume_percent": percent}),
                &format!("volume     {percent}%"),
            )
        }
        StatusCommand::Mute => {
            let proxy = AudioProxy::new(&connection).await?;
            let muted = AudioClient::toggle_muted(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"muted": muted}),
                if muted {
                    "volume     muted"
                } else {
                    "volume     unmuted"
                },
            )
        }
        StatusCommand::Brightness { set } => {
            let proxy = BacklightProxy::new(&connection).await?;
            if let Some(percent) = set {
                BacklightClient::set_brightness(&proxy, percent).await?;
            }
            let backlight = lege::status::parse_backlight(&BacklightClient::get(&proxy).await?)?;
            emit(
                json,
                &backlight,
                &lege::status::render_backlight(&backlight),
            )
        }
        StatusCommand::Brighter | StatusCommand::Dimmer => {
            let proxy = BacklightProxy::new(&connection).await?;
            let delta = if matches!(command, StatusCommand::Brighter) {
                STEP_PERCENT
            } else {
                -STEP_PERCENT
            };
            let percent = BacklightClient::adjust_brightness(&proxy, delta).await?;
            emit(
                json,
                &serde_json::json!({"percent": percent}),
                &format!("brightness {percent}%"),
            )
        }
        StatusCommand::Battery => {
            let proxy = BatteryProxy::new(&connection).await?;
            let battery = lege::status::parse_battery(&BatteryClient::get(&proxy).await?)?;
            emit(json, &battery, &lege::status::render_battery(&battery))
        }
    }
}

/// `lege trash` — the trash as a listing, and the three things done with it.
///
/// The two destructive verbs demand `--yes` rather than prompting: this is the surface a script
/// reaches for, and a permanent delete that can happen by pressing return in the wrong terminal
/// is the one mistake in this whole area that nothing can put right.
async fn trash_command(command: TrashCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = FilesTrashProxy::new(&connection).await?;
    match command {
        TrashCommand::List => {
            let entries = files::parse_trash(&files::TrashClient::list(&proxy).await?)?;
            emit(json, &entries, &files::render_trash(&entries))
        }
        TrashCommand::Restore { trashed_paths } => {
            let op_id = files::TrashClient::restore(&proxy, trashed_paths.clone(), "").await?;
            emit(
                json,
                &serde_json::json!({"op_id": op_id, "restoring": trashed_paths}),
                &format!(
                    "restoring {} item(s) as operation {op_id}",
                    trashed_paths.len()
                ),
            )
        }
        TrashCommand::Delete { trashed_paths, yes } => {
            if !yes {
                return refuse_without_yes("lege trash delete");
            }
            let op_id =
                files::TrashClient::delete_permanently(&proxy, trashed_paths.clone(), "").await?;
            emit(
                json,
                &serde_json::json!({"op_id": op_id, "deleting": trashed_paths}),
                &format!(
                    "deleting {} item(s) permanently as operation {op_id}",
                    trashed_paths.len()
                ),
            )
        }
        TrashCommand::Empty { yes } => {
            if !yes {
                return refuse_without_yes("lege trash empty");
            }
            let op_id = files::TrashClient::empty(&proxy, "").await?;
            if op_id.is_empty() {
                // Not an error: an already-empty trash is the state the verb asks for.
                return emit(
                    json,
                    &serde_json::json!({"op_id": "", "emptied": false}),
                    "the trash is already empty",
                );
            }
            emit(
                json,
                &serde_json::json!({"op_id": op_id, "emptied": true}),
                &format!("emptying the trash as operation {op_id}"),
            )
        }
    }
}

fn refuse_without_yes(verb: &str) -> Result<()> {
    anyhow::bail!("{verb} cannot be undone; pass --yes to go ahead")
}

/// `Files.Search1.Search` returns `[[entry, rank], ...]`; render the path and why it matched.
fn render_search_hits(hits: &serde_json::Value) -> String {
    let Some(array) = hits.as_array() else {
        return "no matches".to_owned();
    };
    if array.is_empty() {
        return "no matches".to_owned();
    }
    let rows: Vec<Vec<String>> = array
        .iter()
        .map(|hit| {
            let path = hit
                .get(0)
                .and_then(|entry| entry.get("path"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let rank = hit
                .get(1)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("match");
            vec![path.to_owned(), rank.to_owned()]
        })
        .collect();
    lege::format::table(&rows)
}

/// `lege update deployments`'s machine-readable shape: the backend and every known deployment.
#[derive(serde::Serialize)]
struct DeploymentsOutput<'a> {
    backend: &'a str,
    deployments: &'a [update::Deployment],
}

/// `lege update hardware`'s machine-readable shape: the detected profile and the declared mode.
#[derive(serde::Serialize)]
struct HardwareOutput<'a> {
    profile: &'a update::GraphicsProfile,
    gpu_mode: &'a str,
}

/// `lege update` dispatch: split by `org.legeos.Host.*` sub-interface (one function per
/// interface, per `clippy::too_many_lines`) since one flat match over all of them reads as one
/// interface even though it is five.
async fn update_command(command: UpdateCommand, json: bool) -> Result<()> {
    // The host daemon is on the system bus, unlike everything else this CLI talks to.
    let connection = bus::system().await?;
    match command {
        UpdateCommand::Status | UpdateCommand::Plan { .. } | UpdateCommand::History => {
            update_updates_command(&connection, command, json).await
        }
        UpdateCommand::Deployments
        | UpdateCommand::Stage { .. }
        | UpdateCommand::MarkGood
        | UpdateCommand::Rollback { .. } => {
            update_deployments_command(&connection, command, json).await
        }
        UpdateCommand::Drivers | UpdateCommand::Violations | UpdateCommand::RepairDrivers => {
            update_drivers_command(&connection, command, json).await
        }
        UpdateCommand::Storage { .. }
        | UpdateCommand::EnforceStorage { .. }
        | UpdateCommand::TrimOrder => update_storage_command(&connection, command, json).await,
        UpdateCommand::Hardware | UpdateCommand::SetGpuMode { .. } => {
            update_hardware_command(&connection, command, json).await
        }
    }
}

async fn update_updates_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostUpdatesProxy::new(connection).await?;
    match command {
        UpdateCommand::Status => {
            let current = update::parse_current(&update::UpdatesClient::current(&proxy).await?)?;
            match current {
                Some(transaction) => emit(
                    json,
                    &transaction,
                    &update::render_transaction(&transaction),
                ),
                None => emit(
                    json,
                    &serde_json::json!({"transaction": null}),
                    "no update transaction in flight",
                ),
            }
        }
        UpdateCommand::Plan { changes_json } => {
            let id = update::UpdatesClient::plan(&proxy, &changes_json).await?;
            emit(
                json,
                &serde_json::json!({"transaction_id": id}),
                &format!("planned transaction {id}"),
            )
        }
        UpdateCommand::History => {
            let transactions =
                update::parse_transactions(&update::UpdatesClient::history(&proxy).await?)?;
            emit(
                json,
                &transactions,
                &update::render_transactions(&transactions),
            )
        }
        _ => unreachable!("update_command only routes Updates1 variants here"),
    }
}

async fn update_deployments_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostDeploymentsProxy::new(connection).await?;
    match command {
        UpdateCommand::Deployments => {
            let backend = update::DeploymentsClient::backend(&proxy).await?;
            let deployments =
                update::parse_deployments(&update::DeploymentsClient::list(&proxy).await?)?;
            emit(
                json,
                &DeploymentsOutput {
                    backend: &backend,
                    deployments: &deployments,
                },
                &update::render_deployments(&backend, &deployments),
            )
        }
        UpdateCommand::Stage { id, tries } => {
            update::DeploymentsClient::stage(&proxy, &id, tries).await?;
            emit(
                json,
                &serde_json::json!({"staged": id, "tries": tries}),
                &format!("staged {id} for a trial boot ({tries} tries)"),
            )
        }
        UpdateCommand::MarkGood => {
            update::DeploymentsClient::mark_good(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"marked_good": true}),
                "current deployment marked good",
            )
        }
        UpdateCommand::Rollback { id } => {
            update::DeploymentsClient::rollback(&proxy, &id).await?;
            emit(
                json,
                &serde_json::json!({"rolled_back_to": id}),
                &format!("rolled back to {id}"),
            )
        }
        _ => unreachable!("update_command only routes Deployments1 variants here"),
    }
}

async fn update_drivers_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostDriversProxy::new(connection).await?;
    match command {
        UpdateCommand::Drivers => {
            let plan = update::parse_driver_plan(&update::DriversClient::plan(&proxy).await?)?;
            emit(json, &plan, &update::render_driver_plan(&plan))
        }
        UpdateCommand::Violations => {
            let violations =
                update::parse_violations(&update::DriversClient::violations(&proxy).await?)?;
            let human = if violations.is_empty() {
                "no support violations".to_owned()
            } else {
                violations
                    .iter()
                    .map(update::SupportViolation::describe)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            emit(json, &violations, &human)
        }
        UpdateCommand::RepairDrivers => {
            let lock = update::parse_version_lock(&update::DriversClient::repair(&proxy).await?)?;
            emit(json, &lock, &update::render_version_lock(&lock))
        }
        _ => unreachable!("update_command only routes Drivers1 variants here"),
    }
}

async fn update_storage_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostStorageProxy::new(connection).await?;
    match command {
        UpdateCommand::Storage { mount } => {
            let usage = update::parse_usage(&update::StorageClient::usage(&proxy, &mount).await?)?;
            emit(json, &usage, &update::render_usage(&usage))
        }
        UpdateCommand::EnforceStorage { mount } => {
            let reports =
                update::parse_trim_reports(&update::StorageClient::enforce(&proxy, &mount).await?)?;
            emit(json, &reports, &update::render_trim_reports(&reports))
        }
        UpdateCommand::TrimOrder => {
            let order = update::StorageClient::trim_order(&proxy).await?;
            emit(json, &order, &order.join(" → "))
        }
        _ => unreachable!("update_command only routes Storage1 variants here"),
    }
}

async fn update_hardware_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostHardwareProxy::new(connection).await?;
    match command {
        UpdateCommand::Hardware => {
            let profile = update::parse_graphics_profile(
                &update::HardwareClient::graphics_profile(&proxy).await?,
            )?;
            let gpu_mode = update::HardwareClient::gpu_mode(&proxy).await?;
            let human = update::render_graphics_profile(&profile, &gpu_mode);
            emit(
                json,
                &HardwareOutput {
                    profile: &profile,
                    gpu_mode: &gpu_mode,
                },
                &human,
            )
        }
        UpdateCommand::SetGpuMode { mode } => {
            update::HardwareClient::set_gpu_mode(&proxy, &mode).await?;
            emit(
                json,
                &serde_json::json!({"gpu_mode": mode}),
                &format!("GPU mode set to {mode}"),
            )
        }
        _ => unreachable!("update_command only routes Hardware1 variants here"),
    }
}

async fn history_command(command: HistoryCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = HistoryProxy::new(&connection).await?;
    match command {
        HistoryCommand::Recent { limit } => {
            let events =
                history::parse_events(&history::HistoryClient::recent(&proxy, limit).await?);
            emit(json, &events, &history::render_events(&events))
        }
        HistoryCommand::Search { query, limit } => {
            let events = history::parse_events(
                &history::HistoryClient::search(&proxy, &query, limit).await?,
            );
            emit(json, &events, &history::render_events(&events))
        }
        HistoryCommand::Undo { event_id } => {
            let undone = history::HistoryClient::undo(&proxy, &event_id).await?;
            emit(
                json,
                &serde_json::json!({"undone": undone, "event_id": event_id}),
                if undone {
                    "undone"
                } else {
                    "nothing was undone (the event is not reversible, or already was)"
                },
            )
        }
    }
}

async fn providers_command(command: ProvidersCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = ProvidersProxy::new(&connection).await?;
    match command {
        ProvidersCommand::Report => {
            let raw = providers::ProvidersClient::report(&proxy).await?;
            let parsed: Vec<(String, providers::Resolution)> = raw
                .iter()
                .map(|(role, resolution_json)| {
                    Ok((role.clone(), providers::parse_resolution(resolution_json)?))
                })
                .collect::<Result<Vec<_>>>()?;
            emit(json, &raw, &providers::render_report(&parsed))
        }
        ProvidersCommand::Select { role, app_id } => {
            // Fail here rather than at the daemon: a typo'd role is the likeliest mistake, and
            // the valid set is known locally.
            if !ProviderRole::ALL.iter().any(|r| r.as_str() == role) {
                let valid: Vec<&str> = ProviderRole::ALL.iter().map(|r| r.as_str()).collect();
                anyhow::bail!(
                    "unknown provider role {role:?}; expected one of {}",
                    valid.join(", ")
                );
            }
            providers::ProvidersClient::select(&proxy, &role, &app_id).await?;
            emit(
                json,
                &serde_json::json!({"role": role, "app_id": app_id}),
                &format!("{app_id} is now the selected provider for {role}"),
            )
        }
    }
}

async fn settings_command(command: SettingsCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = SettingsProxy::new(&connection).await?;
    match command {
        SettingsCommand::List => {
            let values = settings::SettingsClient::list(&proxy).await?;
            emit(json, &values, &settings::render_list(&values))
        }
        SettingsCommand::Schema => {
            let schema = settings::SettingsClient::schema(&proxy).await?;
            emit(json, &schema, &settings::render_schema(&schema))
        }
        SettingsCommand::Get { key } => {
            let value_json = settings::SettingsClient::get(&proxy, &key).await?;
            emit(json, &value_json, &settings::render_value(&value_json))
        }
        SettingsCommand::Set {
            key,
            value_json,
            origin,
        } => {
            settings::SettingsClient::set(&proxy, &key, &value_json, &origin).await?;
            emit(
                json,
                &serde_json::json!({"key": key, "value": value_json}),
                &format!("{key} = {}", settings::render_value(&value_json)),
            )
        }
        SettingsCommand::Reset { key, origin } => {
            settings::SettingsClient::reset(&proxy, &key, &origin).await?;
            emit(
                json,
                &serde_json::json!({"reset": key}),
                &format!("{key} reset to its schema default"),
            )
        }
        SettingsCommand::Changes { limit } => {
            let changes =
                settings::parse_changes(&settings::SettingsClient::changes(&proxy, limit).await?);
            emit(json, &changes, &settings::render_changes(&changes))
        }
    }
}

async fn project_command(command: ProjectCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = ProjectsProxy::new(&connection).await?;
    match command {
        ProjectCommand::List => {
            let projects = project::ProjectsClient::list(&proxy).await?;
            emit(json, &projects, &project::render_list(&projects))
        }
        ProjectCommand::Create {
            project_id,
            name,
            root,
        } => {
            project::ProjectsClient::create(&proxy, &project_id, &name, &root).await?;
            emit(
                json,
                &serde_json::json!({"created": project_id}),
                &format!("created project {project_id}"),
            )
        }
        ProjectCommand::Open { project_id } => {
            project::ProjectsClient::open(&proxy, &project_id).await?;
            emit(
                json,
                &serde_json::json!({"opened": project_id}),
                &format!("opened project {project_id}"),
            )
        }
        ProjectCommand::Close { project_id } => {
            project::ProjectsClient::close(&proxy, &project_id).await?;
            emit(
                json,
                &serde_json::json!({"closed": project_id}),
                &format!("closed project {project_id}"),
            )
        }
        ProjectCommand::Remove { project_id } => {
            project::ProjectsClient::remove(&proxy, &project_id).await?;
            emit(
                json,
                &serde_json::json!({"removed": project_id}),
                &format!("removed project {project_id}"),
            )
        }
        other => project_detail_command(&proxy, other, json).await,
    }
}

/// The three project subcommands that read or edit one project's semantics, split out so
/// `project_command` stays a short list of named steps.
async fn project_detail_command(
    proxy: &ProjectsProxy<'_>,
    command: ProjectCommand,
    json: bool,
) -> Result<()> {
    match command {
        ProjectCommand::Show { project_id } => {
            let payload = project::ProjectsClient::describe(proxy, &project_id).await?;
            let current = project::ProjectsClient::get_current(proxy)
                .await
                .unwrap_or_default();
            let view = project::parse_project(&payload)?;
            let rendered = project::render_show(&view, current == project_id);
            // `--json` hands back the daemon's own payload verbatim; the human form never does.
            if json {
                println!("{payload}");
                Ok(())
            } else {
                println!("{rendered}");
                Ok(())
            }
        }
        ProjectCommand::Set {
            project_id,
            key,
            value,
        } => {
            // `state` has its own method, because archiving is a transition and not a field edit.
            if key == "state" {
                project::ProjectsClient::set_state(proxy, &project_id, &value).await?;
            } else {
                let changes = project::changes_json(&key, &value)?;
                project::ProjectsClient::update(proxy, &project_id, &changes).await?;
            }
            emit(
                json,
                &serde_json::json!({"project": project_id, "set": key, "to": value}),
                &format!("{project_id}: {key} is now {value}"),
            )
        }
        ProjectCommand::Current { project_id } => match project_id {
            None => {
                let current = project::ProjectsClient::get_current(proxy).await?;
                emit(
                    json,
                    &serde_json::json!({"current": current}),
                    &if current.is_empty() {
                        "no current project".to_owned()
                    } else {
                        current.clone()
                    },
                )
            }
            Some(project_id) => {
                let target = if project_id == "none" {
                    String::new()
                } else {
                    project_id
                };
                project::ProjectsClient::set_current(proxy, &target).await?;
                emit(
                    json,
                    &serde_json::json!({"current": target}),
                    &if target.is_empty() {
                        "cleared the current project".to_owned()
                    } else {
                        format!("working in {target}")
                    },
                )
            }
        },
        _ => unreachable!("project_command handles the other subcommands itself"),
    }
}

async fn power_command(command: PowerCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = PowerProxy::new(&connection).await?;
    match command {
        PowerCommand::Get { source } => {
            let sources: Vec<String> = match source {
                Some(source) => vec![source],
                None => vec!["battery".to_owned(), "mains".to_owned()],
            };
            let mut profiles = Vec::new();
            let mut human = Vec::new();
            for source in &sources {
                let profile_json = power::PowerClient::get_profile(&proxy, source).await?;
                let profile = power::parse_profile(&profile_json)?;
                human.push(power::render_profile(source, &profile));
                profiles.push((source.clone(), profile));
            }
            emit(json, &profiles, &human.join("\n"))
        }
        PowerCommand::Reset { source } => {
            power::PowerClient::reset_profile(&proxy, &source).await?;
            emit(
                json,
                &serde_json::json!({"reset": source}),
                &format!("{source} profile reset to the distribution default"),
            )
        }
    }
}
