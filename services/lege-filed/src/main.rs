//! `lege-filed` — file service daemon (`org.legeos.Files`).

use anyhow::{Context, Result};
use clap::Parser;
use lege_filed::storage;
use lege_types::dbus::files as names;
use std::{future::pending, path::Path};
use zbus::{connection, fdo};

/// LegeOS file service.
#[derive(Debug, Parser)]
#[command(name = "lege-filed", version, about)]
struct Cli {
    /// Root-volume reserve as a percentage of its size.
    #[arg(long, default_value_t = 5)]
    reserve_percent: u64,

    /// Print volume information and exit.
    #[arg(long)]
    dry_run: bool,
}

/// `org.legeos.Files.Volumes1`
#[derive(Debug)]
struct VolumesApi {
    reserve_percent: u64,
}

#[zbus::interface(name = "org.legeos.Files.Volumes1")]
impl VolumesApi {
    /// Volume information for the volume containing `path`, as JSON.
    async fn info_for_path(&self, path: &str) -> fdo::Result<String> {
        let info = storage::query(Path::new(path), self.reserve_percent)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        serde_json::to_string(&info).map_err(|error| fdo::Error::Failed(error.to_string()))
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

    if cli.dry_run {
        let info = storage::query(Path::new("/"), cli.reserve_percent)?;
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    let _connection = connection::Builder::session()?
        .name(names::BUS_NAME)?
        .serve_at(
            names::ROOT_PATH,
            VolumesApi {
                reserve_percent: cli.reserve_percent,
            },
        )?
        .build()
        .await
        .context("claiming org.legeos.Files")?;

    tracing::info!(name = names::BUS_NAME, "lege-filed ready");
    pending::<()>().await;
    Ok(())
}
