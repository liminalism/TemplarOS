//! `templar-comp`: the compositor, written directly against Smithay (ADR-0012). See
//! `compositor/COMPOSITOR.md` for the event-to-`templar_window`-call contract every module here
//! implements, and `src/state.rs` for how the pieces fit together.

mod backend;
mod chrome;
mod cursor;
mod flip_recovery;
mod foreign_toplevel;
mod gesture_gate;
mod handlers;
mod idle;
mod input;
mod keybindings;
mod keymap;
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
mod test_input;
mod watchdog;
mod window_bridge;
mod window_groups;
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

    /// Private test input socket. Available only with an explicitly selected nested backend.
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
        // The claim configures the client's own area, which for a shell surface is the whole
        // placement: the frame is only ever subtracted in `space_sync::client_area`.
        assert!(protocol.contains("let client = crate::space_sync::client_area(state, window_id)"));
        assert!(protocol.contains("pending.size = Some((client.width, client.height).into())"));
    }

    /// The bar's 80/48 oscillation: two separate faults, both of which have to stay fixed.
    ///
    /// The compositor configured a 48px bar to be 80px tall (a tag strip added to a surface that
    /// has no frame), and then adopted every catch-up commit as a new choice, so the client and
    /// the compositor chased each other at frame rate. Comparing the placement to the current
    /// geometry cannot detect this — the placement really does alternate — so the ack check is
    /// load-bearing, not belt-and-braces. Asserted at the call sites because both need a live
    /// client to exercise; the behavioural check is a nested run (`RUNBOOK-lane-1.md` A1).
    #[test]
    fn a_shell_placed_surface_is_neither_framed_nor_chased() {
        let mapping = include_str!("xdg_shell.rs");
        // (a) A surface the shell placed itself is requested at its bare committed size.
        assert!(mapping.contains(
            "let requested = if crate::protocols_shell::has_surface_claim(self, surface) {"
        ));
        // (b) Both branches of adopt_client_size consult the same acknowledgement check, so
        // neither can be relaxed on its own.
        assert_eq!(
            mapping.matches("has_acked_last_configure(surface)").count(),
            2,
            "the zone branch and the ordinary branch both consult it"
        );
        assert!(mapping.contains("fn has_acked_last_configure(surface: &WlSurface) -> bool"));
        // The zone branch's own condition, verbatim: a committed size is adopted only when it
        // differs *and* the client has acknowledged what it was last sent.
        assert!(mapping.contains(
            "if (committed.w, committed.h) != (geometry.width, geometry.height)\n                && has_acked_last_configure(surface)"
        ));
    }

    /// K2, the compositor half of the session lock. The three guards that make `locked` mean
    /// locked: nothing behind the lock surface can be pointed at, typed into or dragged. Asserted
    /// at the call sites because the branches themselves need a live display to exercise — the
    /// behaviour is checked by hand in `RUNBOOK-lane-1.md`.
    #[test]
    fn nothing_behind_the_lock_surface_can_be_reached() {
        let input = include_str!("input.rs");
        // The one hit-test every pointer path routes through resolves lock surfaces only.
        let hit_test = input
            .find("pub fn surface_under(")
            .expect("the shared hit-test exists");
        let guard = input[hit_test..]
            .find("if self.session_lock.locked {")
            .expect("the shared hit-test is gated on the lock");
        let fallthrough = input[hit_test..]
            .find("self.space\n            .element_under(pos)")
            .expect("the ordinary path is still there");
        assert!(
            guard < fallthrough,
            "windows are resolved before the lock is consulted"
        );
        // Presses and recovery keybindings were already refused; the cursor is too. The press
        // path's guard returns `false` — "the compositor did not take this press" — rather than
        // bare, so match the head of the branch rather than the whole of it.
        assert!(
            input.contains("if self.session_lock.locked {\n            return false;\n        }")
        );
        assert!(input.contains("if self.session_lock.locked || self.pointer_gesture_active()"));

        let lock = include_str!("session_lock.rs");
        assert!(
            lock.contains("keyboard.set_focus(self, Some(focus), SERIAL_COUNTER.next_serial())"),
            "a lock surface takes the keyboard when it appears"
        );
        assert!(
            lock.contains("crate::input::refocus_after_reveal(self, !any_left)"),
            "unlocking hands the keyboard back rather than leaving it on a dead surface"
        );
    }

    /// The other half of the bar-flicker fix: `WindowService::on_outputs_changed` hands back
    /// client windows only, so the one topology entry point must re-place shell surfaces itself
    /// or a bar stops following its edge across a hotplug.
    #[test]
    fn a_topology_change_re_places_every_shell_surface_claim() {
        let backend = include_str!("backend/mod.rs");
        assert!(
            backend.contains("let repaired = state.window_service.on_outputs_changed(outputs)")
        );
        assert!(backend.contains("crate::protocols_shell::reapply_surface_claims(state)"));
        // One path, not two: changing a zone goes through `recompute_outputs` like every other
        // topology change rather than re-placing its own surface behind its back.
        let protocol = include_str!("protocols_shell.rs");
        assert!(protocol.contains("crate::backend::recompute_outputs(state);\n}"));
    }
}
