//! `templar-session` — what `wayland-sessions/templaros.desktop` executes.
//!
//! systemd-managed session (systemd DESKTOP_ENVIRONMENTS convention): replace any stale display
//! environment in the user manager, start `templaros-session.target`, wait until the compositor
//! unit stops, then stop the target. The compositor is `templar-comp.service`; the shell is
//! restartable independently.

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::process::{Child, Command};

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
    // greetd's live [initial_session] launches this binary without the wayland-session
    // desktop file, so XDG_CURRENT_DESKTOP is unset and `import-environment` would
    // ignore it. Set the session identity here; import only the seat/VT greetd does own.
    Step {
        program: "systemctl",
        args: &[
            "--user",
            "set-environment",
            "XDG_CURRENT_DESKTOP=TemplarOS",
            "XDG_SESSION_DESKTOP=templaros",
            "XDG_SESSION_TYPE=wayland",
            "WAYLAND_DISPLAY=wayland-0",
        ],
    },
    Step {
        program: "systemctl",
        args: &["--user", "import-environment", "XDG_SEAT", "XDG_VTNR"],
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
        args: &["--user", "start", TARGET],
    },
];

const WAIT_STEP: Step = Step {
    program: "systemctl",
    args: &["--user", "start", "--wait", TARGET],
};

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
        for step in START_STEPS
            .iter()
            .copied()
            .chain(desktop_helpers())
            .chain([WAIT_STEP])
            .chain(CLEANUP_STEPS.iter().copied())
        {
            tracing::info!(program = step.program, args = ?step.args, "session step");
        }
        return Ok(());
    }

    let mut helpers = Vec::new();
    let result = run_steps(START_STEPS).and_then(|()| {
        // Polkit's agent registers a UnixSession subject from its PID. A user-manager service
        // has no PAM session, so these children must remain under this greetd-launched process.
        // The target start has already waited for the compositor's Type=notify readiness.
        for step in desktop_helpers() {
            match Command::new(step.program)
                .args(step.args)
                .env_remove("DISPLAY")
                .env("WAYLAND_DISPLAY", WAYLAND_DISPLAY)
                .env("QT_QPA_PLATFORM", "wayland")
                .spawn()
            {
                Ok(child) => helpers.push(child),
                Err(error) => tracing::error!(%error, program = step.program, "desktop helper could not start"),
            }
        }
        run_step(WAIT_STEP)
    });
    stop_helpers(&mut helpers);
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

fn desktop_helpers() -> [Step; 2] {
    let arguments: &'static [&'static str] =
        if std::path::Path::new("/etc/templaros/udiskie.json").is_file() {
            &[
                "--automount",
                "--no-tray",
                "--no-notify",
                "--no-file-manager",
                "--no-password-prompt",
                "--config",
                "/etc/templaros/udiskie.json",
            ]
        } else {
            &[
                "--automount",
                "--no-tray",
                "--no-notify",
                "--no-file-manager",
                "--no-password-prompt",
                "--config",
                "/usr/share/templaros/udiskie/config.json",
            ]
        };
    [
        Step {
            program: "/usr/bin/lxqt-policykit-agent",
            args: &[],
        },
        Step {
            program: "/usr/bin/udiskie",
            args: arguments,
        },
    ]
}

fn stop_helpers(children: &mut [Child]) {
    for child in children {
        // Exact children only, never process-name matching or a live-session unit restart.
        let _ = child.kill();
        let _ = child.wait();
    }
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
