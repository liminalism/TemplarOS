//! `templar-comp`: the compositor, written directly against Smithay (ADR-0012). See
//! `compositor/COMPOSITOR.md` for the event-to-`templar_window`-call contract every module here
//! implements, and `src/state.rs` for how the pieces fit together.

mod backend;
mod chrome;
mod compositor_control;
mod cursor;
mod display_control;
mod essence_renderer;
mod flip_recovery;
mod foreign_toplevel;
mod gesture_gate;
mod handlers;
mod identity_sink;
mod idle;
mod input;
mod input_control;
mod keybindings;
mod keymap;
mod launcher_input;
mod motion;
mod protocols;
mod protocols_recovery;
mod protocols_shell;
mod scene;
mod screencopy;
mod session_command;
mod session_lock;
mod shell_sink;
mod shell_socket;
mod space_sync;
mod state;
mod tab_metadata;
mod test_input;
mod viewport;
mod wallpaper;
mod watchdog;
mod window_bridge;
mod window_groups;
mod xdg_foreign;
mod xdg_shell;
mod xwayland;

use clap::{Parser, ValueEnum};
use std::io::IsTerminal;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum BackendKind {
    /// Nested inside an existing session — development, CI, and how this crate is tested with no
    /// display hardware at all (COMPOSITOR.md).
    Winit,
    /// Real hardware via `LibSeatSession`/DRM/KMS.
    Udev,
}

#[derive(Debug, Parser)]
#[command(name = "templar-comp", version, about)]
struct Cli {
    /// Construct the compositor, report what it advertises, and exit without opening a display.
    ///
    /// The one mode that runs on a machine with no seat and no host compositor, which is where CI
    /// and package builds happen (`src/backend/check.rs` says what it does and does not prove).
    #[arg(long)]
    check: bool,

    /// Run one client under this compositor and exit when it exits, with `WAYLAND_DISPLAY`
    /// already set in its environment.
    ///
    /// This is how the greeter is started (`session/units/greetd/config.toml`): greetd runs
    /// `templar-comp --exec templar-greeter`, and when the greeter finishes, the compositor goes with
    /// it — which is what greetd(5) requires before it will start the user's session. Everything
    /// after `--exec` is the command and its arguments.
    #[arg(long, num_args = 1.., allow_hyphen_values = true, value_name = "COMMAND")]
    exec: Vec<String>,

    /// Which backend to run. Defaults to udev when neither `WAYLAND_DISPLAY` nor `DISPLAY` is set
    /// (starting the session compositor from a bare TTY), winit otherwise (already nested inside
    /// a Wayland/X11 session) — overridable either way ([`default_backend`]).
    #[arg(long, value_enum)]
    backend: Option<BackendKind>,

    /// Exact public Wayland socket name. The installed session uses `wayland-0`; direct and
    /// nested development runs omit this and receive the first available `wayland-N` name.
    ///
    /// Public sockets are always `wayland-N`, because that is the only name a confined
    /// application is permitted to open (snapd's wayland interface allows
    /// `owner /run/user/[0-9]*/wayland-[0-9]* rw,`). The trusted-shell socket derives from this
    /// one but is deliberately outside that pattern — see `src/shell_socket.rs`.
    #[arg(long, value_name = "NAME")]
    socket_name: Option<String>,

    /// Publish this installed session's actual listener to both activation owners.
    #[arg(long, requires = "backend", conflicts_with = "check")]
    session_activation: bool,

    /// Private nested input/scene fixture socket. Also exposes the sibling same-UID-only
    /// templar-wallpaper-fixture-control.sock for real FD/import/presentation smoke tests;
    /// production control authentication is unchanged. Only explicitly selected winit permits it.
    #[arg(long, value_name = "PATH", requires = "backend")]
    test_input_socket: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let exec = (!cli.exec.is_empty()).then_some(cli.exec);
    if cli.check {
        let code = backend::check::run(exec.as_deref())?;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    let backend = cli.backend.unwrap_or_else(default_backend);
    anyhow::ensure!(
        cli.test_input_socket.is_none() || backend == BackendKind::Winit,
        "test input is restricted to the nested winit backend"
    );
    anyhow::ensure!(
        !cli.session_activation || backend == BackendKind::Udev,
        "session activation publication is restricted to the installed udev backend"
    );

    // Say which build is running, for `templar dev status`. Diagnostic only: a stamp that cannot be
    // written must never keep templar-comp from starting.
    if let Err(error) = templar_build::write_stamp("templar-comp") {
        tracing::warn!(%error, "could not write the build stamp");
    }
    tracing::info!(?backend, "starting templar-comp");
    let exit_code = match backend {
        BackendKind::Winit => backend::winit::run(
            exec.as_deref(),
            cli.socket_name.as_deref(),
            cli.test_input_socket.as_deref(),
        ),
        BackendKind::Udev => backend::udev::run(
            exec.as_deref(),
            cli.socket_name.as_deref(),
            cli.session_activation,
        ),
    }?;
    // The --exec child's exit code is this process's, so whatever started us sees what happened to
    // the thing it actually asked for.
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Complete systemd's `Type=notify` start once a backend has presented its first frame. Failure is
/// expected outside the installed unit (there is no `NOTIFY_SOCKET`) and is not fatal.
fn notify_ready_once(already_notified: &mut bool) {
    if *already_notified {
        return;
    }
    if let Err(error) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(%error, "sd_notify READY failed (not running under systemd?)");
    }
    *already_notified = true;
    tracing::info!("first frame presented; compositor is ready");
}

/// Nested (winit) whenever we can already see a host compositor/X server to nest inside;
/// otherwise assume we are the session compositor and want real hardware.
fn default_backend() -> BackendKind {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
        BackendKind::Winit
    } else {
        BackendKind::Udev
    }
}
