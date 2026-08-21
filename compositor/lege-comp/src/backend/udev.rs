//! The udev/KMS backend: real hardware, via `LibSeatSession` and atomic-modesetting DRM. Modelled
//! closely on `anvil/src/udev.rs` (MIT, ADR-0012) with its multi-GPU render-node juggling,
//! VRR/direct-scanout and debug-overlay machinery stripped out — this crate targets one primary
//! GPU correctly, not every GPU configuration anvil handles (see [`is_primary_gpu_dev`]: every
//! other DRM device udev reports, e.g. the NVIDIA half of a hybrid Intel+NVIDIA laptop, is logged
//! and ignored rather than juggled). It cannot be exercised or hardware-tested from the
//! development session this crate was written in (no DRM device this session can take
//! modesetting master on); it is written to be correct by construction against the pinned
//! Smithay API and to fail loudly (a logged error, not a panic that takes the session down) if a
//! step it depends on is unavailable, rather than run.
//!
//! Every output-topology-affecting event below funnels through the same
//! `crate::backend::recompute_outputs` entry point winit uses — connector hotplug, mode changes
//! and session resume all call it, never touching `WindowService` directly (COMPOSITOR.md's "the
//! only place `Output`/topology changes enter `lege-window`").
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

use crate::state::{Backend, LegeCompState};
use lege_comp_integration::{DbusIdentityCache, GovernorNotifier, ShellStateBroadcaster};
use lege_window::WindowService;
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportMemWl,
            element::{solid::SolidColorRenderElement, surface::WaylandSurfaceRenderElement},
            gles::GlesRenderer,
        },
        session::{Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{
        PopupManager, Space, Window,
        space::{SpaceRenderElements, space_render_elements},
    },
    input::{SeatState, pointer::CursorImageStatus},
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
    utils::{Clock, Transform},
    wayland::{
        compositor::CompositorState,
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
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
use smithay_drm_extras::{display_info, drm_scanner::DrmScanner};
use std::{collections::HashMap, time::Duration};

// The scene this backend hands to `DrmOutput::render_frame`, front-to-back: compositor-drawn
// chrome (recovery band/tab, pressure overlay, scatter backdrop — `crate::chrome`) always wins
// over window content, matching `src/backend/winit.rs`'s `render_output` ordering and
// `src/input.rs`'s hit-testing priority for the same rectangles.
//
// `render_elements!` (Smithay macro, `#[macro_export]`'d — not doc-commentable, hence `//` above
// rather than `///`) generates `UdevRenderElement` and its `RenderElement<GlesRenderer>` impl.
smithay::backend::renderer::element::render_elements! {
    UdevRenderElement<=GlesRenderer>;
    Custom=SolidColorRenderElement,
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
}

/// Clear colour behind every output — matches `src/backend/winit.rs`'s so a window dragged
/// between the two backends during development looks the same.
const CLEAR_COLOR: [f32; 4] = [0.05, 0.05, 0.07, 1.0];

/// Scanout pixel formats to try, most-preferred first — 10-bit before 8-bit, matching anvil.
const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
/// No presentation-feedback tracking: neither backend wires `wp_presentation` up yet (see
/// `src/backend/winit.rs`, which does not either) — `()` is `DrmCompositor`'s per-buffer user data
/// when there is nothing to attach.
type UserData = ();
type LegeDrmOutputManager = DrmOutputManager<Allocator, Exporter, UserData, DrmDeviceFd>;
type LegeDrmOutput = DrmOutput<Allocator, Exporter, UserData, DrmDeviceFd>;

/// One connected output: the DRM connector/CRTC pair's live scanout surface plus the Smithay
/// `Output`/global mirroring it into the protocol surface. Dropping this (connector disconnect,
/// device removal) tears the CRTC's `DrmCompositor` down too — `DrmOutput`'s own `Drop` impl
/// removes it from the device's shared compositor map, so no separate teardown call is needed.
struct SurfaceData {
    output: Output,
    global: Option<GlobalId>,
    drm_output: LegeDrmOutput,
}

/// One physical DRM device: since this backend only ever tracks the primary GPU (module doc),
/// there is exactly one entry here for as long as the compositor is running on ordinary hardware.
struct DeviceData {
    drm_output_manager: LegeDrmOutputManager,
    /// The one GLES renderer for this device — no `GpuManager`/multi-renderer juggling, matching
    /// the single-primary-GPU scope: every surface on this device renders through it.
    renderer: GlesRenderer,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    scanner: DrmScanner,
    registration_token: RegistrationToken,
}

pub struct UdevBackendData {
    session: LibSeatSession,
    devices: HashMap<DrmNode, DeviceData>,
    primary_gpu: DrmNode,
    /// `primary_gpu.node_with_type(NodeType::Primary)`'s `dev_id()`, resolved once at startup —
    /// what every udev `Added`/device-list entry's own dev_id is compared against in
    /// [`is_primary_gpu_dev`] to decide whether a newly-seen DRM device is the one this backend
    /// drives or a GPU it deliberately ignores (module doc).
    primary_card_dev_id: u64,
    /// Created once, the first time a primary-GPU device successfully produces a renderer —
    /// `src/backend/winit.rs`'s doc: "the udev backend ... is where DMA-BUF actually matters".
    dmabuf_global: Option<DmabufGlobal>,
    /// Set for the duration of a VT switch away from this session (`Event::PauseSession`) and
    /// cleared on return (`Event::ActivateSession`) — checked by every render/reschedule path so
    /// a stray timer firing mid-pause is a harmless no-op rather than a failed DRM ioctl retried
    /// in a hot loop.
    paused: bool,
}

impl Backend for UdevBackendData {
    fn seat_name(&self) -> String {
        self.session.seat()
    }
}

type State = LegeCompState<UdevBackendData>;

/// Runs the compositor against real DRM/KMS hardware. Returns an error rather than panicking on
/// any step short of "the event loop itself cannot start" — a broken driver should end the process
/// cleanly (the systemd unit's `Restart=no`, `session/units/user/lege-comp.service`, already
/// treats a compositor exit as session-ending on purpose) rather than take down the session with a
/// backtrace.
// One long, linear "wire everything up" function is the shape every Smithay-based compositor's
// backend entry point takes (`anvil`, `smallvil`) — the alternative is threading a dozen
// half-constructed pieces through several helper functions, which reads worse, not better.
#[allow(clippy::too_many_lines)]
pub fn run() -> anyhow::Result<()> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let (session, notifier) = LibSeatSession::new()
        .map_err(|error| anyhow::anyhow!("opening libseat session: {error}"))?;

    let primary_gpu_path = primary_gpu(session.seat())
        .ok()
        .flatten()
        .and_then(|path| DrmNode::from_path(path).ok())
        .and_then(|node| node.node_with_type(NodeType::Render).and_then(Result::ok))
        .or_else(|| {
            all_gpus(session.seat())
                .ok()?
                .into_iter()
                .find_map(|path| DrmNode::from_path(path).ok())
        })
        .ok_or_else(|| anyhow::anyhow!("no GPU found"))?;
    // Every udev `Added`/device-list entry reports a `NodeType::Primary` ("cardN") dev_id;
    // `primary_gpu_path` above may have been coerced to `NodeType::Render` ("renderDNNN") to
    // resolve DRI device selection, so convert back to compare like with like (falls back to the
    // node's own dev_id for the rare device with no distinct render node at all).
    let primary_card_dev_id = primary_gpu_path
        .node_with_type(NodeType::Primary)
        .and_then(Result::ok)
        .map_or_else(|| primary_gpu_path.dev_id(), |node| node.dev_id());

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
    tracing::info!(socket = %socket_name, "wayland socket ready");

    let mut seat_state: SeatState<State> = SeatState::new();
    let seat_name = session.seat();
    let mut seat = seat_state.new_wl_seat(&dh, seat_name.clone());
    seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 200, 25)
        .expect("failed to add keyboard to seat");
    seat.add_pointer();

    let mut state = State {
        backend_data: UdevBackendData {
            session: session.clone(),
            devices: HashMap::new(),
            primary_gpu: primary_gpu_path,
            primary_card_dev_id,
            dmabuf_global: None,
            paused: false,
        },
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
        seat_name,
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
            state.process_input_event(event, None);
        })
        .map_err(|error| anyhow::anyhow!("inserting libinput source: {error}"))?;

    event_loop
        .handle()
        .insert_source(notifier, move |event, (), state: &mut State| match event {
            smithay::backend::session::Event::PauseSession => {
                tracing::info!("session paused (VT switch away): releasing DRM devices");
                state.backend_data.paused = true;
                libinput_context.suspend();
                for device in state.backend_data.devices.values_mut() {
                    device.drm_output_manager.pause();
                }
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
                crate::backend::recompute_outputs(state);
            }
        })
        .map_err(|error| anyhow::anyhow!("inserting session-event source: {error}"))?;

    let udev_backend = UdevBackend::new(state.backend_data.session.seat())
        .map_err(|error| anyhow::anyhow!("enumerating udev devices: {error}"))?;
    for (device_id, path) in udev_backend.device_list() {
        if let Ok(node) = DrmNode::from_dev_id(device_id) {
            add_device(&mut state, node, path);
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
                        scan_connectors(state, node);
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
            "no primary-GPU DRM device came up at startup — the compositor will hold the seat \
             with no display until one appears (hotplug) or the process is stopped"
        );
    }
    state.start_xwayland(&event_loop.handle());

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

