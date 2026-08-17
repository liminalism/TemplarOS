//! `lege-hostd` — the one privileged policy daemon (ADR-0003). System bus `org.legeos.Host`,
//! polkit actions `org.legeos.host.*`.

use anyhow::{Context, Result};
use clap::Parser;
use lege_types::dbus::host as names;
use std::future::pending;
use zbus::connection;

mod deploy;
mod drivers;
mod hardware;
mod storage;
mod updates;

/// LegeOS host daemon.
#[derive(Debug, Parser)]
#[command(name = "lege-hostd", version, about)]
struct Cli {
    /// Print the detected hardware profile and exit.
    #[arg(long)]
    probe: bool,
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
        let profile = hardware::probe()?;
        println!("{}", serde_json::to_string_pretty(&profile)?);
        return Ok(());
    }

    let _connection = connection::Builder::system()?
        .name(names::BUS_NAME)?
        .serve_at(names::ROOT_PATH, hardware::HardwareApi)?
        .serve_at(names::ROOT_PATH, drivers::DriversApi)?
        .serve_at(names::ROOT_PATH, updates::UpdatesApi::default())?
        .serve_at(names::ROOT_PATH, storage::StorageApi)?
        .serve_at(
            names::ROOT_PATH,
            deploy::DeploymentsApi {
                backend: deploy::detect_backend(std::path::Path::new("/")),
                known: Vec::new(),
            },
        )?
        .build()
        .await
        .context("claiming org.legeos.Host on the system bus")?;

    tracing::info!(name = names::BUS_NAME, "lege-hostd ready");
    pending::<()>().await;
    Ok(())
}
