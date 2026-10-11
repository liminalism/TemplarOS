//! Native udev/KMS backend. The deterministic primary GPU owns input-thread scene
//! composition and dmabuf feedback. Secondary DRM/GBM/EGL, fence admission, scanout,
//! modesets and destruction live on bounded detached device-owner lanes; no
//! Smithay cross-GPU/CPU-copy fallback runs on the event/input thread.
//!
//! Every output-topology-affecting event below funnels through the same
//! `crate::backend::recompute_outputs` entry point winit uses — connector hotplug, mode changes
//! and session resume all call it, never touching `WindowService` directly (COMPOSITOR.md's "the
//! only place `Output`/topology changes enter `templar-window`").
//!
//! Rendering is driven entirely by page-flip completion: [`add_device`] creates one
//! [`DrmOutput`]/[`DrmOutputManager`] surface per connected CRTC (`connector_connected`), each
//! frame is composited through `DrmCompositor`'s own damage tracker (`render_and_queue`) and
//! queued with `queue_frame`, and the DRM event source's `DrmEvent::VBlank` (`on_vblank`) both
//! releases the completed buffer (`frame_submitted`) and immediately renders and queues the next
//! one — so the redraw cadence tracks the display's real refresh rate rather than a fixed timer.
//! The one case that cannot be paced by vblank is a frame with no damage at all: nothing is
//! queued, so no further vblank will come, and `schedule_retry` polls once per nominal frame
//! interval until there is something to show again (the same fallback anvil's own render loop
//! uses).

use crate::state::{Backend, TemplarCompState};
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            output::DrmOutputManager,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportMemWl,
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus},
    },
    output::{Mode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::{
            Device as _,
            control::{connector, crtc},
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{Display, backend::GlobalId},
    },
    utils::Transform,
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal},
};
use smithay_drm_extras::{display_info, drm_scanner::DrmScanner};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use templar_comp_integration::{
    DbusIdentityCache, GovernorNotifier, IdleNotifier, ShellStateBroadcaster,
};

// The scene this backend hands to `DrmOutput::render_frame` is built by `crate::scene`, which
// both backends share: whole-output chrome above everything, then each window's own band and
// grip immediately above its own surface, top of the stacking order first. See that module for
// why the order is a correctness question rather than a cosmetic one.

#[path = "native_fence.rs"]
mod native_fence;
#[path = "native_outputs.rs"]
mod native_outputs;
#[path = "native_policy.rs"]
mod native_policy;
#[path = "native_recovery.rs"]
mod native_recovery;
#[path = "native_worker.rs"]
mod native_worker;
/// Clear colour behind every output — matches `src/backend/winit.rs`'s so a window dragged
/// between the two backends during development looks the same.
const CLEAR_COLOR: [f32; 4] = [0.05, 0.05, 0.07, 1.0];

/// How often the main loop checks that every lit output is still being repainted.
const REPAINT_CHECK: Duration = Duration::from_secs(2);
/// A lit output with no frame in flight and no render attempt for this long has lost its render
/// chain: the chain polls once a frame even with nothing to draw (module doc), so no healthy
/// output is ever this quiet.
const REPAINT_STALL: Duration = Duration::from_secs(2);
/// A single `render_and_queue` (captures, scene, render, queue) slower than this is logged with
/// its breakdown — evidence for freezes the event-loop watchdog's 5 s threshold does not catch.
const SLOW_FRAME: Duration = Duration::from_millis(250);

/// Native primary scanout formats; cross-GPU worker scanout is restricted to 8-bit.
const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const CROSS_GPU_COLOR_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
/// Both attestations belong to the exact submitted buffer. Internal Smithay frames carry neither.
#[derive(Debug, Clone, Default)]
struct FrameUserData {
    lock_generation: Option<u64>,
    preview: Option<uuid::Uuid>,
    wallpaper: Option<(String, templar_types::settings::Revision)>,
    transaction: Option<u64>,
}
type UserData = FrameUserData;
type TemplarDrmOutputManager = DrmOutputManager<Allocator, Exporter, UserData, DrmDeviceFd>;
type TemplarDrmOutput = DrmCompositor<Allocator, Exporter, UserData, DrmDeviceFd>;

/// One connected output: direct DRM compositor plus its protocol Output. Direct ownership
/// avoids the manager's hidden untimed bandwidth-fallback fence waits.
struct SurfaceData {
    output: Output,
    global: Option<GlobalId>,
    drm_output: TemplarDrmOutput,
    id: templar_types::display::DisplayId,
    connector: connector::Handle,
    preview_for_next_frame: Option<uuid::Uuid>,
    /// Actual render/queue failure; cleared only by an owned successful submitted frame.
    transaction_for_next_frame: Option<u64>,
    physical_readback_pending: bool,
    render_failure: Option<String>,
    /// This CRTC's page-flip tracking (`src/watchdog.rs`): told when a frame is queued and when
    /// its completion arrives, so a flip the kernel never completes is reported. `None` when the
    /// watchdog could not start.
    flip: Option<crate::watchdog::FlipHandle>,
    /// A frame is queued for scanout and its completion has not arrived. While it is set the
    /// completion (`on_vblank`) owns the next render; nothing else renders this CRTC. Cleared on
    /// completion, on blanking and on a session pause, none of which complete a flip.
    frame_in_flight: bool,
    /// Timestamp fence after a synchronous clear: delayed old events must not release a new frame.
    rearmed_at: Option<crate::flip_recovery::RearmedAt>,
    /// Explicit hardware-lab fault injection; drops one completion, never enabled by default.
    drop_completion_once: bool,
    /// The pending `schedule_retry` timer, if any: with no frame in flight, this is the one other
    /// thing that owns the next render, so a second retry is never stacked on the first.
    retry: Option<RegistrationToken>,
    /// When `render_and_queue` last ran for this CRTC while it was lit, or when it was last lit.
    last_render: Instant,
}

/// One physical DRM device and its independently managed scanout outputs.
struct DeviceData {
    drm_output_manager: TemplarDrmOutputManager,
    allocator: Allocator,
    exporter: Exporter,
    gbm: GbmDevice<DrmDeviceFd>,
    render_node: DrmNode,
    gpu_id: String,
    catalog: HashMap<connector::Handle, templar_types::display::Display>,
    active: bool,
    activation_failure: Option<String>,
    connector_failures: HashMap<connector::Handle, String>,
    connector_scan_known: bool,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    scanner: DrmScanner,
    registration_token: RegistrationToken,
}

pub struct UdevBackendData {
    session: LibSeatSession,
    devices: HashMap<DrmNode, DeviceData>,
    failed_devices: HashMap<DrmNode, native_outputs::FailedDevice>,
    secondary: native_worker::Broker,
    primary_allocator: Option<Allocator>,
    feedback_gpu: Option<DrmNode>,
    connector_settle: HashMap<DrmNode, (native_policy::Settle, Option<RegistrationToken>)>,
    late_edid: HashMap<DrmNode, RegistrationToken>,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    /// Software-composited cursor for the DRM path. The nested backend gets a host cursor;
    /// hardware scanout does not, so this upload cache belongs to the hardware backend.
    cursor_renderer: crate::cursor::CursorRenderer,
    primary_gpu: DrmNode,
    /// Matches the startup preferred card when EGL resolves its actual render node.
    primary_card_dev_id: u64,
    /// Created once, the first time a primary-GPU device successfully produces a renderer —
    /// `src/backend/winit.rs`'s doc: "the udev backend ... is where DMA-BUF actually matters".
    dmabuf_global: Option<DmabufGlobal>,
    /// Set for the duration of a VT switch away from this session (`Event::PauseSession`) and
    /// cleared on return (`Event::ActivateSession`) — checked by every render/reschedule path so
    /// a stray timer firing mid-pause is a harmless no-op rather than a failed DRM ioctl retried
    /// in a hot loop.
    paused: bool,
    /// `Type=notify` readiness is emitted exactly once, after DRM reports the first queued frame
    /// submitted. A listening socket or initialized CRTC alone does not prove that pixels reached
    /// the display.
    ready_notified: bool,
    /// The event-loop stall and page-flip watchdog (`src/watchdog.rs`); lives as long as the
    /// backend, and `None` only if it could not start.
    watchdog: Option<crate::watchdog::Watchdog>,
}
impl UdevBackendData {
    fn logical_primary_output_id(&self) -> Option<&templar_types::display::DisplayId> {
        native_policy::logical_primary(
            self.devices
                .values()
                .flat_map(|device| {
                    device.catalog.iter().map(|(connector, display)| {
                        let mapped = device.surfaces.values().any(|surface| {
                            surface.connector == *connector
                                && surface.id == display.id
                                && surface.output.current_mode().is_some()
                        });
                        (display, mapped)
                    })
                })
                .chain(self.secondary.logical_outputs()),
        )
    }
}