/// Reactivates one already-tracked device after `Event::ActivateSession` and kicks a fresh render
/// of every surface on it: the DRM state (CRTC↔connector bindings in particular) can have changed
/// under a foreign master while this session held no master at all, so every buffer is treated as
/// stale rather than assumed still valid.
fn reactivate_device(state: &mut State, node: DrmNode) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    if let Err(error) = device.drm_output_manager.lock().activate(false) {
        tracing::warn!(%error, ?node, "failed to reactivate drm device after session resume");
        return;
    }
    let crtcs: Vec<crtc::Handle> = device.surfaces.keys().copied().collect();
    for surface in device.surfaces.values() {
        surface.drm_output.reset_buffers();
    }
    for crtc in crtcs {
        state
            .loop_handle
            .insert_idle(move |state| render_surface(state, node, crtc));
    }
}

/// Whether a just-appeared udev DRM device is the one primary GPU this backend drives (module
/// doc: "one primary GPU correctly, not every GPU configuration"). Compares raw `dev_t` values
/// rather than `DrmNode`s so it needs no filesystem access and is unit-testable without a real
/// DRM device.
fn is_primary_gpu_dev(candidate_card_dev_id: u64, primary_card_dev_id: u64) -> bool {
    candidate_card_dev_id == primary_card_dev_id
}

