//! `lege-settings` — one settings application over `org.legeos.Settings1`, `Providers1`,
//! `Associations1`, `History1` (and hostd for privileged keys). Every page maps to a D-Bus
//! interface; nothing is a private implementation detail. UI: libcosmic (added with the first page).

use clap::Parser;

/// LegeOS Settings.
#[derive(Debug, Parser)]
#[command(name = "lege-settings", version, about)]
struct Cli {
    /// Open a specific page (`providers`, `associations`, `background`, `resources`, `history`, `hardware`).
    #[arg(long)]
    page: Option<String>,
}

fn main() {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();
    tracing::info!(page = ?cli.page, "lege-settings skeleton: no UI yet (see apps/lege-settings/README.md)");
}