impl Backend for UdevBackendData {
    fn seat_name(&self) -> String {
        self.session.seat()
    }
    fn session_active(&self) -> bool {
        !self.paused
            && (self.devices.values().any(|device| device.active) || self.secondary.active())
    }
    fn primary_output_name(&self) -> Option<String> {
        self.logical_primary_output_id().map(|id| id.0.clone())
    }
    fn display_inventory(&self) -> Vec<templar_types::display::Display> {
        let mut inventory: Vec<_> = self
            .devices
            .values()
            .flat_map(|device| device.catalog.values().cloned())
            .chain(
                self.failed_devices
                    .values()
                    .flat_map(|device| device.catalog.iter().cloned()),
            )
            .chain(self.secondary.inventory())
            .collect();
        native_policy::project_logical_primary(&mut inventory, self.logical_primary_output_id());
        inventory
    }
    fn display_failures(&self) -> Vec<templar_types::display::DisplayFailure> {
        let mut failures: Vec<_> = self
            .failed_devices
            .values()
            .map(|device| templar_types::display::DisplayFailure {
                gpu: device.gpu.clone(),
                connector: None,
                reason: device.reason.clone(),
            })
            .collect();
        failures.extend(self.secondary.failures());
        for device in self.devices.values() {
            if let Some(reason) = &device.activation_failure {
                failures.push(templar_types::display::DisplayFailure {
                    gpu: device.gpu_id.clone(),
                    connector: None,
                    reason: reason.clone(),
                });
            }
            for (connector, reason) in &device.connector_failures {
                failures.push(templar_types::display::DisplayFailure {
                    gpu: device.gpu_id.clone(),
                    connector: device
                        .catalog
                        .get(connector)
                        .map(|display| display.connector.clone()),
                    reason: reason.clone(),
                });
            }
            for surface in device.surfaces.values() {
                if let Some(reason) = &surface.render_failure {
                    failures.push(templar_types::display::DisplayFailure {
                        gpu: device.gpu_id.clone(),
                        connector: device
                            .catalog
                            .get(&surface.connector)
                            .map(|display| display.connector.clone()),
                        reason: reason.clone(),
                    });
                }
            }
        }
        failures
    }
    fn unknown_physical_outputs(&self) -> bool {
        self.secondary.unknown_physical_outputs()
            || self
                .failed_devices
                .values()
                .any(native_outputs::FailedDevice::unknown_physical_outputs)
            || self.devices.values().any(|device| {
                !device.connector_scan_known
                    || device.catalog.iter().any(|(connector, display)| {
                        display.connected
                            && (!device.active
                                || matches!(
                                    display.physical,
                                    templar_types::display::PhysicalState::Unavailable { .. }
                                )
                                || !matches!(
                                    display.physical,
                                    templar_types::display::PhysicalState::NativeDisabled
                                ) && (!device
                                    .surfaces
                                    .values()
                                    .any(|surface| surface.connector == *connector)
                                    || device.surfaces.values().any(|surface| {
                                        surface.connector == *connector
                                            && surface.render_failure.is_some()
                                    })))
                    })
            })
    }
    fn outputs_blanked(&self) -> bool {
        !self.paused
            && !self.unknown_physical_outputs()
            && self.secondary.blanked()
            && self
                .devices
                .values()
                .flat_map(|device| device.catalog.values())
                .filter(|display| display.connected)
                .all(|display| {
                    matches!(
                        display.physical,
                        templar_types::display::PhysicalState::NativeDisabled
                    )
                })
    }
    fn defer_control(
        state: &mut State,
        request: &templar_types::compositor_control::ControlRequest,
        reply: &mut Option<
            tokio::sync::oneshot::Sender<templar_types::compositor_control::ControlReply>,
        >,
    ) -> Result<bool, templar_types::compositor_control::ControlError> {
        native_worker::defer_control(state, request, reply)
    }
    fn defer_restore(
        state: &mut State,
        profile: &templar_types::display::DisplayProfile,
        token: Option<uuid::Uuid>,
    ) -> Result<bool, templar_types::display::DisplayError> {
        native_worker::begin_restore(state, profile, token, None)
    }
    fn display_restore_pending(&self) -> bool {
        self.secondary.restoring()
    }
    fn validate_displays(
        state: &mut State,
        profile: &templar_types::display::DisplayProfile,
    ) -> Result<(), templar_types::display::DisplayError> {
        native_outputs::test(state, profile)
    }
    fn apply_displays(
        state: &mut State,
        profile: &templar_types::display::DisplayProfile,
    ) -> Result<(), templar_types::display::DisplayError> {
        native_outputs::apply(state, profile)
    }
    fn verify_displays(
        state: &State,
        profile: &templar_types::display::DisplayProfile,
    ) -> Result<(), templar_types::display::DisplayError> {
        native_outputs::verify(state, profile)
    }
    fn request_redraw(state: &mut State) {
        if state.backend_data.paused {
            return;
        }
        native_worker::request_redraw(state);
        for (node, device) in &mut state.backend_data.devices {
            if !device.active {
                continue;
            }
            for (crtc, surface) in &mut device.surfaces {
                surface.drm_output.reset_buffers();
                let (node, crtc) = (*node, *crtc);
                state
                    .loop_handle
                    .insert_idle(move |state| render_surface(state, node, crtc));
            }
        }
    }

    /// Idle blanking on real hardware (`src/idle.rs`): DPMS off, and back on.
    ///
    /// `DrmCompositor::clear` puts the CRTC into DPMS off and disables every plane; the surface
    /// re-enables itself on the next `queue_frame`, which is exactly what waking does. Waking
    /// also has to `reset_buffers` and kick a render by hand, because this backend's redraw
    /// cadence is page-flip completion and a dark output has no flips in flight to carry it (the
    /// same reason `schedule_retry` exists for a damage-free frame). Blanking is not attempted
    /// while the session is paused for a VT switch: the CRTC is not ours to touch then.

    fn set_blanked(state: &mut State, blanked: bool) {
        if state.backend_data.paused {
            return;
        }
        native_worker::set_blanked(state, blanked);
        for (node, device) in &mut state.backend_data.devices {
            for (crtc, surface) in &mut device.surfaces {
                if blanked {
                    match surface.drm_output.clear() {
                        Ok(()) => {
                            // Only a successful clear cancels ownership of an in-flight flip.
                            surface.frame_in_flight = false;
                            if let Some(flip) = &surface.flip {
                                flip.cancel();
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, ?crtc, "physical output clear failed; no blanking proof");
                            if surface.render_failure.is_none() {
                                surface.render_failure =
                                    Some(format!("Actual DRM clear/blank failed: {error}"));
                            }
                        }
                    }
                } else {
                    surface.drm_output.reset_buffers();
                    surface.last_render = Instant::now();
                    let (node, crtc) = (*node, *crtc);
                    state
                        .loop_handle
                        .insert_idle(move |state| render_surface(state, node, crtc));
                }
            }
            native_outputs::refresh_physical(device);
        }
        crate::backend::recompute_outputs(state);
    }
}