/// `interface-id` naming (e.g. `eDP-1`, `HDMI-A-1`) via `Interface::as_str`, the same convention
/// `wlr-randr`/every other compositor uses — not `Interface`'s `Debug` form, which would print
/// e.g. `EmbeddedDisplayPort-1` for the exact same connector.
fn output_name(interface: connector::Interface, interface_id: u32) -> String {
    format!("{}-{interface_id}", interface.as_str())
}

/// The connector's preferred mode if the EDID marked one, else its first mode — `connector.modes()`
/// is already EDID-preference-ordered, same rule every Smithay-based compositor uses. Generic and
/// predicate-injected so it is unit-testable without a real `drm::control::Mode`.
fn select_preferred_mode<M: Copy>(modes: &[M], is_preferred: impl Fn(&M) -> bool) -> Option<M> {
    modes
        .iter()
        .find(|m| is_preferred(m))
        .or_else(|| modes.first())
        .copied()
}

/// Where a newly-connected output should sit: immediately to the right of every already-mapped
/// output, or at the origin if there are none yet. Pure over the existing outputs' right edges so
/// it is unit-testable without a real `Space`.
fn next_output_x(existing_right_edges: impl Iterator<Item = i32>) -> i32 {
    existing_right_edges.max().unwrap_or(0)
}

