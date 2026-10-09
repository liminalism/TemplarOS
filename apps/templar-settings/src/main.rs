//! `templar-settings` — one settings application over `org.templaros.Settings1`, `Providers1`,
//! `Associations1`, `History1`, `Resources1` (and `Host.Hardware1` for the one privileged page).
//! Every page maps to a D-Bus interface; nothing is a private implementation detail. UI: this
//! The normal entry point is graphical. `--print` preserves the deterministic text renderer for
//! terminals, diagnostics and integration tests.
//!
//! Every page's real work — parsing the service's exact wire JSON, cross-referencing sources,
//! rendering a sentence a person can read — is a pure, unit-tested function behind a small trait
//! per D-Bus interface (`crate::providers::ProvidersSource` and friends); `main` only does the
//! connecting and the printing.

mod associations;
mod background;
mod bluetooth;
mod datetime;
mod displays;
mod files;
mod garden;
mod hardware;
mod history;
mod input;
mod names;
mod network;
mod notifications;
mod power;
mod providers;
mod resources;
mod sound;
mod surface;
mod system;
mod wallpaper;

use clap::Parser;
use templar_types::{ProviderRole, SettingsPage};

/// TemplarOS Settings.
#[derive(Debug, Parser)]
#[command(name = "templar-settings", version, about)]
struct Cli {
    /// Render a page to stdout instead of opening the graphical application.
    #[arg(long)]
    print: bool,

    /// Open a canonical Settings page. Omit for System, or to list pages with --print.
    #[arg(long)]
    page: Option<SettingsPage>,

    /// providers: show only this role (e.g. `file-chooser`).
    #[arg(long)]
    role: Option<String>,

    /// associations: resolve this path or URI instead of only listing the global defaults.
    #[arg(long)]
    target: Option<String>,

    /// associations: the open project's ID, for project-scoped resolution.
    #[arg(long, default_value = "")]
    project: String,

    /// history: search text; omit for the most recent events.
    #[arg(long)]
    query: Option<String>,

    /// history/resources: how many events or traces to show.
    #[arg(long, default_value_t = 20)]
    limit: u32,

    /// history: undo one event by ID.
    #[arg(long)]
    undo: Option<String>,
}

fn main() {
    use std::io::IsTerminal as _;
    // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();

    if !cli.print {
        if let Err(error) = surface::run(cli.page.unwrap_or(SettingsPage::System)) {
            eprintln!("templar-settings: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    let runtime = tokio::runtime::Runtime::new().expect("settings print runtime");
    runtime.block_on(print_page(cli));
}

async fn print_page(cli: Cli) {
    let Some(page) = cli.page else {
        println!(
            "templar-settings: pass --page <name> to view one. Pages: {}.",
            SettingsPage::ALL.map(SettingsPage::slug).join(", ")
        );
        return;
    };

    let output = match page {
        SettingsPage::Hardware => hardware::render().await,
        SettingsPage::Sound => sound::render().await,
        SettingsPage::DateTime => datetime::print().await,
        SettingsPage::Files => {
            if let Err(error) = files::print().await {
                println!("Index unavailable — {error:#}");
            }
            return;
        }
        SettingsPage::Network => {
            if let Err(error) = network::print().await {
                println!("Network unavailable — {error:#}");
            }
            return;
        }
        SettingsPage::Bluetooth => {
            if let Err(error) = bluetooth::print().await {
                println!("Bluetooth unavailable — {error:#}");
            }
            return;
        }
        SettingsPage::Displays => {
            if let Err(error) = displays::print().await {
                println!("Displays unavailable — {error:#}");
            }
            return;
        }
        SettingsPage::Input => {
            if let Err(error) = input::print().await {
                println!("Input unavailable — {error:#}");
            }
            return;
        }
        SettingsPage::Appearance
        | SettingsPage::HomeField
        | SettingsPage::Homegarden
        | SettingsPage::Rails
        | SettingsPage::Windows => {
            if page == SettingsPage::Appearance {
                match wallpaper::print().await {
                    Ok(status) => println!("{status}"),
                    Err(error) => println!("Wallpaper unavailable — {error:#}"),
                }
            }
            let prefix = match page {
                SettingsPage::Appearance => "shell.appearance",
                SettingsPage::HomeField => "shell.home-field",
                SettingsPage::Homegarden => "shell.home-garden",
                SettingsPage::Rails => "shell.rails",
                SettingsPage::Windows => "windows.tabs",
                _ => unreachable!(),
            };
            match zbus::Connection::session().await {
                Ok(connection) => {
                    match templar_client::proxies::SettingsProxy::new(&connection).await {
                        Ok(proxy) => match proxy.list().await {
                            Ok(values) => values
                                .into_iter()
                                .filter(|(key, _)| key.starts_with(prefix))
                                .map(|(key, value)| format!("{key} = {value}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            Err(error) => format!("Settings unavailable: {error}"),
                        },
                        Err(error) => format!("Settings unavailable: {error}"),
                    }
                }
                Err(error) => format!("Session bus unavailable: {error}"),
            }
        }
        SettingsPage::System
        | SettingsPage::Providers
        | SettingsPage::Associations
        | SettingsPage::Background
        | SettingsPage::Resources
        | SettingsPage::History => {
            let connection = match zbus::Connection::session().await {
                Ok(connection) => connection,
                Err(error) => {
                    println!("service unavailable: could not reach the session bus ({error})");
                    return;
                }
            };
            match page {
                SettingsPage::System => {
                    println!("{}", system::render(&connection).await);
                    if let Err(error) = power::print().await {
                        println!("Power policy unavailable — {error:#}");
                    }
                    if let Err(error) = notifications::print().await {
                        println!("Notifications policy unavailable — {error:#}");
                    }
                    return;
                }
                SettingsPage::Providers => {
                    let role = cli.role.as_deref().and_then(|name| {
                        ProviderRole::ALL
                            .into_iter()
                            .find(|role| role.as_str() == name)
                    });
                    providers::render(&connection, role).await
                }
                SettingsPage::Associations => {
                    associations::render(&connection, cli.target.as_deref(), &cli.project).await
                }
                SettingsPage::Background => background::render(&connection).await,
                SettingsPage::Resources => resources::render(&connection, cli.limit).await,
                SettingsPage::History => {
                    history::render(
                        &connection,
                        cli.query.as_deref(),
                        cli.limit,
                        cli.undo.as_deref(),
                    )
                    .await
                }
                _ => unreachable!("matched above"),
            }
        }
    };
    println!("{output}");
}
