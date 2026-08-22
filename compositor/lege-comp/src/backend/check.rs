//! `lege-comp --check`: construct the entire compositor without a display, report what it
//! advertises, and exit.
//!
//! This is not a debug aid either — it is the only part of `lege-comp` that can run in CI and on a
//! build machine, where there is no seat, no DRM device and no host compositor to nest inside. The
//! winit backend needs a `WAYLAND_DISPLAY`/`DISPLAY` to open a window in and the udev backend
//! needs a real seat, so on a headless machine neither backend can start at all and the entire
//! crate would otherwise be untested outside a developer's session.
//!
//! What this proves is narrow but real, and it is the half that breaks silently: that every
//! protocol global constructs and registers without conflicting, that the two private protocols
//! are advertised, that the public and trusted-shell sockets bind, and that `WindowService` comes
//! up with the outputs it is given. What it cannot prove is anything involving pixels or input —
//! rendering, damage, direct scanout, and every interaction — which stay the nested and
//! on-hardware checks in `docs/testing/` and are not claimed here.

use crate::state::{Backend, LegeCompState};
use lege_comp_integration::{DbusIdentityCache, GovernorNotifier, ShellStateBroadcaster};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

const SEAT_NAME: &str = "check";

/// Backend data for the self-check: there is no backend, which is the point.
pub struct CheckBackendData;

impl Backend for CheckBackendData {
    fn seat_name(&self) -> String {
        SEAT_NAME.to_owned()
    }
}

type State = LegeCompState<CheckBackendData>;