// Long and linear for the same reason `run` is (see its `#[allow]`): this is "open one device and
// wire up everything it needs," not several independent concerns artificially split apart.
#[allow(clippy::too_many_lines)]
fn add_device(state: &mut State, node: DrmNode, path: &std::path::Path) {
    if !is_primary_gpu_dev(node.dev_id(), state.backend_data.primary_card_dev_id) {
        tracing::info!(
            ?node,
            ?path,
            "ignoring non-primary GPU device (single-primary-GPU scope, see module doc)"
        );
        return;
    }
    if state.backend_data.devices.contains_key(&node) {
        tracing::debug!(?node, "drm device already tracked, ignoring duplicate add");
        return;
    }

    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let Ok(fd) = state.backend_data.session.open(path, open_flags) else {
        tracing::warn!(?path, "failed to open drm device");
        return;
    };
    let fd = DrmDeviceFd::new(smithay::utils::DeviceFd::from(fd));
    let Ok((drm, drm_notifier)) = DrmDevice::new(fd.clone(), true) else {
        tracing::warn!(?path, "failed to initialize drm device");
        return;
    };
    let Ok(gbm) = GbmDevice::new(fd) else {
        tracing::warn!(?path, "failed to initialize gbm device");
        return;
    };

    // SAFETY: `gbm` is a live GBM device for the DRM device just opened above; `EGLDisplay::new`'s
    // only requirement is that the native display handed to it (the GBM device) stays valid for
    // the `EGLDisplay`'s own lifetime, which holds here since `gbm` is cloned into it and the
    // original is kept alive below for the allocator/exporter.
    #[allow(unsafe_code)]
    let egl_display = match unsafe { EGLDisplay::new(gbm.clone()) } {
        Ok(display) => display,
        Err(error) => {
            tracing::warn!(%error, ?path, "failed to create EGL display for primary GPU");
            return;
        }
    };
    let egl_context = match EGLContext::new(&egl_display) {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(%error, ?path, "failed to create EGL context for primary GPU");
            return;
        }
    };
    // SAFETY: `egl_context` was just created immediately above and is not shared with, or bound
    // to, any other renderer — `GlesRenderer::new`'s contract (this renderer is the context's sole
    // owner for its whole lifetime) holds by construction here.
    #[allow(unsafe_code)]
    let renderer = match unsafe { GlesRenderer::new(egl_context) } {
        Ok(renderer) => renderer,
        Err(error) => {
            tracing::warn!(%error, ?path, "failed to create GLES renderer for primary GPU");
            return;
        }
    };

    state.shm_state.update_formats(renderer.shm_formats());

    // Both of these must key off the *render* node, not `node` (the "cardN" device this function
    // was handed): a client's own dmabufs report their originating device as a render node
    // (`renderDNNN`), so filtering/advertising against the primary/card node here would silently
    // and permanently disable direct scanout — every client buffer would fail `NodeFilter`'s
    // comparison and fall back to a compositor-side copy, a real perf cliff but not a correctness
    // one, which is exactly the kind of regression that is easy to miss without hardware to see it
    // on. `state.backend_data.primary_gpu` was resolved to `NodeType::Render` at the top of `run`.
    let render_node = state.backend_data.primary_gpu;

    if state.backend_data.dmabuf_global.is_none() {
        match DmabufFeedbackBuilder::new(render_node.dev_id(), renderer.dmabuf_formats()).build() {
            Ok(feedback) => {
                let global = state
                    .dmabuf_state
                    .create_global_with_default_feedback::<State>(&state.display_handle, &feedback);
                state.backend_data.dmabuf_global = Some(global);
            }
            Err(error) => {
                tracing::warn!(%error, "failed to build dmabuf feedback for primary GPU — dmabuf clients will fall back to shm");
            }
        }
    }

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into());
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();

    let drm_output_manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        SUPPORTED_COLOR_FORMATS.iter().copied(),
        render_formats,
    );

    let registration_token = match state.loop_handle.insert_source(
        drm_notifier,
        move |event, metadata, state: &mut State| {
            let _ = metadata;
            match event {
                DrmEvent::VBlank(crtc) => on_vblank(state, node, crtc),
                DrmEvent::Error(error) => {
                    tracing::warn!(%error, ?node, "drm device error event");
                }
            }
        },
    ) {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!(%error, ?path, "failed to register drm event source");
            return;
        }
    };

    state.backend_data.devices.insert(
        node,
        DeviceData {
            drm_output_manager,
            renderer,
            surfaces: HashMap::new(),
            scanner: DrmScanner::new(),
            registration_token,
        },
    );
    scan_connectors(state, node);
}

fn scan_connectors(state: &mut State, node: DrmNode) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let Ok(scan_result) = device
        .scanner
        .scan_connectors(device.drm_output_manager.device())
    else {
        return;
    };
    for event in scan_result {
        match event {
            smithay_drm_extras::drm_scanner::DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(state, node, &connector, crtc),
            smithay_drm_extras::drm_scanner::DrmScanEvent::Disconnected {
                connector: _,
                crtc: Some(crtc),
            } => connector_disconnected(state, node, crtc),
            _ => {}
        }
    }
    crate::backend::recompute_outputs(state);
}