type State = TemplarCompState<UdevBackendData>;
type NativeRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// Runs the compositor against real DRM/KMS hardware. Returns an error rather than panicking on
/// any step short of "the event loop itself cannot start" — a broken driver should end the process
/// cleanly (the systemd unit's `Restart=no`, `session/units/user/templar-comp.service`, already
/// treats a compositor exit as session-ending on purpose) rather than take down the session with a
/// backtrace.
// One long, linear "wire everything up" function is the shape every Smithay-based compositor's
// backend entry point takes (`anvil`, `smallvil`) — the alternative is threading a dozen
// half-constructed pieces through several helper functions, which reads worse, not better.
#[allow(clippy::too_many_lines)]
pub fn run(
    exec: Option<&[String]>,
    requested_socket_name: Option<&str>,
    session_activation: bool,
) -> anyhow::Result<i32> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let (session, notifier) = LibSeatSession::new()
        .map_err(|error| anyhow::anyhow!("opening libseat session: {error}"))?;

    let mut paths = all_gpus(session.seat())?;
    paths.sort();
    let primary_card = paths
        .iter()
        .find_map(|path| {
            let node = DrmNode::from_path(path).ok()?;
            let boot = std::fs::read_to_string(format!(
                "/sys/dev/char/{}:{}/device/boot_vga",
                libc::major(node.dev_id()),
                libc::minor(node.dev_id())
            ))
            .is_ok_and(|value| value.trim() == "1");
            boot.then_some(node)
        })
        .or_else(|| {
            paths.iter().find_map(|path| {
                let node = DrmNode::from_path(path).ok()?;
                node.node_with_type(NodeType::Render)
                    .and_then(Result::ok)
                    .map(|_| node)
            })
        })
        .or_else(|| paths.iter().find_map(|path| DrmNode::from_path(path).ok()))
        .ok_or_else(|| anyhow::anyhow!("no GPU found"))?;
    let primary_gpu_path = primary_card
        .node_with_type(NodeType::Render)
        .and_then(Result::ok)
        .unwrap_or(primary_card);
    // Every udev `Added`/device-list entry reports a `NodeType::Primary` ("cardN") dev_id;
    // `primary_gpu_path` above may have been coerced to `NodeType::Render` ("renderDNNN") to
    // resolve DRI device selection, so convert back to compare like with like (falls back to the
    // node's own dev_id for the rare device with no distinct render node at all).
    let primary_card_dev_id = primary_gpu_path
        .node_with_type(NodeType::Primary)
        .and_then(Result::ok)
        .map_or_else(|| primary_gpu_path.dev_id(), |node| node.dev_id());
    let gpus = GpuManager::new(GbmGlesBackend::with_factory(|display| {
        let context = EGLContext::new(display)?;
        // SAFETY: Each factory-created context is owned exclusively by its renderer.
        #[allow(unsafe_code)]
        unsafe {
            Ok(GlesRenderer::new(context)?)
        }
    }))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    let recovery = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(3),
            native_recovery::Lease::start(&session.seat()),
        )
        .await
        .map_err(anyhow::Error::from)?
    });
    let (secondary, secondary_events) = native_worker::Broker::new(handle.clone(), recovery);
    let identity_cache =
        DbusIdentityCache::spawn(&handle, crate::identity_sink::install(&event_loop.handle()));
    let governor = GovernorNotifier::spawn(&handle);
    let idle_notifier = IdleNotifier::spawn(&handle);
    let shell_sink = crate::shell_sink::install(&event_loop.handle());
    let shell_broadcaster = ShellStateBroadcaster::spawn(&handle, shell_sink);

    let socket_name =
        crate::state::init_wayland_listener(display, &event_loop.handle(), requested_socket_name)?;
    tracing::info!(socket = %socket_name, "wayland socket ready");
    if session_activation {
        // Startup only: no D-Bus work is performed on the render/event-loop thread.
        runtime.block_on(templar_comp_integration::activation::publish_session(
            &socket_name,
        ))?;
    }

    // Before any device is opened, so every CRTC gets flip tracking and a hang during the rest of
    // initialization (EGL, the first modeset) is reported like any other stall.
    let watchdog = crate::watchdog::install_for_main_loop(&event_loop.handle());

    let mut state = State::new(
        UdevBackendData {
            session: session.clone(),
            devices: HashMap::new(),
            failed_devices: HashMap::new(),
            secondary,
            primary_allocator: None,
            feedback_gpu: None,
            connector_settle: HashMap::new(),
            late_edid: HashMap::new(),
            gpus,
            cursor_renderer: crate::cursor::CursorRenderer::default(),
            primary_gpu: primary_gpu_path,
            primary_card_dev_id,
            dmabuf_global: None,
            paused: false,
            ready_notified: false,
            watchdog,
        },
        &session.seat(),
        &dh,
        event_loop.handle(),
        event_loop.get_signal(),
        identity_cache,
        governor,
        idle_notifier,
        shell_broadcaster,
    )?;
    state.display_control.set_native();
    native_worker::install(&mut state, secondary_events)?;
    state
        .chrome
        .set_display_recovery(state.backend_data.secondary.notice().to_owned());
    let _control_server = if session_activation {
        Some(crate::compositor_control::install(&mut state, &handle)?)
    } else {
        None
    };

    State::create_private_globals(&dh);
    let shell_socket_name = crate::shell_socket::install(&mut state, &socket_name)?;
    // After the socket is listening, so the child can connect the moment it starts.
    let session_command = exec
        .map(|argv| {
            crate::session_command::SessionCommand::spawn(argv, &socket_name, &event_loop.handle())
        })
        .transpose()?;
    tracing::info!(socket = %shell_socket_name, "trusted-shell socket ready");

    // Session pause/resume: drop and reacquire DRM master. Every currently-tracked device is
    // paused/resumed uniformly; connector rescanning after resume rides on the udev `Changed`
    // event the kernel emits on TTY switch back, same as anvil.
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&state.backend_data.session.seat())
        .map_err(|()| anyhow::anyhow!("failed to assign libinput seat"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, (), state: &mut State| {
            match &event {
                smithay::backend::input::InputEvent::DeviceAdded { device } => {
                    state.track_input_device(device.clone())
                }
                smithay::backend::input::InputEvent::DeviceRemoved { device } => {
                    state.untrack_input_device(device.sysname())
                }
                _ => {}
            }
            match event {
                smithay::backend::input::InputEvent::PointerMotionAbsolute { event } => {
                    let device = smithay::reexports::input::event::EventTrait::device(&event);
                    let output_name = device.output_name();
                    state.process_input_event::<LibinputInputBackend>(
                        smithay::backend::input::InputEvent::PointerMotionAbsolute { event },
                        output_name.as_deref(),
                    );
                }
                event => state.process_input_event(event, None),
            }
        })
        .map_err(|error| anyhow::anyhow!("inserting libinput source: {error}"))?;

    event_loop
        .handle()
        .insert_source(notifier, move |event, (), state: &mut State| match event {
            smithay::backend::session::Event::PauseSession => {
                tracing::info!("session paused (VT switch away): releasing DRM devices");
                state.backend_data.paused = true;
                state.session_lock.reset_coverage();
                native_worker::pause(state);
                libinput_context.suspend();
                for device in state.backend_data.devices.values_mut() {
                    device.drm_output_manager.pause();
                    device.active = false;
                    native_outputs::refresh_catalog(device);
                    // Without DRM master nothing in flight completes, and that is not a fault.
                    for surface in device.surfaces.values_mut() {
                        surface.frame_in_flight = false;
                        if let Some(flip) = &surface.flip {
                            flip.cancel();
                        }
                    }
                }
                crate::backend::recompute_outputs(state);
            }
            smithay::backend::session::Event::ActivateSession => {
                tracing::info!("session resumed (VT switch back): reacquiring DRM devices");
                state.backend_data.paused = false;
                if libinput_context.resume().is_err() {
                    tracing::warn!("failed to resume libinput after session activation");
                }
                let nodes: Vec<DrmNode> = state.backend_data.devices.keys().copied().collect();
                for node in nodes {
                    reactivate_device(state, node);
                }
                let failed: Vec<_> = state
                    .backend_data
                    .failed_devices
                    .iter()
                    .map(|(node, device)| (*node, device.path.clone()))
                    .collect();
                for (node, path) in failed {
                    add_device(state, node, &path);
                }
                native_worker::resume(state);
                crate::backend::recompute_outputs(state);
            }
        })
        .map_err(|error| anyhow::anyhow!("inserting session-event source: {error}"))?;

    let udev_backend = UdevBackend::new(state.backend_data.session.seat())
        .map_err(|error| anyhow::anyhow!("enumerating udev devices: {error}"))?;
    let mut device_order: Vec<_> = udev_backend
        .device_list()
        .map(|(id, path)| (id, path.to_owned()))
        .collect();
    device_order.sort_by_key(|(id, path)| (*id != primary_card_dev_id, path.clone()));
    for (device_id, path) in device_order {
        if let Ok(node) = DrmNode::from_dev_id(device_id) {
            add_device(&mut state, node, &path);
        }
    }
    event_loop
        .handle()
        .insert_source(
            udev_backend,
            move |event, _, state: &mut State| match event {
                UdevEvent::Added { device_id, path } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        add_device(state, node, &path);
                    }
                }
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        settle_connectors(state, node);
                    }
                }
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        remove_device(state, node);
                    }
                }
            },
        )
        .map_err(|error| anyhow::anyhow!("inserting udev source: {error}"))?;

    tracing::info!(gpu = %state.backend_data.primary_gpu, "primary GPU selected");
    if state.backend_data.devices.is_empty() {
        tracing::warn!(
            "no healthy DRM device came up at startup — retaining unavailable GPU diagnostics and \
             waiting for real hotplug; readiness still requires an actual presented frame"
        );
    }
    state.start_xwayland(&event_loop.handle());
    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(REPAINT_CHECK),
            |_, (), state: &mut State| {
                restart_stalled_repaints(state);
                TimeoutAction::ToDuration(REPAINT_CHECK)
            },
        )
        .map_err(|error| anyhow::anyhow!("inserting the repaint check: {error}"))?;

    // READY is sent from `on_vblank`, after DRM confirms the first queued frame was submitted.
    // Until then systemd keeps dependants stopped and enforces the unit's startup timeout.
    tracing::info!("compositor initialization complete; waiting for first presented frame");
    event_loop.run(None, &mut state, |state| {
        // Dispatch only queues protocol events.  Without a flush here, a client doing its first
        // registry/configure round trip waits forever even though its requests were handled —
        // precisely the black-session failure seen on the hardware backend.
        state.space.refresh();
        crate::compositor_control::maintain(state);
        state.popups.cleanup();
        if let Err(error) = state.display_handle.flush_clients() {
            tracing::warn!(%error, "flushing Wayland clients after dispatch");
        }
    })?;
    // The loop has stopped answering probes on purpose; waiting for the `--exec` child below is
    // not a stall.
    drop(state.backend_data.watchdog.take());
    Ok(session_command.map_or(0, |command| command.shut_down()))
}

