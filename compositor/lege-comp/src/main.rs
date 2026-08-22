//! `lege-comp`: the compositor, written directly against Smithay (ADR-0012). See
//! `compositor/COMPOSITOR.md` for the event-to-`lege_window`-call contract every module here
//! implements, and `src/state.rs` for how the pieces fit together.

mod backend;
mod chrome;
mod foreign_toplevel;
mod handlers;
mod input;
mod keybindings;
mod protocols;
mod protocols_recovery;
mod protocols_shell;
mod screencopy;
mod session_command;
mod session_lock;
mod shell_sink;
mod shell_socket;
mod space_sync;
mod state;
mod window_bridge;
mod xdg_shell;
mod xwayland;

use clap::{Parser, ValueEnum};

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum BackendKind {
    /// Nested inside an existing session — development, CI, and how this crate is tested with no
    /// display hardware at all (COMPOSITOR.md).
    Winit,
    /// Real hardware via `LibSeatSession`/DRM/KMS.
    Udev,
}

#[derive(Debug, Parser)]
#[command(name = "lege-comp", version, about)]
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
    /// `lege-comp --exec lege-greeter`, and when the greeter finishes, the compositor goes with
    /// it — which is what greetd(5) requires before it will start the user's session. Everything
    /// after `--exec` is the command and its arguments.
    #[arg(long, num_args = 1.., allow_hyphen_values = true, value_name = "COMMAND")]
    exec: Vec<String>,

    /// Which backend to run. Defaults to udev when neither `WAYLAND_DISPLAY` nor `DISPLAY` is set
    /// (starting the session compositor from a bare TTY), winit otherwise (already nested inside
    /// a Wayland/X11 session) — overridable either way ([`default_backend`]).
    #[arg(long, value_enum)]
    backend: Option<BackendKind>,
}

fn main() -> anyhow::Result<()> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

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

    tracing::info!(?backend, "starting lege-comp");
    let exit_code = match backend {
        BackendKind::Winit => backend::winit::run(exec.as_deref()),
        BackendKind::Udev => backend::udev::run(exec.as_deref()),
    }?;
    // The --exec child's exit code is this process's, so whatever started us sees what happened to
    // the thing it actually asked for.
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
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
