//! The winit backend: the compositor hosted inside an existing session
//! (`WAYLAND_DISPLAY`/X11), as one synthetic output. This is the backend the crate is developed
//! and tested against with no display hardware at all (COMPOSITOR.md's "Backends" — "not a debug
//! aid"): every geometry/input/protocol code path above this file is identical between winit and
//! udev, so exercising it here exercises the real thing.
//!
//! DMA-BUF is not wired up on this backend (no `zwp_linux_dmabuf_v1` global is created): the
//! renderer here is a plain `GlesRenderer` bound to a nested window with no GPU-buffer-sharing
//! story of its own to advertise, and every client this backend was actually tested against
//! (`cosmic-term`, `weston-terminal`) works over `wl_shm`. Real zero-copy GPU clients over winit
//! are a real gap, not a silent one — the udev backend (`src/backend/udev.rs`) is where DMA-BUF
//! actually matters and is wired there.

use crate::state::{Backend, LegeCompState};
use lege_comp_integration::{DbusIdentityCache, GovernorNotifier, ShellStateBroadcaster};
use lege_window::WindowService;
use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    desktop::{PopupManager, Space, space::render_output},
    input::{SeatState, pointer::CursorImageStatus},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display},
    utils::{Clock, Transform},
    wayland::{
        compositor::CompositorState,
        dmabuf::DmabufState,
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        presentation::PresentationState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        session_lock::SessionLockManagerState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState, dialog::XdgDialogState},
        shm::ShmState,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xwayland_keyboard_grab::XWaylandKeyboardGrabState,
        xwayland_shell::XWaylandShellState,
    },
};

const OUTPUT_NAME: &str = "winit";

/// Backend data for the winit path: nothing beyond a seat name, since everything else lives on
/// `LegeCompState` and is shared with `src/backend/udev.rs`.
pub struct WinitBackendData;

impl Backend for WinitBackendData {
    fn seat_name(&self) -> String {
        OUTPUT_NAME.to_owned()
    }
}

type State = LegeCompState<WinitBackendData>;

