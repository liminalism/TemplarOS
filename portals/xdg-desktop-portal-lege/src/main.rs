//! `xdg-desktop-portal-lege` — one backend, all clients of the same services:
//! FileChooser → `org.legeos.Files` (shared last-directory policy), AppChooser/OpenURI →
//! `org.legeos.Associations1`, Background → providers/autostart, Settings → `Settings1`.

use anyhow::{Context, Result};
use clap::Parser;
use std::future::pending;
use zbus::connection;

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.legeos";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// LegeOS portal backend.
#[derive(Debug, Parser)]
#[command(name = "xdg-desktop-portal-lege", version, about)]
struct Cli {
    /// Claim the name and idle without serving interfaces (smoke test).
    #[arg(long)]
    idle: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let _cli = Cli::parse();
    let _connection = connection::Builder::session()?
        .name(BUS_NAME)?
        .build()
        .await
        .context("claiming the portal backend name")?;
    tracing::info!(
        name = BUS_NAME,
        "portal backend skeleton ready (no interfaces yet)"
    );
    pending::<()>().await;
    Ok(())
}