/// Reactivates one already-tracked device after `Event::ActivateSession` and kicks a fresh render
/// of every surface on it: the DRM state (CRTC↔connector bindings in particular) can have changed
/// under a foreign master while this session held no master at all, so every buffer is treated as
/// stale rather than assumed still valid.
fn reactivate_device(state: &mut State, node: DrmNode) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let activation = device.drm_output_manager.lock().activate(false);
    if let Err(error) = activation {
        device.active = false;
        device.activation_failure = Some(format!("DRM master activation failed: {error}"));
        native_outputs::refresh_catalog(device);
        tracing::warn!(%error, ?node, "failed to reactivate drm device after session resume");
        select_healthy_render_gpu(&mut state.backend_data);
        return;
    }
    device.active = true;
    device.activation_failure = None;
    native_outputs::refresh_catalog(device);
    for (crtc, surface) in &mut device.surfaces {
        if let Err(error) = surface.drm_output.reset_state() {
            surface.render_failure = Some(format!("Primary output resume reset failed: {error}"));
            continue;
        }
        surface.drm_output.reset_buffers();
        surface.last_render = Instant::now();
        let crtc = *crtc;
        state
            .loop_handle
            .insert_idle(move |state| render_surface(state, node, crtc));
    }
    select_healthy_render_gpu(&mut state.backend_data);
    refresh_render_feedback(state);
}

fn select_healthy_render_gpu(backend: &mut UdevBackendData) {
    let eligible = |device: &DeviceData| {
        device.active
            && (device.surfaces.is_empty()
                || device
                    .surfaces
                    .values()
                    .any(|surface| surface.render_failure.is_none()))
    };
    if let Some(healthy) = backend
        .devices
        .iter()
        .find(|(node, device)| eligible(device) && node.dev_id() == backend.primary_card_dev_id)
        .map(|(_, device)| device.render_node)
        .or_else(|| {
            backend
                .devices
                .values()
                .find(|device| eligible(device) && device.render_node == backend.primary_gpu)
                .map(|device| device.render_node)
        })
        .or_else(|| {
            backend
                .devices
                .values()
                .filter(|device| eligible(device))
                .min_by(|left, right| left.gpu_id.cmp(&right.gpu_id))
                .map(|device| device.render_node)
        })
    {
        backend.primary_gpu = healthy;
    }
}

fn refresh_render_feedback(state: &mut State) {
    let node = state.backend_data.primary_gpu;
    if state.backend_data.feedback_gpu == Some(node) {
        return;
    }
    let feedback = match state
        .backend_data
        .gpus
        .single_renderer(&node)
        .map_err(|error| error.to_string())
        .and_then(|renderer| {
            state
                .shm_state
                .update_formats(renderer.as_ref().shm_formats());
            DmabufFeedbackBuilder::new(node.dev_id(), renderer.as_ref().dmabuf_formats())
                .build()
                .map_err(|error| error.to_string())
        }) {
        Ok(feedback) => feedback,
        Err(error) => {
            tracing::warn!(%error, "actual render-GPU dmabuf feedback unavailable");
            return;
        }
    };
    if let Some(global) = &state.backend_data.dmabuf_global {
        state.dmabuf_state.set_default_feedback(global, &feedback);
    } else {
        state.backend_data.dmabuf_global = Some(
            state
                .dmabuf_state
                .create_global_with_default_feedback::<State>(&state.display_handle, &feedback),
        );
    }
    state.backend_data.feedback_gpu = Some(node);
}

fn settle_connectors(state: &mut State, node: DrmNode) {
    let entry = state.backend_data.connector_settle.entry(node).or_default();
    let generation = entry.0.changed(Instant::now());
    if let Some(timer) = entry.1.take() {
        state.loop_handle.remove(timer);
    }
    match state.loop_handle.insert_source(
        Timer::from_duration(native_policy::CONNECTOR_SETTLE),
        move |_, (), state: &mut State| {
            let remaining = state
                .backend_data
                .connector_settle
                .get(&node)
                .and_then(|entry| entry.0.remaining(generation, Instant::now()));
            if let Some(remaining) = remaining.filter(|remaining| !remaining.is_zero()) {
                return TimeoutAction::ToDuration(remaining);
            }
            if remaining.is_some() {
                if let Some(entry) = state.backend_data.connector_settle.get_mut(&node) {
                    entry.1 = None;
                }
                if node.dev_id() != state.backend_data.primary_card_dev_id {
                    native_worker::changed(state, node);
                } else if let Some(path) = state
                    .backend_data
                    .failed_devices
                    .get(&node)
                    .map(|device| device.path.clone())
                {
                    add_device(state, node, &path);
                } else {
                    scan_connectors(state, node);
                }
            }
            TimeoutAction::Drop
        },
    ) {
        Ok(token) => {
            state
                .backend_data
                .connector_settle
                .entry(node)
                .or_default()
                .1 = Some(token);
        }
        Err(error) => {
            tracing::warn!(%error, "connector settle timer unavailable; keeping old state")
        }
    }
}

/// `interface-id` naming (e.g. `eDP-1`, `HDMI-A-1`) via `Interface::as_str`, the same convention
/// `wlr-randr`/every other compositor uses — not `Interface`'s `Debug` form, which would print
/// e.g. `EmbeddedDisplayPort-1` for the exact same connector.
fn output_name(interface: connector::Interface, interface_id: u32) -> String {
    format!("{}-{interface_id}", interface.as_str())
}

/// Where a newly-connected output should sit: immediately to the right of every already-mapped
/// output, or at the origin if there are none yet. Pure over the existing outputs' right edges so
/// it is unit-testable without a real `Space`.
fn next_output_x(existing_right_edges: impl Iterator<Item = i32>) -> i32 {
    existing_right_edges.max().unwrap_or(0)
}

fn add_device(state: &mut State, node: DrmNode, path: &std::path::Path) {
    if node.dev_id() != state.backend_data.primary_card_dev_id {
        native_worker::add(state, node, path);
        return;
    }
    if state.backend_data.devices.contains_key(&node) {
        return;
    }
    match try_add_device(state, node, path) {
        Ok(()) => {
            state.backend_data.failed_devices.remove(&node);
            scan_connectors(state, node);
        }
        Err(reason) => {
            tracing::warn!(%reason, ?path, "GPU unavailable; retaining real connector diagnostics and unknown physical coverage");
            state.backend_data.failed_devices.insert(
                node,
                native_outputs::FailedDevice::observe(node, path, reason),
            );
            crate::backend::recompute_outputs(state);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn try_add_device(state: &mut State, node: DrmNode, path: &std::path::Path) -> Result<(), String> {
    let gpu_id = native_outputs::gpu_identity(node)
        .map_err(|error| format!("Stable DRM physical-device identity unavailable: {error}"))?;

    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = state
        .backend_data
        .session
        .open(path, open_flags)
        .map_err(|error| format!("DRM device open failed: {error}"))?;
    let fd = DrmDeviceFd::new(smithay::utils::DeviceFd::from(fd));
    let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)
        .map_err(|error| format!("DRM device initialization failed: {error}"))?;
    let gbm =
        GbmDevice::new(fd).map_err(|error| format!("GBM device initialization failed: {error}"))?;

    // SAFETY: `gbm` is a live GBM device for the DRM device just opened above; `EGLDisplay::new`'s
    // only requirement is that the native display handed to it (the GBM device) stays valid for
    // the `EGLDisplay`'s own lifetime, which holds here since `gbm` is cloned into it and the
    // original is kept alive below for the allocator/exporter.
    #[allow(unsafe_code)]
    let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }
        .map_err(|error| format!("GPU EGL display unavailable: {error}"))?;
    let render_node = match EGLDevice::device_for_display(&egl_display)
        .and_then(|device| device.try_get_render_node())
    {
        Ok(Some(render_node)) => render_node,
        Ok(None) => node,
        Err(error) => return Err(format!("GPU render-node discovery failed: {error}")),
    };
    state
        .backend_data
        .gpus
        .as_mut()
        .add_node(render_node, gbm.clone())
        .map_err(|error| format!("GPU renderer registration failed: {error}"))?;
    if let Err(error) = state.backend_data.gpus.devices().map(|_| ()) {
        if !state
            .backend_data
            .devices
            .values()
            .any(|device| device.render_node == render_node)
        {
            state.backend_data.gpus.as_mut().remove_node(&render_node);
        }
        return Err(format!("GPU renderer enumeration failed: {error}"));
    }
    let renderer = match state.backend_data.gpus.single_renderer(&render_node) {
        Ok(renderer) => renderer,
        Err(error) => {
            let reason = format!("GPU renderer initialization failed: {error}");
            if !state
                .backend_data
                .devices
                .values()
                .any(|device| device.render_node == render_node)
            {
                state.backend_data.gpus.as_mut().remove_node(&render_node);
            }
            return Err(reason);
        }
    };
    let render_formats = renderer
        .as_ref()
        .egl_context()
        .dmabuf_render_formats()
        .clone();
    drop(renderer);

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    state.backend_data.primary_allocator = Some(allocator.clone());
    let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into());
    // Allocation and import formats belong to this device, not the preferred GPU.

    let drm_output_manager = DrmOutputManager::new(
        drm,
        allocator.clone(),
        exporter.clone(),
        Some(gbm.clone()),
        SUPPORTED_COLOR_FORMATS.iter().copied(),
        render_formats,
    );

    let registration_token = match state.loop_handle.insert_source(
        drm_notifier,
        move |event, metadata, state: &mut State| match event {
            DrmEvent::VBlank(crtc) => {
                on_vblank(state, node, crtc, metadata.as_ref().map(|event| event.time));
            }
            DrmEvent::Error(error) => {
                if let Some(device) = state.backend_data.devices.get_mut(&node) {
                    device.activation_failure = Some(format!("Actual primary DRM device event error: {error}"));
                    for surface in device.surfaces.values_mut() { surface.render_failure = device.activation_failure.clone(); }
                }
                state.display_control.backend_failed(error.to_string());
                crate::compositor_control::refresh_inventory(state, false);
                state.session_lock.sync_physical_state(state.backend_data.unknown_physical_outputs());
                tracing::warn!(%error, ?node, "drm device error event; physical coverage unavailable");
            }
        },
    ) {
        Ok(token) => token,
        Err(error) => {
            if !state.backend_data.devices.values().any(|device| device.render_node == render_node) { state.backend_data.gpus.as_mut().remove_node(&render_node); }
            return Err(format!("DRM event-source registration failed: {error}"));
        }
    };

    state.backend_data.devices.insert(
        node,
        DeviceData {
            drm_output_manager,
            allocator,
            exporter,
            gbm,
            render_node,
            gpu_id,
            catalog: HashMap::new(),
            connector_scan_known: false,
            activation_failure: None,
            connector_failures: HashMap::new(),
            active: true,
            surfaces: HashMap::new(),
            scanner: DrmScanner::new(),
            registration_token,
        },
    );
    select_healthy_render_gpu(&mut state.backend_data);
    refresh_render_feedback(state);
    Ok(())
}

