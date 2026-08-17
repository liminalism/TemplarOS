//! `lege-session` — what `wayland-sessions/legeos.desktop` executes.
//!
//! systemd-managed session (systemd DESKTOP_ENVIRONMENTS convention): import the display-manager
//! environment into the user manager, start `legeos-session.target`, wait until the compositor
//! unit stops, then stop the target. The compositor is `lege-comp.service`; the shell is
//! restartable independently.

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::process::Command;

const TARGET: &str = "legeos-session.target";
const COMPOSITOR_UNIT: &str = "lege-comp.service";

/// LegeOS session launcher.
#[derive(Debug, Parser)]
#[command(name = "lege-session", version, about)]
struct Cli {
    /// Show what would be run without starting anything.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    let steps: Vec<Vec<&str>> = vec![
        vec![
            "systemctl",
            "--user",
            "import-environment",
            "XDG_SESSION_TYPE",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_SEAT",
            "XDG_VTNR",
        ],
        // Autostart is arbitrated by sessiond (one provider per role); mask the generic runner.
        vec![
            "systemctl",
            "--user",
            "mask",
            "--runtime",
            "xdg-desktop-autostart.target",
        ],
        vec!["systemctl", "--user", "start", "--wait", TARGET],
    ];

    for step in &steps {
        tracing::info!(command = ?step, "session step");
        if cli.dry_run {
            continue;
        }
        let status = Command::new(step[0])
            .args(&step[1..])
            .status()
            .with_context(|| format!("running {step:?}"))?;
        if !status.success() && step[2] != "mask" {
            bail!("{step:?} failed with {status}");
        }
    }

    if !cli.dry_run {
        // The target stopped (compositor exited or logout): make sure everything under it is down.
        let _ = Command::new("systemctl")
            .args(["--user", "stop", TARGET])
            .status();
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "unmask",
                "--runtime",
                "xdg-desktop-autostart.target",
            ])
            .status();
        tracing::info!(unit = COMPOSITOR_UNIT, "session ended");
    }
    Ok(())
}
