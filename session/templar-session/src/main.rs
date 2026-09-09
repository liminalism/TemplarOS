//! `templar-session` — what `wayland-sessions/templaros.desktop` executes.
//!
//! systemd-managed session (systemd DESKTOP_ENVIRONMENTS convention): replace any stale display
//! environment in the user manager, start `templaros-session.target`, wait until the compositor
//! unit stops, then stop the target. The compositor is `templar-comp.service`; the shell is
//! restartable independently.

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::process::Command;

const TARGET: &str = "templaros-session.target";
const COMPOSITOR_UNIT: &str = "templar-comp.service";
const WAYLAND_DISPLAY: &str = "wayland-0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Step {
    program: &'static str,
    args: &'static [&'static str],
}

const START_STEPS: &[Step] = &[
    // A user manager can survive logout.  Remove the previous desktop's socket names before
    // importing this login's non-display environment, otherwise templar-comp may select its nested
    // backend and try to connect to a compositor that no longer exists.
    Step {
        program: "systemctl",
        args: &["--user", "unset-environment", "DISPLAY", "WAYLAND_DISPLAY"],
    },
    Step {
        program: "systemctl",
        args: &[
            "--user",
            "import-environment",
            "XDG_SESSION_TYPE",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "XDG_SEAT",
            "XDG_VTNR",
        ],
    },
    Step {
        program: "systemctl",
        args: &["--user", "set-environment", "WAYLAND_DISPLAY=wayland-0"],
    },
    // Autostart is arbitrated by sessiond (one provider per role); mask the generic runner.
    Step {
        program: "systemctl",
        args: &[
            "--user",
            "mask",
            "--runtime",
            "xdg-desktop-autostart.target",
        ],
    },
    Step {
        program: "systemctl",
        args: &["--user", "start", "--wait", TARGET],
    },
];

const CLEANUP_STEPS: &[Step] = &[
    Step {
        program: "systemctl",
        args: &["--user", "stop", TARGET],
    },
    Step {
        program: "systemctl",
        args: &[
            "--user",
            "unmask",
            "--runtime",
            "xdg-desktop-autostart.target",
        ],
    },
    Step {
        program: "systemctl",
        args: &["--user", "unset-environment", "WAYLAND_DISPLAY"],
    },
];

/// TemplarOS session launcher.
#[derive(Debug, Parser)]
#[command(name = "templar-session", version, about)]
struct Cli {
    /// Show what would be run without starting anything.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    debug_assert_eq!(WAYLAND_DISPLAY, "wayland-0");
    if cli.dry_run {
        for step in START_STEPS {
            tracing::info!(program = step.program, args = ?step.args, "session step");
        }
        return Ok(());
    }

    let result = run_steps(START_STEPS);
    // Cleanup also runs after a failed start.  That is what returns SDDM to a usable greeter when
    // the compositor cannot take the seat, rather than stranding the user on an empty VT.
    for step in CLEANUP_STEPS {
        if let Err(error) = run_step(*step) {
            tracing::warn!(%error, program = step.program, args = ?step.args, "session cleanup failed");
        }
    }
    tracing::info!(unit = COMPOSITOR_UNIT, "session ended");
    result
}

fn run_steps(steps: &[Step]) -> Result<()> {
    for step in steps {
        run_step(*step)?;
    }
    Ok(())
}

fn run_step(step: Step) -> Result<()> {
    tracing::info!(program = step.program, args = ?step.args, "session step");
    let status = Command::new(step.program)
        .args(step.args)
        .status()
        .with_context(|| format!("running {} {:?}", step.program, step.args))?;
    if !status.success() {
        bail!("{} {:?} failed with {status}", step.program, step.args);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_replaces_stale_display_environment_before_starting_target() {
        assert_eq!(
            START_STEPS[0].args,
            ["--user", "unset-environment", "DISPLAY", "WAYLAND_DISPLAY"]
        );
        assert_eq!(
            START_STEPS[2].args,
            ["--user", "set-environment", "WAYLAND_DISPLAY=wayland-0"]
        );
        assert!(!START_STEPS[1].args.contains(&"DISPLAY"));
        assert!(!START_STEPS[1].args.contains(&"WAYLAND_DISPLAY"));
    }

    #[test]
    fn installed_units_share_one_socket_and_bind_session_lifetime_to_compositor() {
        let compositor = include_str!("../../units/user/templar-comp.service");
        let shell = include_str!("../../units/user/templar-shell.service");
        let target = include_str!("../../units/user/templaros-session.target");

        assert!(compositor.contains("--backend udev --socket-name wayland-0"));
        assert!(compositor.contains("Type=notify"));
        assert!(compositor.contains("TimeoutStartSec=15s"));
        assert!(shell.contains("Environment=WAYLAND_DISPLAY=templaros-shell-wayland-0"));
        assert!(shell.contains("ExecStart=/usr/bin/templar-shell --surface"));
        assert!(target.contains("Requires=basic.target templar-comp.service"));
        assert!(target.contains("BindsTo=templar-comp.service"));
    }

    /// snapd's wayland interface allows a confined application exactly
    /// `owner /run/user/[0-9]*/wayland-[0-9]* rw,`. The public socket has to fall inside that or
    /// no snap can open the display; the trusted-shell socket has to fall outside it or every
    /// snap can open the one socket that grants the private protocol. AppArmor's `*` does not
    /// stop at a dash, so "wayland-0" plus any suffix is inside the pattern — which is why the
    /// shell's name is prefixed rather than suffixed.
    #[test]
    fn snaps_may_reach_the_public_socket_and_never_the_trusted_shell_one() {
        fn snap_reachable(name: &str) -> bool {
            name.strip_prefix("wayland-")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        }

        assert!(snap_reachable(WAYLAND_DISPLAY), "{WAYLAND_DISPLAY}");
        let shell = include_str!("../../units/user/templar-shell.service");
        let name = shell
            .lines()
            .find_map(|line| line.strip_prefix("Environment=WAYLAND_DISPLAY="))
            .expect("the shell unit names its socket");
        assert!(!snap_reachable(name), "{name} is reachable by every snap");
    }

    #[test]
    fn cleanup_removes_the_session_socket_from_a_persistent_user_manager() {
        assert!(
            CLEANUP_STEPS
                .iter()
                .any(|step| { step.args == ["--user", "unset-environment", "WAYLAND_DISPLAY"] })
        );
    }
}