fn scan_connectors(state: &mut State, node: DrmNode) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let mut scan_result = match device
        .scanner
        .scan_connectors(device.drm_output_manager.device())
    {
        Ok(scan) => {
            device.activation_failure = None;
            device.connector_scan_known = true;
            scan.into_iter()
        }
        Err(error) => {
            device.activation_failure =
                Some(format!("Kernel connector inventory unavailable: {error}"));
            device.connector_scan_known = false;
            tracing::warn!(%error, ?node, "connector scan failed; physical coverage remains unknown");
            crate::backend::recompute_outputs(state);
            return;
        }
    };
    scan_result.as_mut_slice().sort_by_key(|event| match event {
        smithay_drm_extras::drm_scanner::DrmScanEvent::Disconnected { connector, .. } => {
            (0, false, u32::from(connector.handle()))
        }
        smithay_drm_extras::drm_scanner::DrmScanEvent::Connected { connector, .. }
        | smithay_drm_extras::drm_scanner::DrmScanEvent::Changed { connector, .. } => {
            let internal = matches!(
                connector.interface(),
                connector::Interface::EmbeddedDisplayPort
                    | connector::Interface::LVDS
                    | connector::Interface::DSI
            );
            (1, !internal, u32::from(connector.handle()))
        }
    });
    native_outputs::refresh_catalog(device);
    let needs_late_edid = device.scanner.connectors().values().any(|connector| {
        connector.state() == connector::State::Connected && connector.modes().is_empty()
    });
    if needs_late_edid && !state.backend_data.late_edid.contains_key(&node) {
        match state.loop_handle.insert_source(
            Timer::from_duration(native_policy::LATE_EDID_REPROBE),
            move |_, (), state: &mut State| {
                scan_connectors(state, node);
                TimeoutAction::Drop
            },
        ) {
            Ok(token) => {
                state.backend_data.late_edid.insert(node, token);
            }
            Err(error) => tracing::warn!(%error, "bounded late-EDID reprobe unavailable"),
        }
    }
    for event in scan_result {
        match event {
            smithay_drm_extras::drm_scanner::DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(state, node, &connector, crtc, None),
            smithay_drm_extras::drm_scanner::DrmScanEvent::Disconnected {
                connector: _,
                crtc: Some(crtc),
            } => connector_disconnected(state, node, crtc),
            smithay_drm_extras::drm_scanner::DrmScanEvent::Changed {
                connector,
                crtc: Some(crtc),
            } => {
                let previous = state
                    .backend_data
                    .devices
                    .get(&node)
                    .and_then(|device| device.catalog.get(&connector.handle()))
                    .map(|display| display.current.clone());
                if previous
                    .as_ref()
                    .is_some_and(|configuration| !configuration.enabled)
                    && !state.backend_data.devices.get(&node).is_some_and(|device| {
                        device.connector_failures.contains_key(&connector.handle())
                    })
                {
                    continue;
                }
                if state.display_control.panel_closed()
                    && state
                        .backend_data
                        .devices
                        .get(&node)
                        .and_then(|device| device.catalog.get(&connector.handle()))
                        .is_some_and(|display| display.internal)
                {
                    continue;
                }
                connector_disconnected(state, node, crtc);
                let previous = previous.filter(|configuration| {
                    configuration.enabled
                        && connector.modes().iter().any(|mode| {
                            configuration
                                .mode
                                .as_ref()
                                .is_some_and(|id| *id == native_outputs::mode_id(mode))
                        })
                });
                connector_connected(state, node, &connector, crtc, previous.as_ref());
            }
            smithay_drm_extras::drm_scanner::DrmScanEvent::Connected {
                connector,
                crtc: None,
            }
            | smithay_drm_extras::drm_scanner::DrmScanEvent::Changed {
                connector,
                crtc: None,
            } => {
                if let Some(device) = state.backend_data.devices.get_mut(&node) {
                    connector_failed(
                        device,
                        connector.handle(),
                        "The kernel advertises a connected head but no usable CRTC route".into(),
                    );
                }
            }
            _ => {}
        }
    }
    crate::backend::recompute_outputs(state);
    crate::compositor_control::maybe_emergency_fallback(state);
}

