//! `templar-session` — what `wayland-sessions/templaros.desktop` executes.
//!
//! systemd-managed session (systemd DESKTOP_ENVIRONMENTS convention): replace any stale display
//! environment in the user manager, start `templaros-session.target`, wait until the compositor
//! unit stops, then stop the target. The compositor is `templar-comp.service`; the shell is
//! restartable independently.

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{os::unix::fs::FileTypeExt, path::{Component, Path, PathBuf}, process::{Child, Command}};

const TARGET: &str = "templaros-session.target";
const COMPOSITOR_UNIT: &str = "templar-comp.service";

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
            "QT_QPA_PLATFORMTHEME=gtk3",
        ],
    },
    Step {
        program: "systemctl",
        args: &["--user", "import-environment", "XDG_SEAT", "XDG_VTNR"],
    },
    // Processes retain their original display connection even after activation
    // environment changes. Retire selected backends from the previous desktop;
    // these remain activation-driven, not eagerly started here.
    Step {
        program: "systemctl",
        args: &["--user", "stop", "xdg-desktop-portal.service", "xdg-desktop-portal-gtk.service",
            "xdg-desktop-portal-lxqt.service", "xdg-desktop-portal-templar.service"],
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
        args: &["--user", "unset-environment", "WAYLAND_DISPLAY", "QT_QPA_PLATFORMTHEME"],
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
    use std::io::IsTerminal as _;
    // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();

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
    let result = (|| {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from).context("XDG_RUNTIME_DIR is required for compositor readiness")?;
        let ready_path = runtime.join("templaros/compositor-display.json");
        match std::fs::remove_file(&ready_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("clearing stale compositor readiness"),
        }
        run_steps(START_STEPS)?;
        let wayland_display = ready_display(&runtime)?;
        // Polkit's agent registers a UnixSession subject from its PID. A user-manager service
        // has no PAM session, so these children must remain under this greetd-launched process.
        // The target start has already waited for the compositor's Type=notify readiness.
        for step in desktop_helpers() {
            match Command::new(step.program)
                .args(step.args)
                .env_remove("DISPLAY")
                .env("WAYLAND_DISPLAY", &wayland_display)
                .env("QT_QPA_PLATFORM", "wayland")
                .env("QT_QPA_PLATFORMTHEME", "gtk3")
                .spawn()
            {
                Ok(child) => helpers.push(child),
                Err(error) => tracing::error!(%error, program = step.program, "desktop helper could not start"),
            }
        }
        run_step(WAIT_STEP)
    })();
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

#[derive(serde::Deserialize)]
struct DisplayReady {
    schema: u32,
    compositor_pid: u32,
    socket: String,
}

fn ready_display(runtime: &Path) -> Result<String> {
    let ready: DisplayReady = serde_json::from_slice(
        &std::fs::read(runtime.join("templaros/compositor-display.json"))
            .context("reading actual ready compositor display")?,
    ).context("decoding compositor display readiness")?;
    validate_ready_name(ready.schema, ready.compositor_pid, &ready.socket)?;
    let process = PathBuf::from(format!("/proc/{}/comm", ready.compositor_pid));
    let name = std::fs::read_to_string(process).context("ready compositor process no longer exists")?;
    if name.trim() != "templar-comp" {
        bail!("display readiness PID is not a templar compositor");
    }
    if !std::fs::symlink_metadata(runtime.join(&ready.socket))?.file_type().is_socket() {
        bail!("ready compositor display is not a Unix socket");
    }
    Ok(ready.socket)
}

fn validate_ready_name(schema: u32, pid: u32, socket: &str) -> Result<()> {
    let mut components = Path::new(socket).components();
    if schema != 1 || pid == 0 || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some() {
        bail!("invalid compositor display readiness schema, PID or socket basename");
    }
    Ok(())
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
#[cfg(test)]
mod activation_tests {
    use super::*;
    #[test]
    fn readiness_uses_one_actual_socket_basename() {
        assert!(validate_ready_name(1, 42, "wayland-7").is_ok());
        for socket in ["", "/run/user/1000/wayland-0", "../wayland-0", ".", "nested/wayland-0"] {
            assert!(validate_ready_name(1, 42, socket).is_err());
        }
        assert!(validate_ready_name(2, 42, "wayland-7").is_err());
        assert!(validate_ready_name(1, 0, "wayland-7").is_err());
    }
}
