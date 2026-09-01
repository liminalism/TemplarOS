//! `lege-comp`: the compositor, written directly against Smithay (ADR-0012). See
//! `compositor/COMPOSITOR.md` for the event-to-`lege_window`-call contract every module here
//! implements, and `src/state.rs` for how the pieces fit together.

mod backend;
mod chrome;
mod cursor;
mod foreign_toplevel;
mod handlers;
mod input;
mod keybindings;
mod keymap;
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

    /// Exact public Wayland socket name. The installed session uses `legeos-0`; direct and
    /// nested development runs omit this and receive the first available `wayland-N` name.
    #[arg(long, value_name = "NAME")]
    socket_name: Option<String>,
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

    // Say which build is running, for `lege dev status`. Diagnostic only: a stamp that cannot be
    // written must never keep lege-comp from starting.
    if let Err(error) = lege_build::write_stamp("lege-comp") {
        tracing::warn!(%error, "could not write the build stamp");
    }
    tracing::info!(?backend, "starting lege-comp");
    let exit_code = match backend {
        BackendKind::Winit => backend::winit::run(exec.as_deref(), cli.socket_name.as_deref()),
        BackendKind::Udev => backend::udev::run(exec.as_deref(), cli.socket_name.as_deref()),
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

#[cfg(test)]
mod tests {
    #[test]
    fn every_graphical_backend_notifies_only_after_a_frame_reaches_the_display() {
        for source in [
            include_str!("backend/udev.rs"),
            include_str!("backend/winit.rs"),
        ] {
            assert_eq!(source.matches("crate::notify_ready_once(").count(), 1);
            assert!(!source.contains("insert_idle(|_| crate::notify_ready"));
        }
        let udev = include_str!("backend/udev.rs");
        let submitted = udev
            .find("frame_submitted()")
            .expect("hardware backend observes completed scanout");
        let udev_notify = udev[submitted..]
            .find("crate::notify_ready_once(")
            .expect("hardware backend notifies after completed scanout");
        assert!(udev_notify > 0);

        let winit = include_str!("backend/winit.rs");
        let submitted = winit
            .find("graphics.submit(")
            .expect("nested backend submits a host frame");
        let winit_notify = winit[submitted..]
            .find("crate::notify_ready_once(")
            .expect("nested backend notifies after host-frame submission");
        assert!(winit_notify > 0);
    }

    #[test]
    fn every_graphical_backend_flushes_clients_after_dispatch() {
        for source in [
            include_str!("backend/udev.rs"),
            include_str!("backend/winit.rs"),
        ] {
            let run = source
                .find("event_loop.run(None")
                .expect("backend enters calloop");
            let flush = source[run..]
                .find("flush_clients()")
                .expect("backend flushes queued Wayland replies after dispatch");
            assert!(flush > 0);
        }
        assert!(include_str!("backend/udev.rs").contains("window.send_frame("));
    }

    #[test]
    fn mapping_consumes_the_trusted_shell_surface_claims() {
        let mapping = include_str!("xdg_shell.rs");
        assert!(mapping.contains(
            "crate::protocols_shell::apply_surface_claims(self, surface, window_id, requested)"
        ));
        let protocol = include_str!("protocols_shell.rs");
        assert!(protocol.contains("shell_surface_placement(layer, exclusive, &output, requested)"));
        assert!(protocol.contains("pending.size = Some((geometry.width, geometry.height).into())"));
    }
}