/// Runs the compositor nested inside the current session until the winit window closes.
// See `src/backend/udev.rs::run`'s identical `#[allow]` for why this stays one function.
#[allow(clippy::too_many_lines)]
pub fn run() -> anyhow::Result<()> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let (mut graphics, winit_events) = winit::init::<GlesRenderer>()
        .map_err(|error| anyhow::anyhow!("winit backend init failed: {error}"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    let identity_cache = DbusIdentityCache::spawn(&handle);
    let governor = GovernorNotifier::spawn(&handle);
    let shell_sink = crate::shell_sink::install(&event_loop.handle());
    let shell_broadcaster = ShellStateBroadcaster::spawn(&handle, shell_sink);

    let socket_name = crate::state::init_wayland_listener(display, &event_loop.handle())?;
    // Not exported into this process's own environment: `std::env::set_var` would only affect
    // children this process itself spawns, which winit never does, and mutating process-wide
    // environment state is `unsafe` since Rust 2024 for good reason (races with any other thread
    // reading it). A client connects by being told this socket name directly — logged here, and
    // it is also the name a caller would export `WAYLAND_DISPLAY` to in their own shell.
    tracing::info!(socket = %socket_name, "wayland socket ready — set WAYLAND_DISPLAY to this to connect a client");

    let mut seat_state: SeatState<State> = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, OUTPUT_NAME);
    seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 200, 25)
        .expect("failed to add keyboard to seat");
    seat.add_pointer();

    let mut state = State {
        backend_data: WinitBackendData,
        display_handle: dh.clone(),
        loop_handle: event_loop.handle(),
        loop_signal: event_loop.get_signal(),
        clock: Clock::new(),
        start_time: std::time::Instant::now(),

        compositor_state: CompositorState::new::<State>(&dh),
        shm_state: ShmState::new::<State>(&dh, Vec::new()),
        output_manager_state: OutputManagerState::new_with_xdg_output::<State>(&dh),
        seat_state,
        data_device_state: DataDeviceState::new::<State>(&dh),
        primary_selection_state: PrimarySelectionState::new::<State>(&dh),
        viewporter_state: ViewporterState::new::<State>(&dh),
        fractional_scale_manager_state: FractionalScaleManagerState::new::<State>(&dh),
        presentation_state: PresentationState::new::<State>(&dh, libc::CLOCK_MONOTONIC as u32),
        xdg_shell_state: XdgShellState::new::<State>(&dh),
        xdg_decoration_state: XdgDecorationState::new::<State>(&dh),
        xdg_dialog_state: XdgDialogState::new::<State>(&dh),
        xdg_activation_state: XdgActivationState::new::<State>(&dh),
        session_lock_manager_state: SessionLockManagerState::new::<State, _>(&dh, |_| true),
        foreign_toplevel_list_state: ForeignToplevelListState::new::<State>(&dh),
        dmabuf_state: DmabufState::new(),
        xwayland_shell_state: XWaylandShellState::new::<State>(&dh),
        xwayland_keyboard_grab_state: XWaylandKeyboardGrabState::new::<State>(&dh),
        xwm: None,
        xwayland_display: None,

        seat,
        seat_name: OUTPUT_NAME.to_owned(),
        cursor_status: CursorImageStatus::default_named(),

        space: Space::default(),
        popups: PopupManager::default(),
        windows: crate::window_bridge::WindowIndex::new(),

        window_service: WindowService::new(identity_cache.clone(), Vec::new()),
        identity_cache,
        governor,
        shell_broadcaster,

        shell_protocol: crate::protocols_shell::ShellProtocolState::default(),
        recovery_protocol: crate::protocols_recovery::RecoveryProtocolState::default(),
        session_lock: crate::session_lock::SessionLockState::default(),
        foreign_toplevels: crate::foreign_toplevel::ForeignToplevelState::default(),
        show_all_windows: None,
    };

    let _shell_global = dh.create_global::<State, crate::protocols::lege_shell_v1::lege_shell_manager_v1::LegeShellManagerV1, _>(
        1,
        crate::protocols_shell::ShellManagerGlobalData,
    );
    let _recovery_global = dh
        .create_global::<State, crate::protocols::lege_recovery_v1::lege_recovery_manager_v1::LegeRecoveryManagerV1, _>(
            1,
            crate::protocols_recovery::RecoveryManagerGlobalData,
        );
    let shell_socket_name = crate::shell_socket::install(&mut state, &socket_name)?;
    tracing::info!(socket = %shell_socket_name, "trusted-shell socket ready");

    let size = graphics.window_size();
    let output = Output::new(
        OUTPUT_NAME.to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "LegeOS".into(),
            model: "winit".into(),
            serial_number: "1".into(),
        },
    );
    let _global = output.create_global::<State>(&dh);
    let mode = Mode {
        size,
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));
    crate::backend::recompute_outputs(&mut state);

    state.start_xwayland(&event_loop.handle());

    let output_for_loop = output.clone();
    let mut damage_tracker = OutputDamageTracker::from_output(&output_for_loop);
    event_loop
        .handle()
        .insert_source(
            winit_events,
            move |event, (), state: &mut State| match event {
                WinitEvent::Resized { size, .. } => {
                    let mode = Mode {
                        size,
                        refresh: 60_000,
                    };
                    output_for_loop.change_current_state(Some(mode), None, None, None);
                    output_for_loop.set_preferred(mode);
                    crate::backend::recompute_outputs(state);
                }
                WinitEvent::Input(event) => {
                    state.process_input_event(event, Some(OUTPUT_NAME));
                }
                WinitEvent::Redraw => {
                    let chrome = crate::chrome::collect_elements(state, &output_for_loop);
                    // `render_output`'s returned damage regions borrow the bound framebuffer, which
                    // would keep `graphics` mutably borrowed into the `submit` call below — sidestep
                    // that by only asking "did anything change", then submitting full-output damage
                    // (matching `smallvil`'s winit backend, which does the same for the same reason).
                    let rendered = match graphics.bind() {
                        Ok((renderer, mut framebuffer)) => {
                            match render_output::<
                                _,
                                smithay::backend::renderer::element::solid::SolidColorRenderElement,
                                _,
                                _,
                            >(
                                &output_for_loop,
                                renderer,
                                &mut framebuffer,
                                1.0,
                                0,
                                [&state.space],
                                &chrome,
                                &mut damage_tracker,
                                [0.05, 0.05, 0.07, 1.0],
                            ) {
                                Ok(result) => result.damage.is_some(),
                                Err(error) => {
                                    tracing::warn!(%error, "render_output failed");
                                    false
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to bind winit framebuffer");
                            false
                        }
                    };
                    if rendered {
                        let full_damage =
                            smithay::utils::Rectangle::from_size(graphics.window_size());
                        let _ = graphics.submit(Some(&[full_damage]));
                    }

                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output_for_loop,
                            state.start_time.elapsed(),
                            Some(std::time::Duration::ZERO),
                            |_, _| Some(output_for_loop.clone()),
                        );
                    });
                    state.space.refresh();
                    state.popups.cleanup();
                    let _ = state.display_handle.flush_clients();
                    graphics.window().request_redraw();
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                WinitEvent::Focus(_) => {}
            },
        )
        .map_err(|error| anyhow::anyhow!("inserting winit event source: {error}"))?;

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}
