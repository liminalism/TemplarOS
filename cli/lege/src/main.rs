//! `lege` — one command line over the OS.

use anyhow::Result;
use clap::{Parser, Subcommand};
use lege::doctor;
use lege_client::proxies::{AppIdentityProxy, AssociationsProxy, LauncherProxy};
use std::path::PathBuf;

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
    },
    /// Open a file or URI through the association broker (never bypasses it).
    Open {
        /// Path or URI.
        target: String,
        /// Explain the decision without launching.
        #[arg(long)]
        explain: bool,
    },
    /// Resource governor status.
    Res,
    /// File service (volumes, operations, recents).
    Files,
    /// Host updates and deployments.
    Update,
    /// System-wide change history.
    History {
        /// Search text.
        query: Option<String>,
    },
    /// Exclusive-role providers.
    Providers,
    /// Typed settings.
    Settings,
    /// Project sessions.
    Project,
    /// Power policy.
    Power,
    /// Integration checks (runtime, image root, or hardware matrix).
    Doctor {
        /// Check an unpacked image root instead of `/`.
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Include hardware-matrix checks.
        #[arg(long)]
        hardware: bool,
        /// JSON output.
        #[arg(long)]
        json: bool,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor {
            root,
            hardware,
            json,
        } => {
            let findings = doctor::run_all(&doctor::Context { root, hardware });
            if json {
                println!("{}", serde_json::to_string_pretty(&findings)?);
            } else {
                for f in &findings {
                    println!(
                        "{:<8} {:<20} {}{}",
                        format!("{:?}", f.outcome).to_lowercase(),
                        f.id,
                        f.summary,
                        if f.detail.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", f.detail)
                        }
                    );
                }
            }
            std::process::exit(doctor::exit_code(&findings));
        }
        Command::App { command } => {
            let connection = zbus::Connection::session().await?;
            match command {
                AppCommand::List => {
                    for id in AppIdentityProxy::new(&connection)
                        .await?
                        .list_apps()
                        .await?
                    {
                        println!("{id}");
                    }
                }
                AppCommand::Instances => {
                    for (instance, app, unit, cgroup) in AppIdentityProxy::new(&connection)
                        .await?
                        .list_instances()
                        .await?
                    {
                        println!("{instance}  {app}  {unit}  {cgroup}");
                    }
                }
                AppCommand::Launch { app_id, arguments } => {
                    let instance = LauncherProxy::new(&connection)
                        .await?
                        .launch(&app_id, arguments, "")
                        .await?;
                    println!("{instance}");
                }
            }
        }
        Command::Open { target, explain } => {
            let connection = zbus::Connection::session().await?;
            let (app, mime, reason) = AssociationsProxy::new(&connection)
                .await?
                .resolve_file(&target, "")
                .await?;
            println!(
                "{target}: {mime} → {} ({reason})",
                if app.is_empty() { "ask" } else { &app }
            );
            if !explain && !app.is_empty() {
                let instance = LauncherProxy::new(&connection)
                    .await?
                    .launch(&app, vec![target], "")
                    .await?;
                println!("launched instance {instance}");
            }
        }
        other => {
            println!(
                "`lege {}` is not implemented yet in this milestone",
                subcommand_name(&other)
            );
        }
    }
    Ok(())
}

fn subcommand_name(command: &Command) -> &'static str {
    match command {
        Command::App { .. } => "app",
        Command::Open { .. } => "open",
        Command::Res => "res",
        Command::Files => "files",
        Command::Update => "update",
        Command::History { .. } => "history",
        Command::Providers => "providers",
        Command::Settings => "settings",
        Command::Project => "project",
        Command::Power => "power",
        Command::Doctor { .. } => "doctor",
    }
}
