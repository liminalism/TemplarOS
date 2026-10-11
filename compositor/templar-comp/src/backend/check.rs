//! `templar-comp --check`: construct the entire compositor without a display, report what it
//! advertises, and exit.
//!
//! This is not a debug aid either — it is the only part of `templar-comp` that can run in CI and on a
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

use crate::state::{Backend, TemplarCompState};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use templar_comp_integration::{
    DbusIdentityCache, GovernorNotifier, IdleNotifier, ShellStateBroadcaster,
};

const SEAT_NAME: &str = "check";

/// Backend data for the self-check: there is no backend, which is the point.
#[derive(Debug)]
pub struct CheckBackendData;

impl Backend for CheckBackendData {
    fn seat_name(&self) -> String {
        SEAT_NAME.to_owned()
    }
}

type State = TemplarCompState<CheckBackendData>;

/// Build everything, print a report, and return without entering the event loop.
///
/// With `exec`, it goes one step further and actually runs that client under the compositor until
/// it exits, returning its exit code. That makes the `--exec` path — including its shutdown
/// handshake, which had a real race in it — testable on a machine with no display, no seat and no
/// host compositor, which is the only kind CI has.
pub fn run(exec: Option<&[String]>) -> anyhow::Result<i32> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let (mut state, socket_name) = build_state(&event_loop, runtime.handle())?;
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

    println!("templar-comp self-check");
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
    println!("  private protocols:    templar_shell_v2, templar_recovery_v1");
    println!(
        "  idle screen-off:      {}",
        state.idle.timeout().map_or_else(
            || "never (until the shell sends one)".to_owned(),
            |delay| { format!("{}s", delay.as_secs()) }
        )
    );
    println!(
        "  shell-only globals:   ext_foreign_toplevel_image_capture_source_manager_v1, \
         ext_output_image_capture_source_manager_v1, ext_image_copy_capture_manager_v1"
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

/// The whole compositor state with no backend, listening on a fresh public socket (returned).
/// The session-bus services degrade to disconnected caches/notifiers when there is no bus,
/// exactly as they do when sessiond has not started yet.
pub(crate) fn build_state(
    event_loop: &EventLoop<'static, State>,
    runtime: &tokio::runtime::Handle,
) -> anyhow::Result<(State, String)> {
    let display: Display<State> = Display::new()?;
    let dh = display.handle();
    let identity_cache =
        DbusIdentityCache::spawn(runtime, crate::identity_sink::install(&event_loop.handle()));
    let governor = GovernorNotifier::spawn(runtime);
    let idle_notifier = IdleNotifier::spawn(runtime);
    let shell_sink = crate::shell_sink::install(&event_loop.handle());
    let shell_broadcaster = ShellStateBroadcaster::spawn(runtime, shell_sink);

    let socket_name = crate::state::init_wayland_listener(display, &event_loop.handle(), None)?;

    let state = State::new(
        CheckBackendData,
        SEAT_NAME,
        &dh,
        event_loop.handle(),
        event_loop.get_signal(),
        identity_cache,
        governor,
        idle_notifier,
        shell_broadcaster,
    )?;
    State::create_private_globals(&dh);
    Ok((state, socket_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

    /// Globals that exist only for the trusted shell: `templar_shell_v2` can shelve, activate and
    /// place surfaces in privileged layers, `templar_recovery_v1` drives the pressure overlay, and
    /// the three capture globals read other applications' pixels — the output one reads the whole
    /// screen. A global an ordinary client cannot see is one it cannot attempt to bind.
    const TRUSTED_ONLY: [&str; 5] = [
        "templar_shell_manager_v2",
        "templar_recovery_manager_v1",
        "ext_output_image_capture_source_manager_v1",
        "ext_foreign_toplevel_image_capture_source_manager_v1",
        "ext_image_copy_capture_manager_v1",
    ];

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

    /// Protected globals are absent on the public socket and reachable on the private socket.
    #[test]
    fn public_socket_conceals_protected_globals_private_socket_exposes_them() {
        let (socket_tx, socket_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        // The compositor is not `Send`, so it is built inside its own thread and only its socket
        // name crosses back.
        let server = std::thread::spawn(move || {
            let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (mut state, socket_name) = build_state(&event_loop, runtime.handle()).unwrap();
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

        // The security boundary, from the outside: everything in `TRUSTED_ONLY` is gated by
        // `can_view` on having connected over the private socket.
        for private in TRUSTED_ONLY {
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
        for private in TRUSTED_ONLY {
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

    /// A pointer grab that changes the window model from inside `button` — what the name-tag
    /// click does (`WindowGestureGrab::button` → `toggle_always_on_top` → `sync_after_change`),
    /// and dropping a window on another's band (`group_windows`).
    struct ChangesTheModelOnRelease {
        start_data: smithay::input::pointer::GrabStartData<State>,
    }

    impl smithay::input::pointer::PointerGrab<State> for ChangesTheModelOnRelease {
        fn motion(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            _focus: Option<(
                smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
                smithay::utils::Point<f64, smithay::utils::Logical>,
            )>,
            event: &smithay::input::pointer::MotionEvent,
        ) {
            handle.motion(data, None, event);
        }
        fn relative_motion(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            focus: Option<(
                smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
                smithay::utils::Point<f64, smithay::utils::Logical>,
            )>,
            event: &smithay::input::pointer::RelativeMotionEvent,
        ) {
            handle.relative_motion(data, focus, event);
        }
        fn button(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::ButtonEvent,
        ) {
            handle.button(data, event);
            crate::space_sync::sync_after_change(data);
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
        fn axis(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            details: smithay::input::pointer::AxisFrame,
        ) {
            handle.axis(data, details);
        }
        fn frame(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
        ) {
            handle.frame(data);
        }
        fn gesture_swipe_begin(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GestureSwipeBeginEvent,
        ) {
            handle.gesture_swipe_begin(data, event);
        }
        fn gesture_swipe_update(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GestureSwipeUpdateEvent,
        ) {
            handle.gesture_swipe_update(data, event);
        }
        fn gesture_swipe_end(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GestureSwipeEndEvent,
        ) {
            handle.gesture_swipe_end(data, event);
        }
        fn gesture_pinch_begin(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GesturePinchBeginEvent,
        ) {
            handle.gesture_pinch_begin(data, event);
        }
        fn gesture_pinch_update(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GesturePinchUpdateEvent,
        ) {
            handle.gesture_pinch_update(data, event);
        }
        fn gesture_pinch_end(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GesturePinchEndEvent,
        ) {
            handle.gesture_pinch_end(data, event);
        }
        fn gesture_hold_begin(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GestureHoldBeginEvent,
        ) {
            handle.gesture_hold_begin(data, event);
        }
        fn gesture_hold_end(
            &mut self,
            data: &mut State,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, State>,
            event: &smithay::input::pointer::GestureHoldEndEvent,
        ) {
            handle.gesture_hold_end(data, event);
        }
        fn start_data(&self) -> &smithay::input::pointer::GrabStartData<State> {
            &self.start_data
        }
        fn unset(&mut self, _data: &mut State) {}
    }

    /// The 2026-10-01 desktop freezes: Smithay holds the pointer's mutex while a grab handles a
    /// button, and `sync_after_change` used to ask that same pointer (`is_grabbed`,
    /// `current_location`) whether to refresh its focus — a second lock of a `std` mutex on the
    /// same thread, which never returns. The compositor's main thread slept in it forever: no
    /// frames, no input, every other process still running. A name-tag click took that path.
    #[test]
    fn changing_the_window_model_inside_a_pointer_grab_does_not_hang_the_compositor() {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (mut state, _socket) = build_state(&event_loop, runtime.handle()).unwrap();
            let pointer = state.seat.get_pointer().unwrap();
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            pointer.set_grab(
                &mut state,
                ChangesTheModelOnRelease {
                    start_data: smithay::input::pointer::GrabStartData {
                        focus: None,
                        button: 0x110,
                        location: pointer.current_location(),
                    },
                },
                serial,
                smithay::input::pointer::Focus::Clear,
            );
            pointer.button(
                &mut state,
                &smithay::input::pointer::ButtonEvent {
                    serial,
                    time: 0,
                    button: 0x110,
                    state: smithay::backend::input::ButtonState::Released,
                },
            );
            // The deferred pointer refresh runs on the loop's next turn, outside the grab.
            event_loop
                .dispatch(Some(std::time::Duration::from_millis(0)), &mut state)
                .unwrap();
            done_tx.send(pointer.is_grabbed()).unwrap();
        });
        let still_grabbed = done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the compositor thread deadlocked inside the pointer grab");
        assert!(!still_grabbed, "the grab ended with the release");
    }

    #[derive(Default)]
    struct FocusClient {
        compositor: Option<wayland_client::protocol::wl_compositor::WlCompositor>,
        keyboard: Option<wayland_client::protocol::wl_keyboard::WlKeyboard>,
        events: Vec<FocusEvent>,
    }

    #[derive(Debug)]
    enum FocusEvent {
        Enter(u32),
        Modifiers(u32),
        Key(u32, bool),
    }

    enum FocusCommand {
        Inspect,
        Release,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for FocusClient {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            (): &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == "wl_compositor" {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                } else if interface == "wl_seat" {
                    let seat: wayland_client::protocol::wl_seat::WlSeat =
                        registry.bind(name, version.min(7), qh, ());
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            }
        }
    }
    impl Dispatch<wayland_client::protocol::wl_keyboard::WlKeyboard, ()> for FocusClient {
        fn event(
            state: &mut Self,
            _: &wayland_client::protocol::wl_keyboard::WlKeyboard,
            event: wayland_client::protocol::wl_keyboard::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            use wayland_client::{Proxy, WEnum, protocol::wl_keyboard};
            match event {
                wl_keyboard::Event::Enter { surface, .. } => {
                    state
                        .events
                        .push(FocusEvent::Enter(surface.id().protocol_id()));
                }
                wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                    state.events.push(FocusEvent::Modifiers(mods_depressed));
                }
                wl_keyboard::Event::Key {
                    key,
                    state: key_state,
                    ..
                } => {
                    state.events.push(FocusEvent::Key(
                        key,
                        key_state == WEnum::Value(wl_keyboard::KeyState::Pressed),
                    ));
                }
                _ => {}
            }
        }
    }
    wayland_client::delegate_noop!(FocusClient: ignore wayland_client::protocol::wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(FocusClient: ignore wayland_client::protocol::wl_surface::WlSurface);
    wayland_client::delegate_noop!(FocusClient: ignore wayland_client::protocol::wl_seat::WlSeat);

    struct FocusSurfaces {
        owner: smithay::reexports::wayland_server::Client,
        surfaces: [smithay::reexports::wayland_server::protocol::wl_surface::WlSurface; 3],
        release: mpsc::Sender<FocusCommand>,
        events: mpsc::Receiver<Vec<FocusEvent>>,
        client: std::thread::JoinHandle<()>,
    }

    fn focus_surfaces(event_loop: &mut EventLoop<State>, state: &mut State) -> FocusSurfaces {
        use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
        let (server, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut dh = state.display_handle.clone();
        let owner = dh
            .insert_client(
                server,
                std::sync::Arc::new(crate::state::ClientState {
                    trusted_shell: true,
                    ..Default::default()
                }),
            )
            .unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let client = std::thread::spawn(move || {
            use wayland_client::Proxy;
            let connection = Connection::from_socket(peer).unwrap();
            let mut queue = connection.new_event_queue();
            let qh = queue.handle();
            let mut globals = FocusClient::default();
            connection.display().get_registry(&qh, ());
            queue.roundtrip(&mut globals).unwrap();
            let compositor = globals.compositor.as_ref().unwrap();
            let surfaces: [_; 3] = std::array::from_fn(|_| compositor.create_surface(&qh, ()));
            queue.roundtrip(&mut globals).unwrap();
            ready_tx
                .send(
                    surfaces
                        .each_ref()
                        .map(|surface| surface.id().protocol_id()),
                )
                .unwrap();
            while let Ok(command) = release_rx.recv() {
                match command {
                    FocusCommand::Inspect => {
                        queue.roundtrip(&mut globals).unwrap();
                        events_tx.send(std::mem::take(&mut globals.events)).unwrap();
                    }
                    FocusCommand::Release => break,
                }
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ids = loop {
            if let Ok(ids) = ready_rx.try_recv() {
                break ids;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "client failed to create real surfaces"
            );
            event_loop
                .dispatch(Some(std::time::Duration::from_millis(20)), state)
                .unwrap();
            dh.flush_clients().unwrap();
        };
        let surfaces = ids.map(|id| owner.object_from_protocol_id::<WlSurface>(&dh, id).unwrap());
        FocusSurfaces {
            owner,
            surfaces,
            release,
            events,
            client,
        }
    }

    fn focus_events(
        event_loop: &mut EventLoop<State>,
        state: &mut State,
        commands: &mpsc::Sender<FocusCommand>,
        events: &mpsc::Receiver<Vec<FocusEvent>>,
    ) -> Vec<FocusEvent> {
        commands.send(FocusCommand::Inspect).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(events) = events.try_recv() {
                return events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "keyboard observer roundtrip timed out"
            );
            event_loop
                .dispatch(Some(std::time::Duration::from_millis(20)), state)
                .unwrap();
            state.display_handle.flush_clients().unwrap();
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn popup_focus_preserves_home_and_mru_and_cannot_steal_focus_on_release() {
        use templar_window::{ClientKind, Layer, Rect, WindowClaims};

        let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut state, _) = build_state(&event_loop, runtime.handle()).unwrap();
        let FocusSurfaces {
            owner: shell,
            surfaces,
            release,
            events,
            client,
        } = focus_surfaces(&mut event_loop, &mut state);
        let mut surfaces = surfaces.into_iter();
        let mut make_surface = |layer| {
            let surface = surfaces.next().unwrap();
            let id = state.window_service.on_toplevel_mapped(
                1,
                ClientKind::TrustedShell,
                WindowClaims::default(),
                "focus fixture".into(),
                Rect::new(10, 10, 300, 200),
            );
            state
                .window_service
                .set_layer(id, layer, ClientKind::TrustedShell)
                .unwrap();
            state.window_service.set_role(
                id,
                match layer {
                    Layer::HomeSurface => templar_window::WindowRole::Shell,
                    Layer::ShellOverlay => templar_window::WindowRole::Overlay,
                    _ => templar_window::WindowRole::Application,
                },
            );
            state.windows.mark_mapped(surface.clone(), id);
            (surface, id)
        };
        let (home, home_id) = make_surface(Layer::HomeSurface);
        let (popup, popup_id) = make_surface(Layer::ShellOverlay);
        let (client_surface, client_id) = make_surface(Layer::Normal);
        let keyboard = state.seat.get_keyboard().unwrap();
        let request = |state: &mut State, surface| {
            crate::protocols_shell::focus_shell_surface(state, &shell, surface);
        };
        keyboard.set_focus(
            &mut state,
            Some(client_surface.clone()),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
        let key = |code: u32, key_state| crate::launcher_input::QueuedKey {
            code: smithay::input::keyboard::Keycode::new(code + 8),
            key_state,
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time: 1,
        };
        use smithay::backend::input::KeyState::{Pressed, Released};
        // The raw mapping queue preserves a complete modifier chord before the
        // popup exists and drains it through production keyboard routing after focus.
        state.begin_launcher_input(&shell);
        for event in [
            key(42, Pressed),
            key(30, Pressed),
            key(30, Released),
            key(42, Released),
        ] {
            assert!(state.buffer_launcher_key(event));
        }
        assert!(!keyboard.modifier_state().shift);
        request(&mut state, Some(popup.clone()));
        assert_eq!(keyboard.current_focus(), Some(popup.clone()));
        assert!(!keyboard.modifier_state().shift);
        assert!(keyboard.pressed_keys().is_empty());
        // Reproduce Super+Space's physical ordering: focus enters while Logo is
        // held, then the intercepted release must reach the client's modifiers
        // before the first queued character. Checking XKB alone misses this bug.
        keyboard.set_focus(
            &mut state,
            Some(client_surface.clone()),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
        state.on_keyboard_key(key(125, Pressed));
        state.on_keyboard_key(key(57, Pressed));
        assert!(keyboard.modifier_state().logo);
        let _ = focus_events(&mut event_loop, &mut state, &release, &events);
        state.begin_launcher_input(&shell);
        for event in [
            key(57, Released),
            key(125, Released),
            key(38, Pressed),
            key(38, Released),
            key(15, Pressed),
            key(15, Released),
        ] {
            assert!(state.buffer_launcher_key(event));
        }
        request(&mut state, Some(popup.clone()));
        let observed = focus_events(&mut event_loop, &mut state, &release, &events);
        use smithay::reexports::wayland_server::Resource;
        let popup_enter = observed
            .iter()
            .position(|event| {
                matches!(event,
            FocusEvent::Enter(id) if *id == popup.id().protocol_id())
            })
            .unwrap();
        let first_text = observed
            .iter()
            .position(|event| matches!(event, FocusEvent::Key(38, true)))
            .expect("queued text must reach the actual keyboard client");
        let modifiers: Vec<_> = observed[popup_enter..first_text]
            .iter()
            .filter_map(|event| {
                if let FocusEvent::Modifiers(mask) = event {
                    Some(*mask)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            modifiers.first().is_some_and(|mask| *mask != 0),
            "focus must enter with Logo held: {observed:?}"
        );
        assert_eq!(
            modifiers.last(),
            Some(&0),
            "Logo release must be advertised before text: {observed:?}"
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, FocusEvent::Key(125, _))),
            "reserved Super must not send an orphan key release: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, FocusEvent::Key(15, true))),
            "Tab follows the queued text: {observed:?}"
        );
        assert!(!keyboard.modifier_state().logo);
        assert!(keyboard.pressed_keys().is_empty());
        // An absent shell ends the wait on its own timer and never leaves XKB held.
        keyboard.set_focus(
            &mut state,
            Some(client_surface.clone()),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
        state.begin_launcher_input(&shell);
        assert!(state.buffer_launcher_key(key(29, Pressed)));
        assert!(state.buffer_launcher_key(key(29, Released)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while state.launcher_focus_waiting() {
            assert!(
                std::time::Instant::now() < deadline,
                "launcher wait timer did not fire"
            );
            event_loop
                .dispatch(Some(std::time::Duration::from_millis(20)), &mut state)
                .unwrap();
        }
        assert!(
            !state.buffer_launcher_key(key(30, Pressed)),
            "focus wait must expire"
        );
        assert!(!keyboard.modifier_state().ctrl);
        assert!(keyboard.pressed_keys().is_empty());
        assert_eq!(keyboard.current_focus(), Some(client_surface.clone()));
        state.window_service.activate(client_id);
        state.window_service.set_maximized(client_id, true);
        let launcher_before = state.window_service.shell_toplevels();
        state.run_recovery_action(crate::keybindings::RecoveryAction::RevealLauncher);
        assert!(
            !state.window_service.home_revealed(),
            "launcher requests keep active work visible"
        );
        assert_eq!(state.window_service.shell_toplevels(), launcher_before);
        state.window_service.set_maximized(client_id, false);
        state.window_service.activate(client_id);
        state.window_service.set_home_revealed(true);
        keyboard.set_focus(
            &mut state,
            Some(home.clone()),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
        let before = state.window_service.shell_toplevels();
        request(&mut state, Some(client_surface.clone()));
        assert_eq!(
            keyboard.current_focus(),
            Some(home.clone()),
            "ordinary layers cannot borrow popup focus"
        );
        request(&mut state, Some(popup.clone()));
        assert_eq!(keyboard.current_focus(), Some(popup.clone()));
        crate::input::focus_home_if_needed(&mut state, &home);
        assert_eq!(
            keyboard.current_focus(),
            Some(popup.clone()),
            "a Home resize must not steal popup keys"
        );
        assert!(state.window_service.home_revealed());
        assert_eq!(state.window_service.shell_toplevels(), before);
        request(&mut state, None);
        assert_eq!(keyboard.current_focus(), Some(home.clone()));
        assert_eq!(state.window_service.shell_toplevels(), before);

        // An outside click won focus already: a delayed popup-close request must not take it back.
        state.window_service.set_home_revealed(false);
        keyboard.set_focus(
            &mut state,
            Some(client_surface.clone()),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
        request(&mut state, None);
        assert_eq!(keyboard.current_focus(), Some(client_surface.clone()));
        // Exercise the production outside-press path, including Home and empty desktop.
        state
            .window_service
            .set_geometry(home_id, Rect::new(0, 0, 800, 600));
        state
            .window_service
            .set_geometry(client_id, Rect::new(10, 10, 300, 200));
        state
            .window_service
            .set_geometry(popup_id, Rect::new(500, 10, 300, 200));
        request(&mut state, Some(popup.clone()));
        state.handle_press_for_focus_or_drag(
            (50.0, 50.0).into(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
            0x110,
        );
        assert_eq!(keyboard.current_focus(), Some(client_surface.clone()));
        request(&mut state, None);
        assert_eq!(keyboard.current_focus(), Some(client_surface.clone()));
        request(&mut state, Some(popup.clone()));
        state.handle_press_for_focus_or_drag(
            (400.0, 400.0).into(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
            0x110,
        );
        assert_eq!(keyboard.current_focus(), Some(home.clone()));
        request(&mut state, None);
        assert_eq!(keyboard.current_focus(), Some(home.clone()));
        request(&mut state, Some(popup.clone()));
        state.handle_press_for_focus_or_drag(
            (900.0, 700.0).into(),
            smithay::utils::SERIAL_COUNTER.next_serial(),
            0x110,
        );
        assert_eq!(keyboard.current_focus(), None);
        request(&mut state, None);
        assert_eq!(keyboard.current_focus(), None);
        keyboard.set_focus(
            &mut state,
            Some(client_surface.clone()),
            smithay::utils::SERIAL_COUNTER.next_serial(),
        );
        // Hidden surfaces and the lock screen cannot be bypassed by a shell popup request.
        state.window_service.set_mapped(popup_id, false);
        request(&mut state, Some(popup.clone()));
        assert_eq!(keyboard.current_focus(), Some(client_surface.clone()));
        state.window_service.set_mapped(popup_id, true);
        state.session_lock.locked = true;
        request(&mut state, Some(popup));
        assert_eq!(keyboard.current_focus(), Some(client_surface));
        release.send(FocusCommand::Release).unwrap();
        client.join().unwrap();
    }

    /// `set_minimize_target` and the park/unpark funnel on a whole headless compositor: a card
    /// is accepted only on the shell connection's own Home surface, shelving a window that has
    /// one starts a park into the card's global rectangle, a window without one just vanishes,
    /// unshelving grows back out, and destroying either window drops the card and the motion.
    #[test]
    // One scenario, told in order: each step's state is the next step's precondition.
    #[allow(clippy::too_many_lines)]
    fn a_card_on_home_turns_parking_and_unparking_into_motions() {
        use crate::motion::MotionKind;
        use crate::protocols_shell::set_minimize_target;
        use crate::space_sync::{client_area, on_window_destroyed, sync_after_change};
        use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
        use templar_window::{ClientKind, Layer, Rect, WindowClaims, WindowRole};

        let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut state, _) = build_state(&event_loop, runtime.handle()).unwrap();
        let FocusSurfaces {
            owner: shell,
            surfaces: [home_surface, popup_surface, _],
            release,
            events: _,
            client,
        } = focus_surfaces(&mut event_loop, &mut state);
        let shell_window = |state: &mut State, surface: &WlSurface, layer, role| {
            let id = state.window_service.on_toplevel_mapped(
                1,
                ClientKind::TrustedShell,
                WindowClaims::default(),
                "shell".into(),
                Rect::new(0, 0, 1280, 720),
            );
            state
                .window_service
                .set_layer(id, layer, ClientKind::TrustedShell)
                .unwrap();
            state.window_service.set_role(id, role);
            state.windows.mark_mapped(surface.clone(), id);
            id
        };
        let home = shell_window(
            &mut state,
            &home_surface,
            Layer::HomeSurface,
            WindowRole::Shell,
        );
        shell_window(
            &mut state,
            &popup_surface,
            Layer::ShellOverlay,
            WindowRole::Overlay,
        );
        let app = |state: &mut State, title: &str| {
            let id = state.window_service.on_toplevel_mapped(
                200,
                ClientKind::Application,
                WindowClaims::default(),
                title.into(),
                Rect::new(100, 100, 400, 300),
            );
            state.window_service.activate(id);
            id
        };
        let carded = app(&mut state, "carded");
        let plain = app(&mut state, "plain");
        sync_after_change(&mut state);

        let card = Rect::new(40, 600, 160, 90);
        let (other_server, _other_peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let other = state
            .display_handle
            .clone()
            .insert_client(
                other_server,
                std::sync::Arc::new(crate::state::ClientState {
                    trusted_shell: true,
                    ..Default::default()
                }),
            )
            .unwrap();
        set_minimize_target(&mut state, &other, carded, Some(home_surface.clone()), card);
        assert!(
            state.motion.target(carded).is_none(),
            "a card on another connection's surface is refused"
        );
        set_minimize_target(&mut state, &shell, carded, Some(popup_surface), card);
        assert!(
            state.motion.target(carded).is_none(),
            "a popup is not where a window lives"
        );
        set_minimize_target(&mut state, &shell, carded, Some(home_surface.clone()), card);
        assert_eq!(
            state
                .motion
                .target(carded)
                .map(|target| target.surface_window),
            Some(home)
        );

        let own = client_area(&state, carded);
        let home_area = client_area(&state, home);
        let global_card = Rect::new(home_area.x + 40, home_area.y + 600, 160, 90);
        assert!(state.window_service.shelve(carded));
        assert!(state.window_service.shelve(plain));
        sync_after_change(&mut state);
        let now = crate::window_bridge::now_monotonic_ns(&state.clock);
        let park = *state
            .motion
            .active(carded, now)
            .expect("a carded window parks");
        assert_eq!(park.kind, MotionKind::Park);
        assert_eq!((park.from, park.to), (own, global_card));
        assert!(
            state.motion.active(plain, now).is_none(),
            "no card, no motion: the window simply goes"
        );

        assert!(state.window_service.unshelve(carded).is_some());
        sync_after_change(&mut state);
        let now = crate::window_bridge::now_monotonic_ns(&state.clock);
        let unpark = *state.motion.active(carded, now).expect("it grows back out");
        assert_eq!(unpark.kind, MotionKind::Unpark);
        assert_eq!(unpark.to, client_area(&state, carded));

        set_minimize_target(&mut state, &shell, carded, None, card);
        assert!(
            state.motion.target(carded).is_none(),
            "a null surface clears"
        );
        set_minimize_target(&mut state, &shell, carded, Some(home_surface.clone()), card);
        on_window_destroyed(&mut state, carded);
        assert!(state.motion.target(carded).is_none());
        assert!(state.motion.active(carded, now).is_none());

        set_minimize_target(&mut state, &shell, plain, Some(home_surface), card);
        assert!(state.motion.target(plain).is_some());
        on_window_destroyed(&mut state, home);
        assert!(
            state.motion.target(plain).is_none(),
            "a card dies with the Home surface it was on"
        );

        release.send(FocusCommand::Release).unwrap();
        client.join().unwrap();
    }
}
