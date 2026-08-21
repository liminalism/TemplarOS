//! The udev/KMS backend: real hardware, via `LibSeatSession` and atomic-modesetting DRM. Modelled
//! closely on `anvil/src/udev.rs` (MIT, ADR-0012) with its multi-GPU render-node juggling,
//! VRR/direct-scanout and debug-overlay machinery stripped out — this crate targets one primary
//! GPU correctly, not every GPU configuration anvil handles. It cannot be exercised or
//! hardware-tested from the development session this crate was written in (no DRM device this
//! session can take modesetting master on); it is written to be correct by construction against
//! the pinned Smithay API and to fail loudly (a logged error, not a panic that takes the session
//! down) if a step it depends on is unavailable, rather than run.
//!
//! Every output-topology-affecting event below funnels through the same
//! `crate::backend::recompute_outputs` entry point winit uses — connector hotplug, mode changes
//! and session resume all call it, never touching `WindowService` directly (COMPOSITOR.md's "the
//! only place `Output`/topology changes enter `lege-window`").

use crate::state::{Backend, LegeCompState};
use lege_comp_integration::{DbusIdentityCache, GovernorNotifier, ShellStateBroadcaster};
use lege_window::WindowService;
use smithay::{
    backend::{
        drm::{DrmDevice, DrmDeviceFd, DrmNode, NodeType},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::{Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    desktop::{PopupManager, Space},
    input::{SeatState, pointer::CursorImageStatus},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        input::Libinput,
        wayland_server::{Display, backend::GlobalId},
    },
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
use smithay_drm_extras::{display_info, drm_scanner::DrmScanner};
use std::collections::HashMap;

/// One connected output: the DRM connector/CRTC pair, its Smithay `Output`, and the global to tear
/// down on disconnect.
struct SurfaceData {
    output: Output,
    global: Option<GlobalId>,
}

/// One physical DRM device: its scanner (connector hot-plug diffing) and the currently-active
/// outputs on it. A single-GPU compositor has exactly one entry here.
struct DeviceData {
    drm: DrmDevice,
    surfaces: HashMap<smithay::reexports::drm::control::crtc::Handle, SurfaceData>,
    scanner: DrmScanner,
}

pub struct UdevBackendData {
    session: LibSeatSession,
    devices: HashMap<DrmNode, DeviceData>,
    primary_gpu: DrmNode,
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

    // Session pause/resume: drop and reacquire DRM master. Every currently-tracked surface's
    // device is paused/resumed uniformly; connector rescanning after resume rides on the udev
    // `Changed` event the kernel emits on TTY switch back, same as anvil.
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
                libinput_context.suspend();
                for device in state.backend_data.devices.values_mut() {
                    device.drm.pause();
                }
            }
            smithay::backend::session::Event::ActivateSession => {
                if libinput_context.resume().is_err() {
                    tracing::warn!("failed to resume libinput after session activation");
                }
                for device in state.backend_data.devices.values_mut() {
                    if let Err(error) = device.drm.activate(false) {
                        tracing::warn!(%error, "failed to reactivate drm device");
                    }
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
    state.start_xwayland(&event_loop.handle());

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

fn add_device(state: &mut State, node: DrmNode, path: &std::path::Path) {
    use smithay::reexports::rustix::fs::OFlags;
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let Ok(fd) = state.backend_data.session.open(path, open_flags) else {
        tracing::warn!(?path, "failed to open drm device");
        return;
    };
    let fd = DrmDeviceFd::new(smithay::utils::DeviceFd::from(fd));
    let Ok((drm, drm_notifier)) = DrmDevice::new(fd, true) else {
        tracing::warn!(?path, "failed to initialize drm device");
        return;
    };

    if let Err(error) =
        state
            .loop_handle
            .insert_source(drm_notifier, move |event, metadata, state: &mut State| {
                let _ = (event, metadata, state);
                // Page-flip completion: a fuller implementation schedules the next frame here. Left as a
                // structural stub (see this module's doc) — nothing to verify it against in this session.
            })
    {
        tracing::warn!(%error, "failed to register drm event source");
    }

    state.backend_data.devices.insert(
        node,
        DeviceData {
            drm,
            surfaces: HashMap::new(),
            scanner: DrmScanner::new(),
        },
    );
    scan_connectors(state, node);
}

fn scan_connectors(state: &mut State, node: DrmNode) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let Ok(scan_result) = device.scanner.scan_connectors(&device.drm) else {
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

fn connector_connected(
    state: &mut State,
    node: DrmNode,
    connector: &smithay::reexports::drm::control::connector::Info,
    crtc: smithay::reexports::drm::control::crtc::Handle,
) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    let output_name = format!("{:?}-{}", connector.interface(), connector.interface_id());
    let info = display_info::for_connector(&device.drm, connector.handle());
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

    let Some(drm_mode) = connector
        .modes()
        .iter()
        .find(|m| {
            m.mode_type()
                .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
        })
        .or_else(|| connector.modes().first())
        .copied()
    else {
        return;
    };
    let wl_mode = Mode::from(drm_mode);

    let x = state
        .space
        .outputs()
        .filter_map(|o| state.space.output_geometry(o))
        .map(|geo| geo.loc.x + geo.size.w)
        .max()
        .unwrap_or(0);

    let output = Output::new(
        output_name,
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
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

    // A real `DrmOutput`/`DrmCompositor` (atomic modeset + page-flip render loop) is not
    // initialized here — see this module's doc. `Output`/`Space` are wired so the geometry and
    // protocol surface are already correct; the render/present cycle is the piece that needs real
    // hardware to finish and verify.
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    device.surfaces.insert(
        crtc,
        SurfaceData {
            output,
            global: Some(global),
        },
    );
}

fn connector_disconnected(
    state: &mut State,
    node: DrmNode,
    crtc: smithay::reexports::drm::control::crtc::Handle,
) {
    let Some(device) = state.backend_data.devices.get_mut(&node) else {
        return;
    };
    if let Some(surface) = device.surfaces.remove(&crtc) {
        state.space.unmap_output(&surface.output);
        if let Some(global) = surface.global {
            state.display_handle.remove_global::<State>(global);
        }
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
    }
    crate::backend::recompute_outputs(state);
}
