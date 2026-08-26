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
    desktop::space::render_output,
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
                            let chrome = crate::chrome::collect_elements(
                                &mut state.chrome,
                                &crate::chrome::ChromeInputs {
                                    space: &state.space,
                                    window_service: &state.window_service,
                                    recovery_protocol: &state.recovery_protocol,
                                    show_all_windows: state.show_all_windows.as_deref(),
                                    session_lock: &state.session_lock,
                                    switcher: state.switcher.as_ref(),
                                },
                                &output_for_loop,
                                renderer,
                            );
                            match render_output::<
                                _,
                                smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<GlesRenderer>,
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