// See `add_device`'s identical `#[allow]`: one connector's worth of "compute its mode, register
// its `Output`, hand it a real scanout surface" reads worse split apart than kept linear.
#[allow(clippy::too_many_lines)]
fn connector_connected(
    state: &mut State,
    node: DrmNode,
    connector: &connector::Info,
    crtc: crtc::Handle,
) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let output_name = output_name(connector.interface(), connector.interface_id());
    let info = display_info::for_connector(device.drm_output_manager.device(), connector.handle());
    // `libdisplay_info::info::Info` (the closures' receiver type) isn't nameable here without
    // adding `libdisplay-info` as a direct dependency purely to write `Info::make`/`Info::model`
    // in method-reference form — not worth a new dependency for a clippy style preference.
    #[allow(clippy::redundant_closure_for_method_calls)]
    let make = info
        .as_ref()
        .and_then(|info| info.make())
        .unwrap_or_else(|| "Unknown".to_owned());
    #[allow(clippy::redundant_closure_for_method_calls)]
    let model = info
        .as_ref()
        .and_then(|info| info.model())
        .unwrap_or_else(|| "Unknown".to_owned());

    let Some(drm_mode) = select_preferred_mode(connector.modes(), |m| {
        m.mode_type()
            .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
    }) else {
        tracing::warn!(%output_name, "connector reports no usable mode, leaving it dark");
        return;
    };
    let wl_mode = Mode::from(drm_mode);

    let x = next_output_x(
        state
            .space
            .outputs()
            .filter_map(|o| state.space.output_geometry(o))
            .map(|geo| geo.loc.x + geo.size.w),
    );

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make,
            model,
            serial_number: "1".into(),
        },
    );
    let global = output.create_global::<State>(&state.display_handle);
    output.change_current_state(
        Some(wl_mode),
        Some(Transform::Normal),
        None,
        Some((x, 0).into()),
    );
    output.set_preferred(wl_mode);
    state.space.map_output(&output, (x, 0));

    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        state.space.unmap_output(&output);
        state.display_handle.remove_global::<State>(global);
        return;
    };
    let drm_device = device.drm_output_manager.device();
    let driver = match drm_device.get_driver() {
        Ok(driver) => driver,
        Err(error) => {
            tracing::warn!(%error, %output_name, "failed to query drm driver, leaving connector dark");
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };
    let mut planes = match drm_device.planes(&crtc) {
        Ok(planes) => planes,
        Err(error) => {
            tracing::warn!(%error, %output_name, "failed to query crtc planes, leaving connector dark");
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

    let mut locked = device.drm_output_manager.lock();
    let renderer = &mut device.renderer;
    let drm_output = locked.initialize_output::<GlesRenderer, UdevRenderElement>(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        Some(planes),
        renderer,
        &DrmOutputRenderElements::default(),
    );

    let drm_output = match drm_output {
        Ok(drm_output) => drm_output,
        Err(error) => {
            tracing::warn!(%error, %output_name, "failed to initialize drm output, leaving connector dark");
            state.space.unmap_output(&output);
            state.display_handle.remove_global::<State>(global);
            return;
        }
    };

    device.surfaces.insert(
        crtc,
        SurfaceData {
            output,
            global: Some(global),
            drm_output,
        },
    );
    tracing::info!(%output_name, ?crtc, "output lit");

    state
        .loop_handle
        .insert_idle(move |state| render_surface(state, node, crtc));
}

fn connector_disconnected(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    if let Some(surface) = device.surfaces.remove(&crtc) {
        state.space.unmap_output(&surface.output);
        if let Some(global) = surface.global {
            state.display_handle.remove_global::<State>(global);
        }
        // `surface.drm_output` drops here, which removes this crtc's `DrmCompositor` from the
        // device's shared compositor map (`DrmOutput`'s own `Drop` impl) — no separate teardown
        // call needed.
    }
}

fn remove_device(state: &mut State, node: DrmNode) {
    if let Some(mut device) = state.backend_data.devices.remove(&node) {
        for (_, surface) in device.surfaces.drain() {
            state.space.unmap_output(&surface.output);
            if let Some(global) = surface.global {
                state.display_handle.remove_global::<State>(global);
            }
        }
        state.loop_handle.remove(device.registration_token);
    }
    crate::backend::recompute_outputs(state);
}

/// Marks the just-completed frame submitted (releasing its buffer back to the swapchain) and
/// immediately renders and queues the next one — this is what paces redraws to the display's real
/// refresh rate instead of a fixed-interval timer (module doc).
fn on_vblank(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    if let Some(surface) = state
        .backend_data
        .devices
        .get(&node)
        .and_then(|device| device.surfaces.get(&crtc))
        && let Err(error) = surface.drm_output.frame_submitted()
    {
        tracing::warn!(%error, ?crtc, "failed to mark drm frame submitted");
    }
    render_surface(state, node, crtc);
}

/// Renders one CRTC's surface and, unless a flip was actually queued (in which case `on_vblank`
/// will drive the next frame when it completes), falls back to a timer retry.
fn render_surface(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    if state.backend_data.paused {
        return;
    }
    let Some(output) = state
        .backend_data
        .devices
        .get(&node)
        .and_then(|device| device.surfaces.get(&crtc))
        .map(|surface| surface.output.clone())
    else {
        return;
    };

    let chrome = crate::chrome::collect_elements(state, &output);
    if render_and_queue(state, node, crtc, &output, chrome) {
        schedule_retry(state, node, crtc);
    }
}

/// Builds this frame's elements, renders them through `DrmOutput`'s damage tracker, and queues a
/// page flip if anything actually changed. Returns whether the caller should fall back to a timer
/// retry: true on any failure or a damage-free frame (nothing queued, so no page-flip completion
/// will come to drive the next attempt), false once a flip is queued.
fn render_and_queue(
    state: &mut State,
    node: DrmNode,
    crtc: crtc::Handle,
    output: &Output,
    chrome: Vec<SolidColorRenderElement>,
) -> bool {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return false;
    };
    let renderer = &mut device.renderer;

    let space_elements =
        match space_render_elements::<_, Window, _>(renderer, [&state.space], output, 1.0) {
            Ok(elements) => elements,
            Err(error) => {
                tracing::warn!(%error, ?crtc, "failed to collect space render elements");
                return true;
            }
        };

    let mut elements: Vec<UdevRenderElement> =
        Vec::with_capacity(chrome.len() + space_elements.len());
    elements.extend(chrome.into_iter().map(UdevRenderElement::Custom));
    elements.extend(space_elements.into_iter().map(UdevRenderElement::Space));

    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return false;
    };

    match surface
        .drm_output
        .render_frame(renderer, &elements, CLEAR_COLOR, FrameFlags::DEFAULT)
    {
        Ok(result) if result.is_empty => true,
        Ok(_) => match surface.drm_output.queue_frame(()) {
            Ok(()) => false,
            Err(error) => {
                tracing::warn!(%error, ?crtc, "failed to queue drm frame for scanout");
                true
            }
        },
        Err(error) => {
            tracing::warn!(%error, ?crtc, "drm render_frame failed");
            true
        }
    }
}

