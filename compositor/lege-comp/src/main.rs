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
    let backend = cli.backend.unwrap_or_else(default_backend);

    tracing::info!(?backend, "starting lege-comp");
    match backend {
        BackendKind::Winit => backend::winit::run(),
        BackendKind::Udev => backend::udev::run(),
    }
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