/// Build everything, print a report, and return without entering the event loop.
///
/// With `exec`, it goes one step further and actually runs that client under the compositor until
/// it exits, returning its exit code. That makes the `--exec` path — including its shutdown
/// handshake, which had a real race in it — testable on a machine with no display, no seat and no
/// host compositor, which is the only kind CI has.
pub fn run(exec: Option<&[String]>) -> anyhow::Result<i32> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    // These reach for the session bus, which need not exist here: each degrades to a disconnected
    // cache/notifier rather than failing, exactly as it does when sessiond has not started yet.
    let identity_cache = DbusIdentityCache::spawn(&handle);
    let governor = GovernorNotifier::spawn(&handle);
    let shell_sink = crate::shell_sink::install(&event_loop.handle());
    let shell_broadcaster = ShellStateBroadcaster::spawn(&handle, shell_sink);

    let socket_name = crate::state::init_wayland_listener(display, &event_loop.handle())?;

    let mut state = State::new(
        CheckBackendData,
        SEAT_NAME,
        &dh,
        event_loop.handle(),
        event_loop.get_signal(),
        identity_cache,
        governor,
        shell_broadcaster,
    )?;
    State::create_private_globals(&dh);
    let shell_socket_name = crate::shell_socket::install(&mut state, &socket_name)?;

    let session_command = exec
        .map(|argv| {
            crate::session_command::SessionCommand::spawn(argv, &socket_name, &event_loop.handle())
        })
        .transpose()?;

    // One dispatch turn: any global registered twice, or a calloop source inserted twice, fails
    // here rather than looking fine until the first client connects.
    event_loop.dispatch(Some(std::time::Duration::from_millis(0)), &mut state)?;
    state.display_handle.flush_clients()?;

    println!("lege-comp self-check");
    println!("  wayland socket:       {socket_name}");
    println!("  trusted-shell socket: {shell_socket_name}");
    println!("  seat:                 {}", state.seat_name);
    println!(
        "  windows:              {}",
        state.window_service.windows().count()
    );
    println!(
        "  outputs:              {}",
        state.window_service.outputs().len()
    );
    println!("  private protocols:    lege_shell_v1, lege_recovery_v1");
    println!(
        "  shell-only globals:   ext_foreign_toplevel_image_capture_source_manager_v1, \
         ext_image_copy_capture_manager_v1"
    );

    if let Some(command) = session_command {
        println!("  running --exec client until it exits");
        event_loop.run(None, &mut state, |_| {})?;
        let code = command.shut_down();
        println!("ok (--exec client exited {code})");
        return Ok(code);
    }

    println!("ok");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

    /// Collects the registry's `global` events — the compositor's advertised protocol set, as a
    /// client actually sees it on the wire.
    #[derive(Default)]
    struct Globals {
        names: Vec<String>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
        fn event(
            state: &mut Self,
            _registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _connection: &Connection,
            _queue: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, .. } = event {
                state.names.push(interface);
            }
        }
    }

    /// A client connects to the compositor and sees every protocol it is supposed to.
    ///
    /// The point of doing this over a socket with a real `wayland-client` rather than by
    /// inspecting our own `State` is that only the wire tells the truth: a global that is
    /// constructed but never registered, or registered against the wrong interface name or
    /// version, looks perfectly correct from inside the compositor and is invisible to a client.
    /// This is also the only test in the crate that runs a client at all, so it is what stands
    /// behind `--check`'s claim that the compositor "comes up".
    ///
    /// It needs no display, no seat and no session bus, which is what lets it run in CI.
    #[test]
    fn a_client_connects_and_sees_every_advertised_protocol() {
        let (socket_tx, socket_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        // The compositor is not `Send`, so it is built inside its own thread and only its socket
        // name crosses back.
        let server = std::thread::spawn(move || {
            let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
            let display: Display<State> = Display::new().unwrap();
            let dh = display.handle();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let handle = runtime.handle().clone();
            let identity_cache = DbusIdentityCache::spawn(&handle);
            let governor = GovernorNotifier::spawn(&handle);
            let shell_sink = crate::shell_sink::install(&event_loop.handle());
            let shell_broadcaster = ShellStateBroadcaster::spawn(&handle, shell_sink);
            let socket_name =
                crate::state::init_wayland_listener(display, &event_loop.handle()).unwrap();
            let mut state = State::new(
                CheckBackendData,
                SEAT_NAME,
                &dh,
                event_loop.handle(),
                event_loop.get_signal(),
                identity_cache,
                governor,
                shell_broadcaster,
            )
            .unwrap();
            State::create_private_globals(&dh);
            let shell_socket_name = crate::shell_socket::install(&mut state, &socket_name).unwrap();
            socket_tx.send((socket_name, shell_socket_name)).unwrap();

            while stop_rx.try_recv().is_err() {
                event_loop
                    .dispatch(Some(std::time::Duration::from_millis(20)), &mut state)
                    .unwrap();
                state.display_handle.flush_clients().unwrap();
            }
        });

        let (socket_name, shell_socket_name) =
            socket_rx.recv().expect("compositor failed to start");

        // Connect by path rather than by exporting WAYLAND_DISPLAY: `std::env::set_var` is
        // `unsafe` in Rust 2024 because it races every other thread reading the environment, and
        // this test has a compositor running in one.
        let runtime_dir = std::path::PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set"),
        );
        let connect = |name: &str| {
            let path = runtime_dir.join(name);
            let stream = std::os::unix::net::UnixStream::connect(&path)
                .unwrap_or_else(|error| panic!("connecting to {}: {error}", path.display()));
            Connection::from_socket(stream)
                .expect("client could not speak Wayland on the compositor's socket")
        };
        let registry_of = |connection: &Connection| {
            let mut queue = connection.new_event_queue();
            connection.display().get_registry(&queue.handle(), ());
            let mut globals = Globals::default();
            queue.roundtrip(&mut globals).expect("registry roundtrip");
            globals.names
        };

        let ordinary = connect(&socket_name);
        let advertised = registry_of(&ordinary);

        // Everything LegeOS's own documents promise an ordinary client. A protocol dropped from
        // `LegeCompState::new` fails here rather than at a user's first attempt to use it.
        //
        // `wl_output` is deliberately absent: outputs are created by the backend (one synthetic
        // output for winit, one per connector for udev), and the self-check backend has none.
        for required in [
            "wl_compositor",
            "wl_shm",
            "wl_seat",
            "xdg_wm_base",
            "zxdg_decoration_manager_v1",
            "xdg_activation_v1",
            "xdg_wm_dialog_v1",
            // ADR-0010's compositor-owned lock screen and the shell's toplevel listing.
            "ext_session_lock_manager_v1",
            "ext_foreign_toplevel_list_v1",
            "wp_viewporter",
            "wp_fractional_scale_manager_v1",
            "wp_presentation",
        ] {
            assert!(
                advertised.iter().any(|name| name == required),
                "compositor did not advertise {required}; it advertised: {advertised:?}"
            );
        }

        // The security boundary, from the outside. `lege_shell_v1` can shelve windows, activate
        // them and declare surfaces into privileged layers; `lege_recovery_v1` drives the
        // pressure overlay. Both are gated by `can_view` on having connected over the private
        // socket, so an ordinary client must not even learn they exist — a global it cannot see
        // is one it cannot attempt to bind.
        for private in ["lege_shell_manager_v1", "lege_recovery_manager_v1"] {
            assert!(
                !advertised.iter().any(|name| name == private),
                "{private} was advertised to an ordinary client; the trusted-shell gate is open"
            );
        }

        // ...and from the inside: connecting on the private socket is the one thing that grants
        // trust, so the same globals must appear there. Without this half, deleting
        // `create_private_globals` entirely would still pass the assertions above.
        let shell = connect(&shell_socket_name);
        let shell_advertised = registry_of(&shell);
        for private in ["lege_shell_manager_v1", "lege_recovery_manager_v1"] {
            assert!(
                shell_advertised.iter().any(|name| name == private),
                "the trusted shell did not see {private}; it saw: {shell_advertised:?}"
            );
        }

        drop(shell);
        drop(ordinary);
        stop_tx.send(()).ok();
        server.join().expect("compositor thread panicked");
    }
}
