//! `templar-resourced` — observe-only by default (`crate::policy::Stage::ObserveOnly`).

use anyhow::{Context, Result};
use clap::Parser;
use std::{path::PathBuf, time::Duration};
use templar_resourced::{config, daemon, policy::GovernorConfig, replay};

/// TemplarOS resource governor.
#[derive(Debug, Parser)]
#[command(name = "templar-resourced", version, about)]
struct Cli {
    /// Sampling interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,

    /// Run one tick, print the report, and exit — no D-Bus, no persistence (smoke test / `templar
    /// doctor`). Must succeed on a machine with no TemplarOS units at all.
    #[arg(long)]
    once: bool,

    /// Threshold/weight overrides (default: `$XDG_CONFIG_HOME/templaros/resourced.toml`).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Where the stage setting and trace ring live (default: `$XDG_STATE_HOME/templaros/resources`).
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Replay recorded traces through today's policy and print what changed. Takes a
    /// `traces.jsonl` file or a directory of them. Reads only; touches no cgroup and no bus.
    #[arg(long, value_name = "PATH")]
    replay: Option<PathBuf>,

    /// Rewrite the replayed traces' recorded level, victim and actions to what today's policy
    /// decides. For committing a *deliberate* policy change after its diff has been read — never
    /// for making a surprising diff go away.
    #[arg(long, requires = "replay")]
    bless: bool,
}

fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let cli = Cli::parse();

    if let Some(path) = &cli.replay {
        return replay_traces(path, cli.bless);
    }

    if cli.once {
        return daemon::run_once();
    }

    let config_path = cli.config.unwrap_or_else(config::default_path);
    let state_dir = cli.state_dir.unwrap_or_else(daemon::default_state_dir);
    tracing::info!(
        config = %config_path.display(),
        state_dir = %state_dir.display(),
        "templar-resourced starting",
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

/// `--replay`: run recorded pressure back through the current policy and report the difference.
///
/// Exits non-zero when anything diverged, so a replay can gate a change the way a test does. The
/// output is the form a commit message quotes: one line per trace, one line per divergent record.
fn replay_traces(path: &std::path::Path, bless: bool) -> Result<()> {
    if bless {
        let files = if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("reading {}", path.display()))?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect();
            files.sort();
            files
        } else {
            vec![path.to_path_buf()]
        };
        for file in files {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let blessed = replay::bless(&text, GovernorConfig::default());
            std::fs::write(&file, blessed)
                .with_context(|| format!("writing {}", file.display()))?;
            println!("blessed {}", file.display());
        }
        return Ok(());
    }

    let reports = replay::replay_path(path, GovernorConfig::default())
        .with_context(|| format!("replaying {}", path.display()))?;
    let mut diverged = 0;
    for report in &reports {
        print!("{}", report.render());
        if !report.agrees() {
            diverged += 1;
        }
    }
    if diverged == 0 {
        println!("\n{} trace(s) replay identically", reports.len());
        Ok(())
    } else {
        anyhow::bail!("{diverged} of {} trace(s) diverge", reports.len())
    }
}
