//! `lege-settings` — one settings application over `org.legeos.Settings1`, `Providers1`,
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
mod hardware;
mod history;
mod names;
mod providers;
mod resources;
mod surface;
mod system;

use clap::Parser;
use lege_types::ProviderRole;

/// LegeOS Settings.
#[derive(Debug, Parser)]
#[command(name = "lege-settings", version, about)]
struct Cli {
    /// Render a page to stdout instead of opening the graphical application.
    #[arg(long)]
    print: bool,

    /// Open a specific page: `system`, `providers`, `associations`, `background`, `resources`,
    /// `history`, `hardware`. Omit to list the available pages instead of connecting to anything.
    #[arg(long)]
    page: Option<String>,

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

const PAGES: &[&str] = &[
    "system",
    "providers",
    "associations",
    "background",
    "resources",
    "history",
    "hardware",
];

fn main() {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    if !cli.print {
        if let Err(error) = surface::run() {
            eprintln!("lege-settings: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    let runtime = tokio::runtime::Runtime::new().expect("settings print runtime");
    runtime.block_on(print_page(cli));
}

async fn print_page(cli: Cli) {
    let Some(page) = cli.page.as_deref() else {
        println!(
            "lege-settings: pass --page <name> to view one. Pages: {}.",
            PAGES.join(", ")
        );
        return;
    };

    let output = match page {
        "hardware" => hardware::render().await,
        "system" | "providers" | "associations" | "background" | "resources" | "history" => {
            let connection = match zbus::Connection::session().await {
                Ok(connection) => connection,
                Err(error) => {
                    println!("service unavailable: could not reach the session bus ({error})");
                    return;
                }
            };
            match page {
                "system" => system::render(&connection).await,
                "providers" => {
                    let role = cli.role.as_deref().and_then(|name| {
                        ProviderRole::ALL
                            .into_iter()
                            .find(|role| role.as_str() == name)
                    });
                    providers::render(&connection, role).await
                }
                "associations" => {
                    associations::render(&connection, cli.target.as_deref(), &cli.project).await
                }
                "background" => background::render(&connection).await,
                "resources" => resources::render(&connection, cli.limit).await,
                "history" => {
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
        other => {
            println!(
                "lege-settings: unknown page {other:?}. Pages: {}.",
                PAGES.join(", ")
            );
            return;
        }
    };
    println!("{output}");
}