/// One nominal frame interval after this call, retries rendering `crtc` — the fallback for the
/// two cases page-flip completion cannot drive: a damage-free frame (nothing queued, so no vblank
/// is coming) and a `render_frame`/`queue_frame` failure. Falls back to a plain 16ms poll if the
/// output's mode isn't known yet.
fn schedule_retry(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    if state.backend_data.paused {
        return;
    }
    let delay = state
        .backend_data
        .devices
        .get(&node)
        .and_then(|device| device.surfaces.get(&crtc))
        .and_then(|surface| surface.output.current_mode())
        .filter(|mode| mode.refresh > 0)
        .map_or(Duration::from_millis(16), |mode| {
            Duration::from_secs_f64(1000.0 / f64::from(mode.refresh))
        });
    if let Err(error) = state.loop_handle.insert_source(
        Timer::from_duration(delay),
        move |_, (), state: &mut State| {
            render_surface(state, node, crtc);
            TimeoutAction::Drop
        },
    ) {
        tracing::warn!(%error, ?crtc, "failed to schedule drm repaint retry");
    }
}

#[cfg(test)]
mod tests {
    use super::{is_primary_gpu_dev, next_output_x, output_name, select_preferred_mode};

    #[test]
    fn select_preferred_mode_picks_the_flagged_entry() {
        assert_eq!(select_preferred_mode(&[1, 2, 3], |&m| m == 2), Some(2));
    }

    #[test]
    fn select_preferred_mode_falls_back_to_first() {
        assert_eq!(select_preferred_mode(&[5, 6, 7], |_| false), Some(5));
    }

    #[test]
    fn select_preferred_mode_empty_list_is_none() {
        assert_eq!(select_preferred_mode::<i32>(&[], |_| true), None);
    }

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
    fn is_primary_gpu_dev_matches_the_primary_card() {
        assert!(is_primary_gpu_dev(7, 7));
    }

    #[test]
    fn is_primary_gpu_dev_rejects_a_secondary_gpu() {
        // e.g. the NVIDIA half of a hybrid Intel+NVIDIA laptop reporting its own "cardN".
        assert!(!is_primary_gpu_dev(9, 7));
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
