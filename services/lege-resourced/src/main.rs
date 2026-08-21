//! `lege-resourced` — observe-only by default (`crate::policy::Stage::ObserveOnly`).

use anyhow::{Context, Result};
use clap::Parser;
use lege_resourced::{config, daemon};
use std::{path::PathBuf, time::Duration};

/// LegeOS resource governor.
#[derive(Debug, Parser)]
#[command(name = "lege-resourced", version, about)]
struct Cli {
    /// Sampling interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,

    /// Run one tick, print the report, and exit — no D-Bus, no persistence (smoke test / `lege
    /// doctor`). Must succeed on a machine with no LegeOS units at all.
    #[arg(long)]
    once: bool,

    /// Threshold/weight overrides (default: `$XDG_CONFIG_HOME/legeos/resourced.toml`).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Where the stage setting and trace ring live (default: `$XDG_STATE_HOME/legeos/resources`).
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    if cli.once {
        return daemon::run_once();
    }

    let config_path = cli.config.unwrap_or_else(config::default_path);
    let state_dir = cli.state_dir.unwrap_or_else(daemon::default_state_dir);
    tracing::info!(
        config = %config_path.display(),
        state_dir = %state_dir.display(),
        "lege-resourced starting",
    );

    // A small, dedicated runtime: the D-Bus connection and the tokio task that bridges signals
    // out of the sampling loop are the only async work this process does (ADR-0002 — the
    // sampling loop itself is a plain thread, never scheduled on this runtime).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;

    runtime.block_on(daemon::serve(
        Duration::from_millis(cli.interval_ms),
        &config_path,
        &state_dir,
    ))
}
