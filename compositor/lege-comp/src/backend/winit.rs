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
use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    input::pointer::{CursorIcon, CursorImageStatus},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display},
    utils::Transform,
};

const OUTPUT_NAME: &str = "winit";

/// Backend data for the winit path: nothing beyond a seat name, since everything else lives on
/// `LegeCompState` and is shared with `src/backend/udev.rs`.
pub struct WinitBackendData;

impl Backend for WinitBackendData {
    fn seat_name(&self) -> String {
        OUTPUT_NAME.to_owned()
    }

    /// Idle blanking nested (`src/idle.rs`): there is no CRTC here to put into DPMS off, so the
    /// nested output is painted black instead — the redraw path below hands the damage tracker an
    /// empty element list while `state.idle.blanked()`. That is not a stand-in for the real
    /// thing, it is the honest nested equivalent: the timing, the inhibitors and the wake are the
    /// same code as on hardware, and only the last inch differs.
    fn set_blanked(state: &mut State, blanked: bool) {
        tracing::info!(blanked, "nested output blanking");
        let _ = state;
    }
}

type State = LegeCompState<WinitBackendData>;

/// Runs the compositor nested inside the current session until the winit window closes.
// See `src/backend/udev.rs::run`'s identical `#[allow]` for why this stays one function.
#[allow(clippy::too_many_lines)]
pub fn run(exec: Option<&[String]>, requested_socket_name: Option<&str>) -> anyhow::Result<i32> {
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

    let socket_name =
        crate::state::init_wayland_listener(display, &event_loop.handle(), requested_socket_name)?;
    // Not exported into this process's own environment: `std::env::set_var` would only affect
    // children this process itself spawns, which winit never does, and mutating process-wide
    // environment state is `unsafe` since Rust 2024 for good reason (races with any other thread
    // reading it). A client connects by being told this socket name directly — logged here, and
    // it is also the name a caller would export `WAYLAND_DISPLAY` to in their own shell.
    tracing::info!(socket = %socket_name, "wayland socket ready — set WAYLAND_DISPLAY to this to connect a client");

    let mut state = State::new(
        WinitBackendData,
        OUTPUT_NAME,
        &dh,
        event_loop.handle(),
        event_loop.get_signal(),
        identity_cache,
        governor,
        shell_broadcaster,
    )?;

    State::create_private_globals(&dh);
    let shell_socket_name = crate::shell_socket::install(&mut state, &socket_name)?;
    // After the socket is listening, so the child can connect the moment it starts.
    let session_command = exec
        .map(|argv| {
            crate::session_command::SessionCommand::spawn(argv, &socket_name, &event_loop.handle())
        })
        .transpose()?;
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
    let mut ready_notified = false;
    // The host cursor last asked for, so the window is only told when it actually changes.
    let mut host_cursor: Option<CursorIcon> = None;
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
                    // `render_output`'s returned damage regions borrow the bound framebuffer, which
                    // would keep `graphics` mutably borrowed into the `submit` call below — sidestep
                    // that by only asking "did anything change", then submitting full-output damage
                    // (matching `smallvil`'s winit backend, which does the same for the same reason).
                    let rendered = match graphics.bind() {
                        Ok((renderer, mut framebuffer)) => {
                            // The same shared scene builder the udev backend uses, so the two
                            // backends cannot disagree about draw order — which is the winit
                            // backend's whole claim (module doc): what is exercised here is the
                            // real thing. Nothing at all while the session is idle-blanked, so
                            // the clear colour below is the whole frame.
                            let elements = if state.idle.blanked() {
                                Vec::new()
                            } else {
                                crate::scene::collect(
                                    &mut state.chrome,
                                    &state.windows,
                                    &crate::chrome::ChromeInputs {
                                        space: &state.space,
                                        window_service: &state.window_service,
                                        recovery_protocol: &state.recovery_protocol,
                                        show_all_windows: state.show_all_windows.as_deref(),
                                        session_lock: &state.session_lock,
                                        switcher: state.switcher.as_ref(),
                                        drop_target: state.drop_target,
                                    },
                                    &output_for_loop,
                                    renderer,
                                )
                            };
                            let clear = if state.idle.blanked() {
                                [0.0, 0.0, 0.0, 1.0]
                            } else {
                                [0.05, 0.05, 0.07, 1.0]
                            };
                            match damage_tracker.render_output(
                                renderer,
                                &mut framebuffer,
                                0,
                                &elements,
                                clear,
                            ) {
                                Ok(result) => {
                                    // Previews queued since the last frame, with the renderer
                                    // this pass just used (`src/screencopy.rs`'s module doc).
                                    crate::screencopy::drain_pending(
                                        &mut state.screencopy,
                                        &state.windows,
                                        &state.clock,
                                        renderer,
                                    );
                                    result.damage.is_some()
                                }
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
                    let presented = if rendered {
                        let full_damage =
                            smithay::utils::Rectangle::from_size(graphics.window_size());
                        match graphics.submit(Some(&[full_damage])) {
                            Ok(()) => true,
                            Err(error) => {
                                tracing::warn!(%error, "failed to submit winit frame");
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if presented {
                        crate::notify_ready_once(&mut ready_notified);
                    }

                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output_for_loop,
                            state.start_time.elapsed(),
                            Some(std::time::Duration::ZERO),
                            |_, _| Some(output_for_loop.clone()),
                        );
                    });
                    apply_host_cursor(graphics.window(), &state.cursor_status, &mut host_cursor);
                    graphics.window().request_redraw();
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                WinitEvent::Focus(_) => {}
            },
        )
        .map_err(|error| anyhow::anyhow!("inserting winit event source: {error}"))?;

    // READY is sent by the redraw callback after the first host-frame submission. Until then a
    // Type=notify unit remains in startup and its dependants cannot race an unpainted backend.
    tracing::info!("compositor initialization complete; waiting for first presented frame");
    event_loop.run(None, &mut state, |state| {
        // Flush after every dispatch, not only after a host-window redraw: registry/configure
        // round trips must complete even while the nested output has no visual damage.
        state.space.refresh();
        state.popups.cleanup();
        if let Err(error) = state.display_handle.flush_clients() {
            tracing::warn!(%error, "flushing Wayland clients after dispatch");
        }
    })?;
    Ok(session_command.map_or(0, |command| command.shut_down()))
}

/// Show `status` on the host window, so the compositor's own cursors (the resize perimeter, most
/// of all) are visible when running nested — where there is no scanout to paint one onto and
/// `src/cursor.rs` therefore never runs.
///
/// A client-supplied cursor surface is drawn by the scene like any other surface, so the host
/// pointer is hidden underneath it rather than showing two cursors at once. `last` is the icon
/// already applied: winit's `set_cursor` is not free, and this is called once per frame.
fn apply_host_cursor(
    window: &dyn smithay::reexports::winit::window::Window,
    status: &CursorImageStatus,
    last: &mut Option<CursorIcon>,
) {
    let wanted = match status {
        CursorImageStatus::Named(icon) => Some(*icon),
        CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => None,
    };
    if *last == wanted {
        return;
    }
    match wanted {
        Some(icon) => {
            window.set_cursor(icon.into());
            window.set_cursor_visible(true);
        }
        None => window.set_cursor_visible(false),
    }
    *last = wanted;
}