// See `add_device`'s identical `#[allow]`: one connector's worth of "compute its mode, register
// its `Output`, hand it a real scanout surface" reads worse split apart than kept linear.
#[allow(clippy::too_many_lines)]
fn connector_connected(
    state: &mut State,
    node: DrmNode,
    connector: &connector::Info,
    crtc: crtc::Handle,
    configuration: Option<&templar_types::display::OutputConfiguration>,
) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let Some(definition) = device.catalog.get(&connector.handle()) else {
        tracing::warn!(
            ?node,
            "connector is missing its authoritative native inventory record"
        );
        return;
    };
    if configuration.is_none() && state.display_control.panel_closed() && definition.internal {
        return;
    }
    let output_name = definition.id.0.clone();
    let make = definition.make.clone();
    let model = definition.model.clone();
    let serial = definition.serial.clone().unwrap_or_default();
    let mut effective = configuration
        .cloned()
        .unwrap_or_else(|| definition.current.clone());

    let selected = match configuration {
        Some(configuration) => connector
            .modes()
            .iter()
            .find(|mode| {
                configuration
                    .mode
                    .as_ref()
                    .is_some_and(|id| *id == native_outputs::mode_id(mode))
            })
            .copied(),
        None => native_policy::select_mode(
            connector.modes(),
            matches!(
                connector.interface(),
                connector::Interface::HDMIA | connector::Interface::HDMIB
            ),
        ),
    };
    let Some(drm_mode) = selected else {
        connector_failed(
            device,
            connector.handle(),
            "No usable exact signal timing; the physical image was not cleared".into(),
        );
        return;
    };
    let wl_mode = Mode::from(drm_mode);

    let x = next_output_x(
        state
            .space
            .outputs()
            .filter_map(|o| crate::viewport::output_geometry(&state.space, o))
            .map(|geo| geo.loc.x + geo.size.w),
    );
    if configuration.is_none() {
        effective.x = x;
        effective.y = 0;
        effective.primary = node.dev_id() == state.backend_data.primary_card_dev_id
            && !device
                .catalog
                .values()
                .any(|display| display.current.enabled && display.current.primary);
    }
    effective.enabled = true;
    effective.mode = Some(native_outputs::mode_id(&drm_mode));

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make,
            model,
            serial_number: serial,
        },
    );
    let global = output.create_global::<State>(&state.display_handle);
    output.change_current_state(
        Some(wl_mode),
        Some(native_outputs::transform(effective.rotation)),
        Some(smithay::output::Scale::Fractional(effective.scale)),
        Some((effective.x, effective.y).into()),
    );
    for mode in connector.modes() {
        output.add_mode(Mode::from(*mode));
        if mode
            .mode_type()
            .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
        {
            output.set_preferred(Mode::from(*mode));
        }
    }
    crate::viewport::configure(&output, effective.insets, false);
    state.space.map_output(&output, (effective.x, effective.y));

    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        state.space.unmap_output(&output);
        state.display_handle.remove_global::<State>(global);
        return;
    };
    let drm_device = device.drm_output_manager.device_mut();
    let driver = match drm_device.get_driver() {
        Ok(driver) => driver,
        Err(error) => {
            connector_failed(
                device,
                connector.handle(),
                format!("DRM driver query failed: {error}"),
            );
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };
    let mut planes = match drm_device.planes(&crtc) {
        Ok(planes) => planes,
        Err(error) => {
            connector_failed(
                device,
                connector.handle(),
                format!("CRTC plane discovery failed: {error}"),
            );
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };
    // Using an overlay plane on an nvidia card is known to break `DrmCompositor`'s plane
    // assignment (anvil and cosmic-comp both work around this the same way) — exactly the hybrid
    // Intel+NVIDIA laptop this backend targets, on the connector actually driven by the primary
    // (Intel) GPU.
    let driver_name = driver.name().to_string_lossy().to_lowercase();
    let driver_description = driver.description().to_string_lossy().to_lowercase();
    if driver_name.contains("nvidia") || driver_description.contains("nvidia") {
        planes.overlay.clear();
    }

    let renderer = match state.backend_data.gpus.single_renderer(&device.render_node) {
        Ok(renderer) => renderer,
        Err(error) => {
            connector_failed(
                device,
                connector.handle(),
                format!("Primary output renderer unavailable: {error}"),
            );
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };
    // DrmOutputManager's bandwidth fallback hides an untimed sync.wait(). Use the direct
    // compositor: TEST_ONLY rejects unsupported combinations without a blocking fallback.
    let drm_output = drm_device
        .create_surface(crtc, drm_mode, &[connector.handle()])
        .map_err(|error| error.to_string())
        .and_then(|raw| {
            DrmCompositor::new(
                &output,
                raw,
                Some(planes),
                device.allocator.clone(),
                device.exporter.clone(),
                SUPPORTED_COLOR_FORMATS.iter().copied(),
                renderer
                    .as_ref()
                    .egl_context()
                    .dmabuf_render_formats()
                    .iter()
                    .copied(),
                drm_device.cursor_size(),
                Some(device.gbm.clone()),
            )
            .map_err(|error| error.to_string())
        });

    let drm_output = match drm_output {
        Ok(drm_output) => drm_output,
        Err(error) => {
            let reason = format!(
                "DRM output initialization failed; the physical image was not cleared: {error}"
            );
            drop(renderer);
            connector_failed(device, connector.handle(), reason);
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };

    let flip = state
        .backend_data
        .watchdog
        .as_ref()
        .map(|watchdog| watchdog.track(format!("{output_name}:{}", u32::from(crtc))));
    device.surfaces.insert(
        crtc,
        SurfaceData {
            output,
            global: Some(global),
            drm_output,
            id: effective.id.clone(),
            connector: connector.handle(),
            preview_for_next_frame: None,
            transaction_for_next_frame: None,
            physical_readback_pending: true,
            render_failure: None,
            flip,
            frame_in_flight: false,
            rearmed_at: Some(crate::flip_recovery::RearmedAt {
                monotonic: state.clock.now().into(),
                realtime: std::time::SystemTime::now(),
            }),
            drop_completion_once: std::env::var_os("TEMPLAROS_TEST_DROP_PAGEFLIP_ONCE").is_some(),
            retry: None,
            last_render: Instant::now(),
        },
    );
    if let Some(definition) = device.catalog.get_mut(&connector.handle()) {
        definition.current = effective;
    }
    drop(renderer);
    device.connector_failures.remove(&connector.handle());
    native_outputs::refresh_physical(device);
    tracing::info!(%output_name, ?crtc, "output lit");

    state
        .loop_handle
        .insert_idle(move |state| render_surface(state, node, crtc));
}

fn connector_failed(device: &mut DeviceData, connector: connector::Handle, reason: String) {
    tracing::warn!(%reason, ?connector, "connector unavailable; physical image not attested");
    device.connector_failures.insert(connector, reason.clone());
    native_outputs::refresh_physical(device);
    if let Some(display) = device.catalog.get_mut(&connector) {
        display.configure = templar_types::settings::Capability::Unavailable { reason };
    }
}

fn connector_disconnected(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    if let Some(surface) = device.surfaces.remove(&crtc) {
        surface.output.leave_all();
        state.space.unmap_output(&surface.output);
        state.chrome.forget_output(&surface.output);
        if let Some(global) = surface.global {
            state.display_handle.remove_global::<State>(global);
        }
        // `surface.drm_output` drops here, which removes this crtc's `DrmCompositor` from the
        // device's shared compositor map (`DrmOutput`'s own `Drop` impl) — no separate teardown
        // call needed.
    }
}

fn remove_device(state: &mut State, node: DrmNode) {
    if node.dev_id() != state.backend_data.primary_card_dev_id {
        native_worker::remove(state, node);
        return;
    }
    if let Some((_, Some(timer))) = state.backend_data.connector_settle.remove(&node) {
        state.loop_handle.remove(timer);
    }
    if let Some(timer) = state.backend_data.late_edid.remove(&node) {
        state.loop_handle.remove(timer);
    }
    state.backend_data.failed_devices.remove(&node);
    if let Some(mut device) = state.backend_data.devices.remove(&node) {
        for (_, surface) in device.surfaces.drain() {
            surface.output.leave_all();
            state.space.unmap_output(&surface.output);
            state.chrome.forget_output(&surface.output);
            if let Some(global) = surface.global {
                state.display_handle.remove_global::<State>(global);
            }
        }
        state.loop_handle.remove(device.registration_token);
        if !state
            .backend_data
            .devices
            .values()
            .any(|other| other.render_node == device.render_node)
        {
            if let Ok(mut renderer) = state.backend_data.gpus.single_renderer(&device.render_node) {
                let context = smithay::backend::renderer::Renderer::context_id(renderer.as_mut());
                state.wallpaper.release_context(&context);
            }
            state
                .backend_data
                .gpus
                .as_mut()
                .remove_node(&device.render_node);
        }
        select_healthy_render_gpu(&mut state.backend_data);
    }
    crate::backend::recompute_outputs(state);
    crate::compositor_control::maybe_emergency_fallback(state);
}

/// Marks the just-completed frame submitted (releasing its buffer back to the swapchain) and
/// immediately renders and queues the next one — this is what paces redraws to the display's real
/// refresh rate instead of a fixed-interval timer (module doc).
fn on_vblank(
    state: &mut State,
    node: DrmNode,
    crtc: crtc::Handle,
    timestamp: Option<smithay::backend::drm::DrmEventTime>,
) {
    let submitted = state
        .backend_data
        .devices
        .get_mut(&node)
        .and_then(|device| device.surfaces.get_mut(&crtc))
        .filter(|surface| surface.frame_in_flight)
        .and_then(|surface| {
            if surface.drop_completion_once {
                surface.drop_completion_once = false;
                tracing::warn!(?crtc, "fault injection: dropping one page-flip completion");
                return None;
            }
            if surface
                .rearmed_at
                .is_some_and(|cutoff| !cutoff.accepts(timestamp))
            {
                return None;
            }
            surface.frame_in_flight = false;
            if let Some(flip) = &surface.flip {
                flip.completed();
            }
            Some((
                surface.output.clone(),
                surface.id.clone(),
                surface.drm_output.frame_submitted(),
            ))
        });
    match submitted {
        Some((output, id, Ok(metadata))) => {
            let physical_changed =
                state
                    .backend_data
                    .devices
                    .get_mut(&node)
                    .is_some_and(|device| {
                        let recovered = device
                            .surfaces
                            .get_mut(&crtc)
                            .is_some_and(|surface| surface.render_failure.take().is_some());
                        let needs_readback =
                            device.surfaces.get_mut(&crtc).is_some_and(|surface| {
                                std::mem::take(&mut surface.physical_readback_pending)
                            });
                        if needs_readback {
                            native_outputs::refresh_physical(device);
                        }
                        needs_readback || recovered
                    });
            if physical_changed {
                crate::compositor_control::refresh_inventory(state, false);
                state
                    .session_lock
                    .sync_physical_state(state.backend_data.unknown_physical_outputs());
            }
            state.session_lock.frame_presented(
                &output,
                metadata
                    .as_ref()
                    .and_then(|metadata| metadata.lock_generation),
            );
            let preview = metadata.as_ref().and_then(|metadata| metadata.preview);
            let transaction = metadata.as_ref().and_then(|metadata| metadata.transaction);
            if !native_worker::defer_presentation(state, &id, transaction, preview) {
                state
                    .display_control
                    .frame_presented(&id, preview, Instant::now());
            }
            if let Some((name, revision)) = metadata.and_then(|metadata| metadata.wallpaper) {
                state.wallpaper.frame_presented(&name, Some(revision));
            }
            crate::notify_ready_once(&mut state.backend_data.ready_notified);
        }
        Some((_, _, Err(error))) => {
            tracing::warn!(%error, ?crtc, "failed to mark drm frame submitted");
            if let Some(surface) = state
                .backend_data
                .devices
                .get_mut(&node)
                .and_then(|device| device.surfaces.get_mut(&crtc))
            {
                if surface.render_failure.is_none() {
                    surface.render_failure = Some(format!(
                        "Actual DRM submitted-frame acknowledgement failed: {error}"
                    ));
                }
            }
            crate::compositor_control::refresh_inventory(state, false);
            state
                .session_lock
                .sync_physical_state(state.backend_data.unknown_physical_outputs());
        }
        None => return,
    }
    render_surface(state, node, crtc);
}

/// Renders one CRTC's surface and, unless a flip was actually queued (in which case `on_vblank`
/// will drive the next frame when it completes), falls back to a timer retry.
///
/// While a frame is in flight its completion owns the next render, so a redraw request, a wake
/// or a stale retry arriving meanwhile does nothing: rendering then would only stack a second,
/// independent repaint sequence on the first.
fn render_surface(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    if state.backend_data.paused {
        return;
    }
    let Some((output, had_failure)) = state
        .backend_data
        .devices
        .get(&node)
        .filter(|device| device.active)
        .and_then(|device| device.surfaces.get(&crtc))
        .filter(|surface| !surface.frame_in_flight)
        .map(|surface| (surface.output.clone(), surface.render_failure.is_some()))
    else {
        return;
    };

    let retry = render_and_queue(state, node, crtc, &output);
    let has_failure = state
        .backend_data
        .devices
        .get(&node)
        .and_then(|device| device.surfaces.get(&crtc))
        .is_some_and(|surface| surface.render_failure.is_some());
    if has_failure != had_failure {
        select_healthy_render_gpu(&mut state.backend_data);
        crate::compositor_control::refresh_inventory(state, false);
        state
            .session_lock
            .sync_physical_state(state.backend_data.unknown_physical_outputs());
    }

    // A client which committed a frame is entitled to its next wl_surface.frame callback once
    // that output has been rendered.  Without this, animated or data-refreshed shell surfaces
    // paint once and then stop scheduling frames on real hardware.
    for window in state.space.elements() {
        window.send_frame(
            &output,
            state.start_time.elapsed(),
            None,
            smithay::desktop::utils::surface_primary_scanout_output,
        );
    }

    if retry {
        schedule_retry(state, node, crtc);
    }
}

fn track_primary(
    space: &smithay::desktop::Space<smithay::desktop::Window>,
    output: &Output,
    states: &smithay::backend::renderer::element::RenderElementStates,
) {
    for window in space.elements() {
        window.with_surfaces(|surface, data| {
            smithay::desktop::utils::update_surface_primary_scanout_output(
                surface,
                output,
                data,
                None,
                states,
                smithay::backend::renderer::element::default_primary_scanout_output_compare,
            );
        });
    }
}

/// Builds this frame's elements, renders them through `DrmOutput`'s damage tracker, and queues a
/// page flip if anything actually changed. Returns whether the caller should fall back to a timer
/// retry: true on any failure or a damage-free frame (nothing queued, so no page-flip completion
/// will come to drive the next attempt), false once a flip is queued.
fn render_and_queue(state: &mut State, node: DrmNode, crtc: crtc::Handle, output: &Output) -> bool {
    // A blanked output stays blanked: queueing a frame is what would re-enable DPMS
    // (`DrmCompositor::clear`'s contract), so the idle blank has to be checked here rather than
    // only where it was decided.
    if state.idle.blanked() {
        return false;
    }
    let started = Instant::now();
    // Finished park/unpark motions go before the frame is built (`src/motion.rs`). Nothing has
    // to ask for the next frame while one runs: this render chain already draws every refresh,
    // and the motion's own damage keeps flips queued until it ends.
    let now_ns = crate::window_bridge::now_monotonic_ns(&state.clock);
    let mirror_source = if state.session_lock.locked {
        None
    } else {
        state
            .backend_data
            .devices
            .get(&node)
            .and_then(|device| {
                device
                    .surfaces
                    .get(&crtc)
                    .and_then(|surface| device.catalog.get(&surface.connector))
            })
            .and_then(|display| display.current.mirror_of.as_ref())
    };
    let scene_output = if let Some(source) = mirror_source {
        let Some(source_output) = state
            .space
            .outputs()
            .find(|candidate| candidate.name() == source.0)
        else {
            return true;
        };
        source_output
    } else {
        output
    };
    let mirrored = scene_output != output;
    let primary_capture = state
        .backend_data
        .logical_primary_output_id()
        .is_some_and(|id| id.0 == output.name());
    state.motion.prune(now_ns);
    let backend = &mut state.backend_data;
    let Some(device) = backend.devices.get_mut(&node) else {
        return false;
    };
    if let Some(surface) = device.surfaces.get_mut(&crtc) {
        surface.last_render = started;
    }
    let render_node = device.render_node;
    // Secondary devices never enter this manager. Its cross-GPU fallback includes untimed
    // waits and CPU readback in pinned Smithay and is not an admitted input-thread path.
    let mut renderer = match backend.gpus.single_renderer(&render_node) {
        Ok(renderer) => renderer,
        Err(error) => {
            tracing::warn!(%error, ?node, ?crtc, "multi-GPU renderer unavailable");
            if let Some(surface) = device.surfaces.get_mut(&crtc) {
                if surface.render_failure.is_none() {
                    surface.render_failure =
                        Some(format!("Actual multi-GPU renderer unavailable: {error}"));
                }
            }
            return true;
        }
    };
    let renderer = &mut renderer;

    let cursor_elements = state
        .seat
        .get_pointer()
        .zip(crate::viewport::output_geometry(&state.space, scene_output))
        .map_or_else(Vec::new, |(pointer, geometry)| {
            backend.cursor_renderer.elements(
                renderer,
                &mut state.cursor_status,
                pointer.current_location(),
                geometry,
                smithay::utils::Scale::from(scene_output.current_scale().fractional_scale()),
            )
        });

    // `state.chrome`/`state.space`/`state.window_service`/... are fields of `state` disjoint
    // from `state.backend_data` (which `device`/`renderer` above borrow from) — Rust's borrow
    // checker tracks that disjointness through this whole function, which is exactly why
    // `crate::scene::collect` takes these pieces individually (`ChromeInputs`) rather than
    // `&TemplarCompState<B>` as a whole (see that type's doc).
    let scene_elements = crate::scene::collect(
        &mut state.chrome,
        &mut state.wallpaper,
        &mut state.essence_renderer,
        &state.windows,
        &crate::chrome::ChromeInputs {
            space: &state.space,
            window_service: &state.window_service,
            tab_counts: &state.tab_metadata.counts,
            recovery_protocol: &state.recovery_protocol,
            session_lock: &state.session_lock,
            switcher: state.switcher.as_ref(),
            name_tab_menu: &state.name_tab_menu,
            drop_target: state.drop_target,
            gesture_gate: &state.gesture_gate,
            motions: &state.motion,
            now_ns,
        },
        scene_output,
        renderer,
    );

    let calibrated = !crate::viewport::insets(output).is_zero();
    let identification = if state.session_lock.locked {
        None
    } else {
        device
            .surfaces
            .get(&crtc)
            .and_then(|surface| state.display_control.identification_for(&surface.id))
            .and_then(|(number, label, calibration)| {
                crate::chrome::collect_identification(
                    &mut state.chrome,
                    output,
                    number,
                    label,
                    calibration,
                    renderer,
                )
            })
    };
    let identifying = identification.is_some();
    let mut elements: Vec<crate::scene::NativeSceneElement<NativeRenderer<'_>>> =
        Vec::with_capacity(cursor_elements.len() + scene_elements.len() + usize::from(identifying));
    if let Some(identification) = identification {
        elements.push(crate::scene::NativeSceneElement::Identification(
            identification,
        ));
    }
    let scene = cursor_elements
        .into_iter()
        .map(crate::scene::SceneElement::Cursor)
        .chain(scene_elements);
    if calibrated || mirrored {
        let Some(scene) = crate::viewport::constrain_elements_at_scale(
            output,
            scene,
            scene_output.current_scale().fractional_scale(),
        ) else {
            return true;
        };
        elements.extend(scene.map(crate::scene::NativeSceneElement::Calibrated));
    } else {
        elements.extend(scene.map(crate::scene::NativeSceneElement::Plain));
    }

    // Captures use ONLY the primary renderer. Home previews drain once on the
    // logical primary output; other primary-GPU outputs may serve screenshots.
    let captures_started = Instant::now();
    crate::screencopy::drain_pending(
        &mut state.screencopy,
        &state.windows,
        &state.clock,
        state.start_time.elapsed(),
        renderer,
        output,
        &elements,
        primary_capture,
    );
    let captures = captures_started.elapsed();

    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return false;
    };

    if state.session_lock.requires_protected_frame(output) || surface.render_failure.is_some() {
        // Even an already-dark, damage-free desktop needs one buffer of this lock generation.
        surface.drm_output.reset_buffers();
    }
    surface.preview_for_next_frame = device
        .catalog
        .get(&surface.connector)
        .and_then(|display| state.display_control.queued_token_for(&display.current));
    surface.transaction_for_next_frame = device
        .catalog
        .get(&surface.connector)
        .and_then(|display| backend.secondary.transaction_for(&display.current));
    let frame = surface
        .drm_output
        .render_frame(
            renderer,
            &elements,
            if calibrated || state.session_lock.locked {
                [0.0, 0.0, 0.0, 1.0]
            } else {
                CLEAR_COLOR
            },
            if calibrated || mirrored || identifying || state.session_lock.locked {
                FrameFlags::empty()
            } else {
                FrameFlags::DEFAULT
            },
        )
        .map_err(anyhow::Error::from)
        .and_then(|result| {
            let empty = result.is_empty;
            if !empty
                && result.needs_sync()
                && let smithay::backend::drm::compositor::PrimaryPlaneElement::Swapchain(element) =
                    result.primary_element
            {
                native_fence::bounded(&element.sync).map_err(anyhow::Error::msg)?;
            }
            track_primary(&state.space, output, &result.states);
            let wallpaper = if state.session_lock.locked {
                None
            } else {
                state
                    .wallpaper
                    .frame_token(&scene_output.name(), &result.states)
                    .map(|revision| (scene_output.name(), revision))
            };
            Ok((empty, wallpaper))
        });
    let retry = match frame {
        Ok((true, _)) => true,
        Ok((false, wallpaper)) => {
            surface.physical_readback_pending |= surface.render_failure.is_some();
            let retry = queue_rendered(
                surface,
                crtc,
                state.session_lock.frame_generation(),
                wallpaper,
            );
            if retry {
                state.wallpaper.frame_failed(
                    &scene_output.name(),
                    surface
                        .render_failure
                        .as_deref()
                        .unwrap_or("DRM wallpaper frame queue failed"),
                );
            }
            retry
        }
        Err(error) => {
            tracing::warn!(%error, ?crtc, "drm render_frame failed");
            state.wallpaper.frame_failed(&scene_output.name(), &error);
            if surface.render_failure.is_none() {
                surface.render_failure =
                    Some(format!("Actual DRM frame rendering failed: {error}"));
            }
            true
        }
    };

    let total = started.elapsed();
    if total >= SLOW_FRAME {
        tracing::warn!(
            ?crtc,
            total_ms = u64::try_from(total.as_millis()).unwrap_or(u64::MAX),
            captures_ms = u64::try_from(captures.as_millis()).unwrap_or(u64::MAX),
            elements = elements.len(),
            "slow frame"
        );
    }
    retry
}

/// Queues the frame `render_frame` just produced for scanout. Returns whether the caller should
/// fall back to a timer retry (the queue failed), as [`render_and_queue`] does.
fn queue_rendered(
    surface: &mut SurfaceData,
    crtc: crtc::Handle,
    lock_generation: Option<u64>,
    wallpaper: Option<(String, templar_types::settings::Revision)>,
) -> bool {
    match surface.drm_output.queue_frame(FrameUserData {
        lock_generation,
        preview: surface.preview_for_next_frame.take(),
        wallpaper,
        transaction: surface.transaction_for_next_frame.take(),
    }) {
        Ok(()) => {
            surface.frame_in_flight = true;
            if let Some(flip) = &surface.flip {
                flip.queued();
            }
            false
        }
        Err(error) => {
            tracing::warn!(%error, ?crtc, "failed to queue drm frame for scanout");
            if surface.render_failure.is_none() {
                surface.render_failure = Some(format!("Actual DRM frame queue failed: {error}"));
            }
            true
        }
    }
}

/// One nominal frame interval after this call, retries rendering `crtc` — the fallback for the
/// two cases page-flip completion cannot drive: a damage-free frame (nothing queued, so no vblank
/// is coming) and a `render_frame`/`queue_frame` failure. Falls back to a plain 16ms poll if the
/// output's mode isn't known yet. A retry already pending for `crtc` is the one retry: nothing is
/// stacked on it.
fn schedule_retry(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    if state.backend_data.paused || state.idle.blanked() {
        return;
    }
    let Some(surface) = state
        .backend_data
        .devices
        .get(&node)
        .and_then(|device| device.surfaces.get(&crtc))
    else {
        return;
    };
    if surface.retry.is_some() {
        return;
    }
    let delay = surface
        .output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
        .map_or(Duration::from_millis(16), |mode| {
            Duration::from_secs_f64(1000.0 / f64::from(mode.refresh))
        });
    match state.loop_handle.insert_source(
        Timer::from_duration(delay),
        move |_, (), state: &mut State| {
            if let Some(surface) = state
                .backend_data
                .devices
                .get_mut(&node)
                .and_then(|device| device.surfaces.get_mut(&crtc))
            {
                surface.retry = None;
            }
            render_surface(state, node, crtc);
            TimeoutAction::Drop
        },
    ) {
        Ok(token) => {
            if let Some(surface) = state
                .backend_data
                .devices
                .get_mut(&node)
                .and_then(|device| device.surfaces.get_mut(&crtc))
            {
                surface.retry = Some(token);
            }
        }
        Err(error) => tracing::warn!(%error, ?crtc, "failed to schedule drm repaint retry"),
    }
}

/// A healthy chain renders at least every frame interval. An overdue pending flip is recovered
/// by synchronously clearing scanout (which safely clears Smithay's pending frame), resetting
/// damage, then re-queuing. Never synthesize frame_submitted while DRM still owns that buffer.
fn restart_stalled_repaints(state: &mut State) {
    native_worker::maintain(state);
    if state.backend_data.paused || state.idle.blanked() {
        return;
    }
    let mut rearmed = Vec::new();
    for (node, device) in &mut state.backend_data.devices {
        for (crtc, surface) in &mut device.surfaces {
            if !surface.frame_in_flight || surface.last_render.elapsed() < REPAINT_STALL {
                continue;
            }
            let recovered = crate::flip_recovery::rearm(&mut surface.frame_in_flight, || {
                surface.drm_output.clear()?;
                surface.drm_output.reset_state()
            });
            match recovered {
                Ok(()) => {
                    surface.rearmed_at = Some(crate::flip_recovery::RearmedAt {
                        monotonic: Duration::from_nanos(crate::window_bridge::now_monotonic_ns(
                            &state.clock,
                        )),
                        realtime: std::time::SystemTime::now(),
                    });
                    surface.drm_output.reset_buffers();
                    if let Some(flip) = &surface.flip {
                        flip.cancel();
                    }
                    surface.last_render = Instant::now();
                    rearmed.push((*node, *crtc));
                    tracing::warn!(
                        ?crtc,
                        "overdue page flip recovered: scanout cleared safely; re-arming repaint"
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, ?crtc, "overdue page flip clear failed; pending buffer retained");
                }
            }
        }
    }
    for (node, crtc) in rearmed {
        render_surface(state, node, crtc);
    }
    let stalled: Vec<(DrmNode, crtc::Handle, Duration)> = state
        .backend_data
        .devices
        .iter()
        .flat_map(|(node, device)| {
            device.surfaces.iter().filter_map(move |(crtc, surface)| {
                let quiet = surface.last_render.elapsed();
                (!surface.frame_in_flight && surface.retry.is_none() && quiet >= REPAINT_STALL)
                    .then_some((*node, *crtc, quiet))
            })
        })
        .collect();
    for (node, crtc, quiet) in stalled {
        tracing::warn!(
            ?crtc,
            quiet_ms = u64::try_from(quiet.as_millis()).unwrap_or(u64::MAX),
            "output repaint stalled: nothing rendered, nothing in flight, no retry pending; restarting it"
        );
        render_surface(state, node, crtc);
    }
}

#[cfg(test)]
mod tests {
    use super::{next_output_x, output_name};

    #[test]
    fn next_output_x_places_first_output_at_origin() {
        assert_eq!(next_output_x(std::iter::empty()), 0);
    }

    #[test]
    fn next_output_x_places_after_the_rightmost_existing_output() {
        assert_eq!(next_output_x([1920, 1280].into_iter()), 1920);
    }

    #[test]
    fn next_output_x_ignores_a_lower_left_edge_than_the_max() {
        // Three outputs at x=0 (width 1920), x=1920 (width 1080) and one already removed —
        // the next one belongs after the rightmost surviving right edge, not the sum of widths.
        assert_eq!(next_output_x([1920, 3000, 800].into_iter()), 3000);
    }

    #[test]
    fn output_name_uses_the_short_connector_form_not_debug() {
        assert_eq!(
            output_name(
                smithay::reexports::drm::control::connector::Interface::HDMIA,
                1
            ),
            "HDMI-A-1"
        );
        assert_eq!(
            output_name(
                smithay::reexports::drm::control::connector::Interface::EmbeddedDisplayPort,
                1
            ),
            "eDP-1"
        );
    }
}
