//! Secondary-GPU driver isolation and asynchronous cross-device display transactions.
//! The broker owns protocol Outputs, not secondary DRM/EGL objects. No worker is joined,
//! no request waits for a driver, and a stalled worker retains its old physical ownership.
use super::*;
use smithay::backend::{
    allocator::{
        Allocator as _, Format, Modifier,
        dmabuf::{AsDmabuf, Dmabuf},
        format::FormatSet,
    },
    renderer::{Bind, damage::OutputDamageTracker},
};
use smithay::reexports::calloop::channel;
use std::{
    collections::{BTreeSet, HashMap},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use templar_types::{
    compositor_control::{ControlError, ControlReply, ControlRequest},
    display::{
        Display, DisplayError, DisplayId, DisplayProfile, OutputConfiguration, PhysicalState,
    },
};
#[path = "native_device_worker.rs"]
mod device_worker;

pub(super) struct FramePacket {
    pub epoch: native_policy::Epoch,
    pub serial: u64,
    pub configuration: OutputConfiguration,
    pub buffer: Dmabuf,
    pub fence: Option<OwnedFd>,
    pub requires_fence: bool,
    pub metadata: FrameUserData,
}
pub(super) enum Command {
    Scan,
    Test {
        serial: u64,
        profile: DisplayProfile,
    },
    Apply {
        profile: DisplayProfile,
        transaction: Option<u64>,
        finish_break: bool,
    },
    Frame {
        crtc: crtc::Handle,
        packet: FramePacket,
    },
    Blank(bool),
    Pause(native_policy::Epoch),
    Resume(native_policy::Epoch),
    Shutdown,
}
pub(super) struct OutputSnapshot {
    pub crtc: crtc::Handle,
    pub configuration: OutputConfiguration,
    pub mode: Mode,
    pub physical_size: smithay::utils::Size<i32, smithay::utils::Raw>,
}
pub(super) struct Snapshot {
    pub catalog: Vec<Display>,
    pub outputs: Vec<OutputSnapshot>,
    pub formats: FormatSet,
    pub active: bool,
    pub failures: Vec<templar_types::display::DisplayFailure>,
    pub topology: bool,
    pub blanked: bool,
}
pub(super) enum Event {
    Observed {
        node: DrmNode,
        epoch: native_policy::Epoch,
        observation: native_outputs::FailedDevice,
    },
    Admitted {
        node: DrmNode,
        epoch: native_policy::Epoch,
        result: Result<(), String>,
    },
    Rearmed {
        node: DrmNode,
        epoch: native_policy::Epoch,
        result: Result<(), String>,
    },
    Snapshot {
        node: DrmNode,
        epoch: native_policy::Epoch,
        snapshot: Snapshot,
    },
    Tested {
        node: DrmNode,
        epoch: native_policy::Epoch,
        serial: u64,
        result: Result<(), String>,
    },
    Presented {
        node: DrmNode,
        epoch: native_policy::Epoch,
        crtc: crtc::Handle,
        serial: u64,
        metadata: FrameUserData,
    },
    Retired {
        node: DrmNode,
        epoch: native_policy::Epoch,
        crtc: crtc::Handle,
        serial: u64,
    },
    BreakFinished {
        node: DrmNode,
        epoch: native_policy::Epoch,
        transaction: Option<u64>,
    },
    Failed {
        node: DrmNode,
        epoch: native_policy::Epoch,
        reason: String,
    },
    Exited {
        node: DrmNode,
        epoch: native_policy::Epoch,
    },
}
struct RemoteOutput {
    output: Output,
    global: GlobalId,
    configuration: OutputConfiguration,
    tracker: OutputDamageTracker,
    buffer: Option<Dmabuf>,
    format: Option<Format>,
    busy: Option<(u64, Instant)>,
    serial: u64,
    presented: bool,
    retry: Option<RegistrationToken>,
}
struct RemoteDevice {
    path: PathBuf,
    gpu: String,
    epoch: native_policy::Epoch,
    owner_epoch: native_policy::Epoch,
    commands: Option<channel::SyncSender<Command>>,
    removed: bool,
    catalog: Vec<Display>,
    outputs: HashMap<crtc::Handle, RemoteOutput>,
    formats: FormatSet,
    active: bool,
    failure: Option<String>,
    failures: Vec<templar_types::display::DisplayFailure>,
    known: bool,
    admitting: bool,
    first_presented: bool,
    started: Instant,
    stalled: bool,
    blanked: bool,
    admission_cancel: Arc<AtomicBool>,
    restart: bool,
}
struct PendingCheck {
    serial: u64,
    profile: DisplayProfile,
    request: ControlRequest,
    reply: tokio::sync::oneshot::Sender<ControlReply>,
    awaiting: BTreeSet<u64>,
    started: Instant,
}
struct Restore {
    serial: u64,
    token: Option<uuid::Uuid>,
    profile: DisplayProfile,
    completion: Option<tokio::sync::oneshot::Sender<ControlReply>>,
    awaiting: BTreeSet<u64>,
    applying: bool,
    started: Instant,
}
struct Cutover {
    serial: u64,
    profile: DisplayProfile,
    awaiting: BTreeSet<DisplayId>,
    held: Vec<(DisplayId, Option<uuid::Uuid>)>,
    primary_remove: Vec<(DrmNode, crtc::Handle)>,
    breaking: BTreeSet<u64>,
    started: Instant,
}
impl Cutover {
    fn presented(
        &mut self,
        id: &DisplayId,
        transaction: Option<u64>,
        preview: Option<uuid::Uuid>,
    ) -> bool {
        if transaction != Some(self.serial) || !self.awaiting.remove(id) {
            return false;
        }
        self.held.push((id.clone(), preview));
        self.awaiting.is_empty() && self.breaking.is_empty()
    }
}
pub(super) struct Broker {
    devices: HashMap<DrmNode, RemoteDevice>,
    events: channel::Sender<Event>,
    runtime: tokio::runtime::Handle,
    recovery: Option<native_recovery::Lease>,
    notice: String,
    serial: u64,
    pending_check: Option<PendingCheck>,
    checked: Option<(DisplayProfile, Vec<(u64, native_policy::Epoch)>)>,
    cutover: Option<Cutover>,
    completed: Option<DisplayProfile>,
    restore: Option<Restore>,
}
impl Broker {
    pub fn new(
        runtime: tokio::runtime::Handle,
        recovery: anyhow::Result<native_recovery::Lease>,
    ) -> (Self, channel::Channel<Event>) {
        let (events, receiver) = channel::channel();
        let (recovery, notice) = match recovery {
            Ok(lease) => {
                let notice = if lease.notice.is_empty() {
                    String::new()
                } else {
                    tracing::warn!(explanation = %lease.notice, "secondary GPU crash breaker active");
                    "Display recovery: primary GPU only. Secondary GPU did not present; external GPU displays unavailable until reboot.".into()
                };
                (Some(lease), notice)
            }
            Err(error) => {
                tracing::warn!(%error, "trusted display crash-breaker authority unavailable");
                (None, "Display safety: external GPU outputs unavailable because the trusted crash-breaker authority could not be reached.".into())
            }
        };
        (
            Self {
                devices: HashMap::new(),
                events,
                runtime,
                recovery,
                notice,
                serial: 0,
                pending_check: None,
                checked: None,
                cutover: None,
                completed: None,
                restore: None,
            },
            receiver,
        )
    }
    pub fn notice(&self) -> &str {
        &self.notice
    }
    pub fn restoring(&self) -> bool {
        self.restore.is_some()
    }
    pub fn active(&self) -> bool {
        self.devices
            .values()
            .any(|device| !device.removed && device.active)
    }
    pub fn inventory(&self) -> impl Iterator<Item = Display> + '_ {
        self.devices
            .values()
            .filter(|device| !device.removed)
            .flat_map(|device| device.catalog.iter().cloned())
    }
    /// Protocol outputs remain mapped until `unmap`; no secondary renderer is involved
    /// in logical-primary selection. Owner configurations are intentionally left untouched.
    pub fn logical_outputs(&self) -> impl Iterator<Item = (&Display, bool)> + '_ {
        self.devices
            .values()
            .filter(|device| !device.removed)
            .flat_map(|device| {
                device.catalog.iter().map(|display| {
                    let mapped = device.outputs.values().any(|output| {
                        output.configuration.id == display.id
                            && output.configuration.enabled
                            && output.output.current_mode().is_some()
                    });
                    (display, mapped)
                })
            })
    }
    pub fn failures(&self) -> Vec<templar_types::display::DisplayFailure> {
        self.devices
            .values()
            .filter(|device| !device.removed)
            .flat_map(|device| {
                let mut failures = device.failures.clone();
                if let Some(reason) = &device.failure {
                    failures.push(templar_types::display::DisplayFailure {
                        gpu: device.catalog.first().map_or_else(
                            || device.path.display().to_string(),
                            |display| display.gpu.clone(),
                        ),
                        connector: None,
                        reason: reason.clone(),
                    });
                }
                failures
            })
            .collect()
    }
    pub fn contains(&self, id: &DisplayId) -> bool {
        self.devices
            .values()
            .any(|device| !device.removed && device.catalog.iter().any(|display| &display.id == id))
    }
    pub fn unknown_physical_outputs(&self) -> bool {
        self.devices
            .values()
            .filter(|device| !device.removed)
            .any(|device| {
                !device.known
                    || device.catalog.iter().any(|display| {
                        let actual = match &display.physical {
                            PhysicalState::NativeDisabled => Some(false),
                            PhysicalState::NativeEnabled { .. } => Some(true),
                            _ => None,
                        };
                        let presented = device.outputs.values().any(|output| {
                            output.configuration == display.current && output.presented
                        });
                        display.connected
                            && (!device.active
                                || device.failure.is_some()
                                || !native_policy::physical_proof(
                                    display.current.enabled && !device.blanked,
                                    actual,
                                    presented,
                                ))
                    })
            })
    }
    pub fn blanked(&self) -> bool {
        self.devices
            .values()
            .filter(|device| !device.removed)
            .flat_map(|device| &device.catalog)
            .filter(|display| display.connected)
            .all(|display| matches!(display.physical, PhysicalState::NativeDisabled))
    }
    fn epochs(&self) -> Vec<(u64, native_policy::Epoch)> {
        let mut epochs: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, device)| !device.removed && device.commands.is_some())
            .map(|(node, device)| (node.dev_id(), device.epoch))
            .collect();
        epochs.sort_by_key(|entry| entry.0);
        epochs
    }
    pub fn checked(&self, profile: &DisplayProfile) -> bool {
        self.devices
            .values()
            .filter(|device| !device.removed)
            .all(|device| device.commands.is_none())
            || self
                .checked
                .as_ref()
                .is_some_and(|(checked, epochs)| checked == profile && *epochs == self.epochs())
    }
    pub fn verify(&self, profile: &DisplayProfile) -> Result<(), DisplayError> {
        if self.cutover.is_some() {
            return Err(DisplayError::new(
                "worker_pending",
                "Secondary GPU transaction is awaiting actual presentation and make-before-break readback",
            ));
        }
        for configuration in &profile.outputs {
            let Some(device) = self.devices.values().find(|device| {
                !device.removed
                    && device
                        .catalog
                        .iter()
                        .any(|display| display.id == configuration.id)
            }) else {
                continue;
            };
            let display = device
                .catalog
                .iter()
                .find(|display| display.id == configuration.id)
                .expect("located worker display");
            if !device.active || device.failure.is_some() || &display.current != configuration {
                return Err(DisplayError::new(
                    "worker_readback",
                    "Secondary GPU current state is not the requested live configuration",
                ));
            }
            if configuration.enabled {
                if !matches!(display.physical, PhysicalState::NativeEnabled { .. })
                    || !device
                        .outputs
                        .values()
                        .any(|output| output.configuration == *configuration && output.presented)
                {
                    return Err(DisplayError::new(
                        "worker_readback",
                        "Secondary GPU has not presented and read back its requested output",
                    ));
                }
                // The worker validates structural native timing on first presentation.
            } else if !matches!(display.physical, PhysicalState::NativeDisabled) {
                return Err(DisplayError::new(
                    "worker_readback",
                    "Secondary GPU disabled output has no actual physical blanking proof",
                ));
            }
        }
        Ok(())
    }
    pub fn transaction_for(&self, configuration: &OutputConfiguration) -> Option<u64> {
        self.cutover
            .as_ref()
            .filter(|cutover| {
                cutover
                    .profile
                    .outputs
                    .iter()
                    .any(|target| target == configuration)
            })
            .map(|cutover| cutover.serial)
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        for device in self.devices.values() {
            device.admission_cancel.store(true, Ordering::Release);
            if let Some(commands) = &device.commands {
                let _ = commands.try_send(Command::Shutdown);
            }
        }
    }
}
fn error(code: &str, message: impl Into<String>) -> ControlError {
    ControlError {
        code: code.into(),
        message: message.into(),
    }
}
pub(super) fn install(state: &mut State, receiver: channel::Channel<Event>) -> anyhow::Result<()> {
    state
        .loop_handle
        .insert_source(receiver, |event, (), state: &mut State| {
            if let channel::Event::Msg(event) = event {
                receive(state, event);
            }
        })
        .map_err(|error| anyhow::anyhow!("inserting secondary GPU results: {error}"))?;
    Ok(())
}
fn unobserved(path: &Path) -> RemoteDevice {
    RemoteDevice {
        path: path.into(),
        gpu: path.display().to_string(),
        epoch: native_policy::Epoch(0),
        owner_epoch: native_policy::Epoch(0),
        commands: None,
        removed: false,
        catalog: Vec::new(),
        outputs: HashMap::new(),
        formats: FormatSet::default(),
        active: false,
        failure: None,
        failures: Vec::new(),
        known: false,
        stalled: false,
        blanked: false,
        admitting: false,
        first_presented: false,
        started: Instant::now(),
        admission_cancel: Arc::new(AtomicBool::new(false)),
        restart: false,
    }
}
pub(super) fn add(state: &mut State, node: DrmNode, path: &Path) {
    if let Some(device) = state
        .backend_data
        .secondary
        .devices
        .get_mut(&node)
        .filter(|device| device.removed)
    {
        device.path = path.into();
        device.restart = true;
        return; // Replug waits for the old owner to finish real cleanup.
    }
    if state
        .backend_data
        .secondary
        .devices
        .get(&node)
        .is_some_and(|device| device.commands.is_some() || device.admitting || device.removed)
    {
        return;
    }
    // Even sysfs connector status/EDID reads may enter a driver lock. They are
    // isolated too, with one occupied admission per node until the read returns.
    if state
        .backend_data
        .secondary
        .devices
        .values()
        .filter(|device| device.admitting || device.commands.is_some())
        .count()
        >= 8
    {
        let device = state
            .backend_data
            .secondary
            .devices
            .entry(node)
            .or_insert_with(|| unobserved(path));
        device.failure = Some("Bounded secondary device-owner admission is occupied".into());
        crate::backend::recompute_outputs(state);
        return;
    }
    state.backend_data.secondary.serial = state.backend_data.secondary.serial.wrapping_add(1);
    let epoch = native_policy::Epoch(state.backend_data.secondary.serial);
    let device = state
        .backend_data
        .secondary
        .devices
        .entry(node)
        .or_insert_with(|| unobserved(path));
    device.epoch = epoch;
    device.owner_epoch = epoch;
    device.admitting = true;
    device.stalled = false;
    device.started = Instant::now();
    let path = path.to_owned();
    let events = state.backend_data.secondary.events.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(format!("connectors-{}", node.dev_id()))
        .spawn(move || {
            let observation = native_outputs::FailedDevice::observe(
                node,
                &path,
                "Secondary GPU is dormant until connector presence is established".into(),
            );
            let _ = events.send(Event::Observed {
                node,
                epoch,
                observation,
            });
        })
    {
        let device = state
            .backend_data
            .secondary
            .devices
            .get_mut(&node)
            .expect("owned metadata admission");
        device.admitting = false;
        device.failure = Some(format!(
            "Secondary connector owner could not start: {error}"
        ));
    }
    crate::backend::recompute_outputs(state);
}
struct DeviceExit {
    node: DrmNode,
    epoch: native_policy::Epoch,
    events: channel::Sender<Event>,
    runtime: tokio::runtime::Handle,
    cleanup: Option<(native_recovery::Lease, String)>,
}
impl Drop for DeviceExit {
    fn drop(&mut self) {
        if let Some((lease, gpu)) = self.cleanup.take() {
            self.runtime.spawn(async move {
                match tokio::time::timeout(Duration::from_secs(3), lease.close(&gpu)).await {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => tracing::warn!(%error, "secondary root lease release refused"),
                    Err(error) => tracing::warn!(%error, "secondary release acknowledgment timed out; kernel cleanup is not canceled"),
                }
            });
        }
        let _ = self.events.send(Event::Exited {
            node: self.node,
            epoch: self.epoch,
        });
    }
}
fn start_device(
    state: &mut State,
    node: DrmNode,
    epoch: native_policy::Epoch,
    lease: native_recovery::Lease,
) -> Result<(), String> {
    let device = state
        .backend_data
        .secondary
        .devices
        .get_mut(&node)
        .ok_or("Secondary metadata owner disappeared")?;
    let gpu = device.gpu.clone();
    let cancel = device.admission_cancel.clone();
    cancel.store(false, Ordering::Release);
    let (commands, receiver) = channel::sync_channel(32);
    let (handoff, descriptor) = std::sync::mpsc::sync_channel::<Result<OwnedFd, String>>(1);
    let events = state.backend_data.secondary.events.clone();
    let cleanup_events = events.clone();
    let cleanup_lease = lease.clone();
    let cleanup_gpu = gpu.clone();
    let runtime = state.backend_data.secondary.runtime.clone();
    let cleanup_runtime = runtime.clone();
    // Start the fd's lifetime owner BEFORE requesting any descriptor. Thread
    // admission failure can therefore never last-close a DRM fd on input dispatch.
    std::thread::Builder::new()
        .name(format!("kms-{}", node.dev_id()))
        .spawn(move || {
            // Also runs after Rust unwind, after run's DRM/EGL locals have been dropped.
            let exit = DeviceExit {
                node,
                epoch,
                events: cleanup_events,
                runtime: cleanup_runtime,
                cleanup: Some((cleanup_lease, cleanup_gpu)),
            };
            let result = match descriptor.recv() {
                Ok(Ok(fd)) if !cancel.load(Ordering::Acquire) => device_worker::run(
                    node,
                    epoch,
                    fd,
                    receiver,
                    exit.events.clone(),
                    cancel.clone(),
                ),
                Ok(Ok(fd)) => {
                    drop(fd);
                    Ok(())
                }
                Ok(Err(reason)) => Err(reason),
                Err(_) => Err("Secondary descriptor admission ended before handoff".into()),
            };
            if let Err(reason) = result {
                let _ = exit.events.send(Event::Failed {
                    node,
                    epoch,
                    reason,
                });
            }
        })
        .map_err(|error| format!("Secondary driver owner could not start: {error}"))?;
    device.commands = Some(commands);
    device.admitting = true;
    device.started = Instant::now();
    runtime.spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(7), async {
            lease.begin(&gpu).await?;
            lease.acquire(&gpu).await
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()));
        if result.is_err() {
            let _ = tokio::time::timeout(Duration::from_secs(3), lease.close(&gpu)).await;
        }
        let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
        // This receiver remains owned until this one bounded result is delivered,
        // including cancellation/unplug. No leased fd crosses the input queue.
        let _ = handoff.try_send(result);
        let _ = events.send(Event::Admitted {
            node,
            epoch,
            result: outcome,
        });
    });
    Ok(())
}
pub(super) fn changed(state: &mut State, node: DrmNode) {
    if state.backend_data.paused {
        return;
    }
    if state
        .backend_data
        .secondary
        .devices
        .get(&node)
        .is_some_and(|device| device.admitting || device.stalled || device.removed)
    {
        return;
    }
    if let Some(device) = state
        .backend_data
        .secondary
        .devices
        .get_mut(&node)
        .filter(|device| device.commands.is_some())
    {
        if !device.known {
            return;
        } // Coalesce while this one real scan is unresolved.
        device.known = false;
        device.started = Instant::now();
        if device
            .commands
            .as_ref()
            .is_some_and(|commands| commands.try_send(Command::Scan).is_err())
        {
            device.stalled = true;
            device.admission_cancel.store(true, Ordering::Release);
            device.failure =
                Some("Secondary connector scan admission is occupied or closed".into());
        }
    } else if let Some(path) = state
        .backend_data
        .secondary
        .devices
        .get(&node)
        .map(|device| device.path.clone())
    {
        add(state, node, &path);
    }
}
pub(super) fn remove(state: &mut State, node: DrmNode) {
    if let Some(device) = state.backend_data.secondary.devices.get_mut(&node) {
        device.removed = true;
        device.restart = false;
        device.admission_cancel.store(true, Ordering::Release);
        device.active = false;
        device.known = false;
        if let Some(commands) = &device.commands {
            let _ = commands.try_send(Command::Shutdown);
        }
        let outputs = std::mem::take(&mut device.outputs);
        for output in outputs.into_values() {
            unmap(state, output);
        }
    }
    if state
        .backend_data
        .secondary
        .devices
        .get(&node)
        .is_some_and(|device| device.commands.is_none() && !device.admitting)
    {
        state.backend_data.secondary.devices.remove(&node);
    }
    state.backend_data.secondary.checked = None;
    fail_cutover(state, "Secondary GPU removed during transaction".into());
    crate::backend::recompute_outputs(state);
    crate::compositor_control::maybe_emergency_fallback(state);
}
fn unmap(state: &mut State, output: RemoteOutput) {
    if let Some(retry) = output.retry {
        state.loop_handle.remove(retry);
    }
    output.output.leave_all();
    state.space.unmap_output(&output.output);
    state.chrome.forget_output(&output.output);
    state.display_handle.remove_global::<State>(output.global);
}
fn receive(state: &mut State, event: Event) {
    match event {
        Event::Observed {
            node,
            epoch,
            observation,
        } => {
            let Some(device) = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .filter(|device| device.epoch == epoch)
            else {
                return;
            };
            device.admitting = false;
            if device.removed {
                let restart = device.restart.then(|| device.path.clone());
                state.backend_data.secondary.devices.remove(&node);
                if let Some(path) = restart {
                    add(state, node, &path);
                }
                return;
            }
            device.gpu = observation.gpu;
            device.catalog = observation.catalog;
            device.known = observation.connectors_known;
            let connected = device.catalog.iter().any(|display| display.connected);
            if !device.stalled {
                device.failure = (!device.known).then_some(observation.reason);
            }
            if connected && !device.stalled && !state.backend_data.paused && !state.idle.blanked() {
                if let Some(lease) = state
                    .backend_data
                    .secondary
                    .recovery
                    .clone()
                    .filter(|lease| lease.notice.is_empty())
                {
                    if let Err(reason) = start_device(state, node, epoch, lease) {
                        if let Some(device) = state.backend_data.secondary.devices.get_mut(&node) {
                            device.failure = Some(reason);
                        }
                    }
                } else if let Some(device) = state.backend_data.secondary.devices.get_mut(&node) {
                    device.failure = Some(state.backend_data.secondary.notice.clone());
                }
            } else if !connected
                && !device.stalled
                && !state.backend_data.late_edid.contains_key(&node)
            {
                let path = device.path.clone();
                if let Ok(token) =
                    state.loop_handle.insert_source(
                        Timer::from_duration(native_policy::LATE_EDID_REPROBE),
                        move |_, (), state: &mut State| {
                            if state.backend_data.secondary.devices.get(&node).is_some_and(
                                |device| {
                                    !device.removed
                                        && device.commands.is_none()
                                        && !device.admitting
                                },
                            ) {
                                add(state, node, &path);
                            }
                            TimeoutAction::Drop
                        },
                    )
                {
                    state.backend_data.late_edid.insert(node, token);
                }
            }
            crate::backend::recompute_outputs(state);
        }
        Event::Admitted {
            node,
            epoch,
            result,
        } => {
            if let Some(device) = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .filter(|device| device.epoch == epoch)
            {
                device.admitting = false;
                device.started = Instant::now();
                if let Err(reason) = result {
                    device.failure = Some(format!(
                        "Authenticated secondary device admission failed: {reason}"
                    ));
                }
            }
            crate::backend::recompute_outputs(state);
        }
        Event::Rearmed {
            node,
            epoch,
            result,
        } => {
            let Some(device) = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .filter(|device| device.epoch == epoch && !device.removed)
            else {
                return;
            };
            device.admitting = false;
            if device.admission_cancel.load(Ordering::Acquire) || device.stalled {
                if let Some(commands) = &device.commands {
                    let _ = commands.try_send(Command::Shutdown);
                }
                return;
            }
            if state.backend_data.paused {
                return;
            }
            match result {
                Ok(()) => {
                    device.first_presented = false;
                    device.failure = None;
                    device.stalled = false;
                    if let Some(commands) = &device.commands {
                        if commands.try_send(Command::Resume(epoch)).is_err() {
                            device.stalled = true;
                            device.admission_cancel.store(true, Ordering::Release);
                            device.failure =
                                Some("Secondary resume command admission is occupied".into());
                        }
                    }
                }
                Err(reason) => {
                    device.failure =
                        Some(format!("Authenticated secondary rearm failed: {reason}"));
                    if let Some(commands) = &device.commands {
                        let _ = commands.try_send(Command::Shutdown);
                    }
                }
            }
            crate::backend::recompute_outputs(state);
        }
        Event::Snapshot {
            node,
            epoch,
            snapshot,
        } => {
            if state.backend_data.paused {
                return;
            }
            let Some(device) =
                state
                    .backend_data
                    .secondary
                    .devices
                    .get_mut(&node)
                    .filter(|device| {
                        device.epoch == epoch
                            && !device.removed
                            && !device.stalled
                            && !device.admission_cancel.load(Ordering::Acquire)
                    })
            else {
                return;
            };
            device.catalog = snapshot.catalog;
            device.formats = snapshot.formats;
            device.active = snapshot.active;
            device.blanked = snapshot.blanked;
            device.failures = snapshot.failures;
            device.known = true;
            // A topology observation alone does not erase a render/import failure.
            let mut old = std::mem::take(&mut device.outputs);
            let mut outputs = HashMap::new();
            let mut removed = Vec::new();
            for definition in snapshot.outputs {
                let existing = match old.remove(&definition.crtc) {
                    Some(output) if output.configuration.id == definition.configuration.id => {
                        Some(output)
                    }
                    Some(output) => {
                        removed.push(output);
                        None
                    }
                    None => None,
                };
                let mut output = if let Some(output) = existing {
                    output
                } else {
                    let display = device
                        .catalog
                        .iter()
                        .find(|display| display.id == definition.configuration.id)
                        .expect("worker output belongs to its catalog");
                    let output = Output::new(
                        display.id.0.clone(),
                        PhysicalProperties {
                            size: definition.physical_size,
                            subpixel: smithay::output::Subpixel::Unknown,
                            make: display.make.clone(),
                            model: display.model.clone(),
                            serial_number: display.serial.clone().unwrap_or_default(),
                        },
                    );
                    let global = output.create_global::<State>(&state.display_handle);
                    RemoteOutput {
                        tracker: OutputDamageTracker::from_output(&output),
                        output,
                        global,
                        configuration: definition.configuration.clone(),
                        buffer: None,
                        format: None,
                        busy: None,
                        serial: 0,
                        presented: false,
                        retry: None,
                    }
                };
                if output.configuration != definition.configuration {
                    output.buffer = None;
                    output.format = None;
                    output.presented = false;
                }
                if !snapshot.active {
                    output.presented = false;
                }
                output.configuration = definition.configuration;
                output.output.change_current_state(
                    Some(definition.mode),
                    Some(native_outputs::transform(output.configuration.rotation)),
                    Some(smithay::output::Scale::Fractional(
                        output.configuration.scale,
                    )),
                    Some((output.configuration.x, output.configuration.y).into()),
                );
                crate::viewport::configure(&output.output, output.configuration.insets, false);
                state.space.map_output(
                    &output.output,
                    (output.configuration.x, output.configuration.y),
                );
                outputs.insert(definition.crtc, output);
            }
            removed.extend(old.into_values());
            device.outputs = outputs;
            let needs_auto = snapshot.topology
                && device.commands.is_some()
                && device.catalog.iter().any(|display| {
                    display.connected
                        && !display.modes.is_empty()
                        && !device
                            .outputs
                            .values()
                            .any(|output| output.configuration.id == display.id)
                });
            let topology = snapshot.topology;
            for output in removed {
                unmap(state, output);
            }
            if needs_auto {
                auto_configure(state, node);
            }
            if topology {
                state.backend_data.secondary.checked = None;
            }
            crate::backend::recompute_outputs_with_event(state, topology);
            if topology {
                crate::compositor_control::maybe_emergency_fallback(state);
            }
            request_redraw_node(state, node);
        }
        Event::Tested {
            node,
            epoch,
            serial,
            result,
        } => {
            if !state
                .backend_data
                .secondary
                .devices
                .get(&node)
                .is_some_and(|device| device.epoch == epoch)
            {
                return;
            }
            if state
                .backend_data
                .secondary
                .restore
                .as_ref()
                .is_some_and(|restore| restore.serial == serial)
            {
                if let Err(reason) = result {
                    finish_restore(state, Err(DisplayError::new("worker_test_failed", reason)));
                    return;
                }
                let restore = state
                    .backend_data
                    .secondary
                    .restore
                    .as_mut()
                    .expect("owned restore check");
                restore.awaiting.remove(&node.dev_id());
                if restore.awaiting.is_empty() {
                    apply_restore(state);
                }
                return;
            }
            let Some(check) = state
                .backend_data
                .secondary
                .pending_check
                .as_mut()
                .filter(|check| check.serial == serial)
            else {
                return;
            };
            if let Err(reason) = result {
                let check = state
                    .backend_data
                    .secondary
                    .pending_check
                    .take()
                    .expect("owned check");
                let _ = check.reply.send(ControlReply {
                    result: Err(error("worker_test_failed", reason)),
                });
                return;
            }
            check.awaiting.remove(&node.dev_id());
            if check.awaiting.is_empty() {
                let check = state
                    .backend_data
                    .secondary
                    .pending_check
                    .take()
                    .expect("owned check");
                let epochs = state.backend_data.secondary.epochs();
                state.backend_data.secondary.checked = Some((check.profile, epochs));
                if !check.reply.is_closed() {
                    crate::compositor_control::resume_deferred(state, check.request, check.reply);
                }
            }
        }
        Event::Retired {
            node,
            epoch,
            crtc,
            serial,
        } => {
            let Some(device) = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .filter(|device| device.epoch == epoch && !device.removed)
            else {
                return;
            };
            let Some(output) = device
                .outputs
                .get_mut(&crtc)
                .filter(|output| output.busy.is_some_and(|(queued, _)| queued == serial))
            else {
                return;
            };
            output.busy = None;
            output.presented = false;
            // Only real worker clear/retirement releases a submitted transport
            // buffer. Epoch changes or elapsed time never synthesize this event.
            state
                .loop_handle
                .insert_idle(move |state| render(state, node, crtc));
        }
        Event::Presented {
            node,
            epoch,
            crtc,
            serial,
            metadata,
        } => {
            let Some(device) =
                state
                    .backend_data
                    .secondary
                    .devices
                    .get_mut(&node)
                    .filter(|device| {
                        !device.stalled
                            && !device.admission_cancel.load(Ordering::Acquire)
                            && native_policy::accepts(device.epoch, epoch, device.active)
                    })
            else {
                return;
            };
            let Some(output) = device
                .outputs
                .get_mut(&crtc)
                .filter(|output| output.busy.is_some_and(|(queued, _)| queued == serial))
            else {
                return;
            };
            output.busy = None;
            output.presented = true;
            device.failure = None;
            let first = !device.first_presented;
            device.first_presented = true;
            let output = output.output.clone();
            let id = DisplayId(output.name());
            let gpu = device.catalog.first().map(|display| display.gpu.clone());
            if first
                && let Some((lease, gpu)) = state.backend_data.secondary.recovery.clone().zip(gpu)
            {
                state.backend_data.secondary.runtime.spawn(async move { if let Err(error) = lease.presented(&gpu).await { tracing::warn!(%error, "first secondary frame presented but crash marker could not be cleared"); } });
            }
            state
                .session_lock
                .frame_presented(&output, metadata.lock_generation);
            if !defer_presentation(state, &id, metadata.transaction, metadata.preview) {
                state
                    .display_control
                    .frame_presented(&id, metadata.preview, Instant::now());
            }
            if let Some((name, revision)) = metadata.wallpaper {
                state.wallpaper.frame_presented(&name, Some(revision));
            }
            crate::notify_ready_once(&mut state.backend_data.ready_notified);
            crate::compositor_control::refresh_inventory(state, false);
            state
                .session_lock
                .sync_physical_state(state.backend_data.unknown_physical_outputs());
            render(state, node, crtc);
            crate::compositor_control::maintain(state);
        }
        Event::BreakFinished {
            node,
            epoch,
            transaction,
        } => {
            if !state
                .backend_data
                .secondary
                .devices
                .get(&node)
                .is_some_and(|device| device.epoch == epoch)
            {
                return;
            }
            if let Some(cutover) = state
                .backend_data
                .secondary
                .cutover
                .as_mut()
                .filter(|cutover| Some(cutover.serial) == transaction)
            {
                cutover.breaking.remove(&node.dev_id());
                if cutover.breaking.is_empty() {
                    complete_cutover(state);
                }
            }
        }
        Event::Failed {
            node,
            epoch,
            reason,
        } => {
            let Some(device) = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .filter(|device| device.epoch == epoch && !device.removed)
            else {
                return;
            };
            device.failure = Some(reason.clone());
            tracing::warn!(?node, %reason, "secondary GPU failed; healthy primary scanout retained");
            fail_cutover(state, reason);
            crate::compositor_control::refresh_inventory(state, false);
            state
                .session_lock
                .sync_physical_state(state.backend_data.unknown_physical_outputs());
        }
        Event::Exited { node, epoch } => {
            if !state
                .backend_data
                .secondary
                .devices
                .get(&node)
                .is_some_and(|device| device.owner_epoch == epoch)
            {
                return;
            }
            let device = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .expect("located owner");
            let restart = (device.restart && !state.backend_data.paused && !state.idle.blanked())
                .then(|| device.path.clone());
            device.restart = false;
            device.commands = None;
            device.active = false;
            device.admitting = false;
            let outputs = std::mem::take(&mut device.outputs);
            let removed = device.removed;
            for output in outputs.into_values() {
                unmap(state, output);
            }
            if removed {
                state.backend_data.secondary.devices.remove(&node);
            }
            if let Some(path) = restart {
                add(state, node, &path);
            }
            crate::backend::recompute_outputs(state);
        }
    }
}
fn auto_configure(state: &mut State, node: DrmNode) {
    let mut x = next_output_x(
        state
            .space
            .outputs()
            .filter_map(|output| crate::viewport::output_geometry(&state.space, output))
            .map(|geometry| geometry.loc.x + geometry.size.w),
    );
    let mut needs_primary = !state
        .space
        .outputs()
        .any(|output| output.current_mode().is_some());
    let Some(device) = state.backend_data.secondary.devices.get_mut(&node) else {
        return;
    };
    device.catalog.sort_by(|left, right| left.id.cmp(&right.id));
    let mut outputs = Vec::new();
    for display in device.catalog.iter().filter(|display| display.connected) {
        let mut configuration = display.current.clone();
        if !device
            .outputs
            .values()
            .any(|output| output.configuration.id == display.id)
        {
            let mode = display
                .modes
                .iter()
                .find(|mode| {
                    display.connector.starts_with("HDMI-")
                        && mode.width == 1920
                        && mode.height == 1080
                        && !mode.interlaced
                        && (59_000..=61_000).contains(&mode.refresh_millihz)
                })
                .or_else(|| display.modes.iter().find(|mode| mode.preferred))
                .or_else(|| display.modes.first());
            let Some(mode) = mode else {
                continue;
            };
            configuration.enabled = true;
            configuration.mode = Some(mode.id.clone());
            configuration.x = x;
            configuration.y = 0;
            configuration.primary = needs_primary;
            needs_primary = false;
            x = x.saturating_add(mode.width.min(i32::MAX as u32) as i32);
        }
        outputs.push(configuration);
    }
    if let Some(commands) = &device.commands {
        if commands
            .try_send(Command::Apply {
                profile: DisplayProfile {
                    outputs,
                    allow_headless: false,
                },
                transaction: None,
                finish_break: true,
            })
            .is_err()
        {
            device.failure = Some("Secondary initial output command admission is occupied".into());
        }
    }
}
pub(super) fn begin_restore(
    state: &mut State,
    profile: &DisplayProfile,
    token: Option<uuid::Uuid>,
    completion: Option<tokio::sync::oneshot::Sender<ControlReply>>,
) -> Result<bool, DisplayError> {
    if state
        .backend_data
        .secondary
        .devices
        .values()
        .filter(|device| !device.removed)
        .all(|device| device.commands.is_none())
    {
        return Ok(false);
    }
    if let Some(restore) = &state.backend_data.secondary.restore {
        if restore.profile == *profile && restore.token == token && completion.is_none() {
            return Ok(true);
        }
        return Err(DisplayError::new(
            "rollback_busy",
            "Another owned asynchronous native rollback is pending",
        ));
    }
    // A rollback supersedes the cutover, never its uncancelable queued driver work.
    state.backend_data.secondary.cutover = None;
    state.backend_data.secondary.completed = None;
    if let Some(check) = state.backend_data.secondary.pending_check.take() {
        let _ = check.reply.send(ControlReply {
            result: Err(error(
                "rollback_started",
                "Native recovery superseded output validation",
            )),
        });
    }
    if state.backend_data.secondary.devices.values().any(|device| {
        !device.removed && device.commands.is_some() && (device.stalled || device.admitting)
    }) {
        let message = match native_outputs::restore_primary(state, profile) {
            Ok(true) => "Saved primary-GPU targets were rearmed and still require their real frame/readback; full rollback is unavailable while a secondary driver owner remains occupied".into(),
            Ok(false) => "No saved enabled primary-GPU target is available; full rollback is unavailable while a secondary driver owner remains occupied".into(),
            Err(failure) => format!("Secondary driver owner remains occupied; saved primary-GPU recovery also failed: {failure}"),
        };
        crate::compositor_control::complete_native_restore(
            state,
            token,
            profile,
            completion,
            Err(DisplayError::new("worker_unavailable", message.clone())),
        );
        return Err(DisplayError::new("worker_unavailable", message));
    }
    state.backend_data.secondary.serial = state.backend_data.secondary.serial.wrapping_add(1);
    let serial = state.backend_data.secondary.serial;
    let mut awaiting = BTreeSet::new();
    for (node, device) in &state.backend_data.secondary.devices {
        if device.removed {
            continue;
        }
        if let Some(commands) = &device.commands {
            commands
                .try_send(Command::Test {
                    serial,
                    profile: profile.clone(),
                })
                .map_err(|_| {
                    DisplayError::new(
                        "worker_unavailable",
                        "Secondary rollback command admission is occupied or closed",
                    )
                })?;
            awaiting.insert(node.dev_id());
        }
    }
    state.backend_data.secondary.restore = Some(Restore {
        serial,
        token,
        profile: profile.clone(),
        completion,
        awaiting,
        applying: false,
        started: Instant::now(),
    });
    Ok(true)
}
fn apply_restore(state: &mut State) {
    let Some(restore) = state.backend_data.secondary.restore.as_mut() else {
        return;
    };
    if restore.applying {
        return;
    }
    restore.applying = true;
    let profile = restore.profile.clone();
    let epochs = state.backend_data.secondary.epochs();
    state.backend_data.secondary.checked = Some((profile.clone(), epochs));
    if let Err(failure) = native_outputs::apply(state, &profile) {
        finish_restore(state, Err(failure));
        return;
    }
    crate::backend::recompute_outputs_with_event(state, false);
    UdevBackendData::request_redraw(state);
    if state.backend_data.secondary.cutover.is_none() {
        let result = native_outputs::verify(state, &profile);
        finish_restore(state, result);
    }
}
fn finish_restore(state: &mut State, result: Result<(), DisplayError>) {
    let Some(restore) = state.backend_data.secondary.restore.take() else {
        return;
    };
    if result.is_err() {
        state.backend_data.secondary.cutover = None;
        state.backend_data.secondary.completed = None;
    }
    crate::compositor_control::complete_native_restore(
        state,
        restore.token,
        &restore.profile,
        restore.completion,
        result,
    );
}
/// Only display requests enter this async preparation. Unrelated input/protocol dispatch
/// continues normally while real worker TEST_ONLY is pending.
pub(super) fn defer_control(
    state: &mut State,
    request: &ControlRequest,
    reply: &mut Option<tokio::sync::oneshot::Sender<ControlReply>>,
) -> Result<bool, ControlError> {
    if let ControlRequest::RevertDisplays { token } = request {
        if !state.display_control.interactive_token(*token) {
            return Err(error(
                "operational_transaction",
                "Only the compositor owns operational transaction recovery",
            ));
        }
        let profile = state
            .display_control
            .rollback(*token)
            .map_err(|failure| error(&failure.code, failure.message))?;
        if state
            .backend_data
            .secondary
            .devices
            .values()
            .any(|device| device.commands.is_some())
        {
            begin_restore(state, &profile, Some(*token), reply.take())
                .map_err(|failure| error(&failure.code, failure.message))?;
            return Ok(true);
        }
    }
    let profile = match request {
        ControlRequest::ValidateDisplays { expected, profile } => {
            state
                .display_control
                .validate(
                    *expected,
                    profile,
                    state.backend_data.session_active(),
                    state.session_lock.locked,
                )
                .map_err(|failure| error(&failure.code, failure.message))?;
            profile.clone()
        }
        ControlRequest::PreviewDisplays { expected, profile } => {
            state
                .display_control
                .prepare(
                    *expected,
                    profile.clone(),
                    state.backend_data.session_active(),
                    state.session_lock.locked,
                )
                .map_err(|failure| error(&failure.code, failure.message))?
                .target
        }
        ControlRequest::RestoreConfirmedDisplays { expected, profile } => {
            state
                .display_control
                .prepare_restore(
                    *expected,
                    profile.clone(),
                    state.backend_data.session_active(),
                )
                .map_err(|failure| error(&failure.code, failure.message))?
                .target
        }
        ControlRequest::RevertDisplays { token } => state
            .display_control
            .rollback(*token)
            .map_err(|failure| error(&failure.code, failure.message))?,
        ControlRequest::SetPanelClosed {
            expected,
            closed,
            allow_headless,
        } => {
            if state.display_control.pending_token().is_some() {
                return Ok(false);
            }
            state
                .display_control
                .prepare_panel(
                    *expected,
                    *closed,
                    *allow_headless,
                    state.backend_data.session_active(),
                )
                .map_err(|failure| error(&failure.code, failure.message))?
                .target
        }
        _ => return Ok(false),
    };
    if state.backend_data.secondary.devices.values().any(|device| {
        !device.removed && device.commands.is_some() && (device.stalled || device.admitting)
    }) {
        return Err(error(
            "worker_unavailable",
            "Secondary owner has unresolved driver work; no further TEST_ONLY was queued",
        ));
    }
    if state.backend_data.secondary.checked(&profile) {
        return Ok(false);
    }
    if state.backend_data.secondary.pending_check.is_some() {
        return Err(error(
            "worker_busy",
            "Another native output TEST_ONLY request owns the worker broker",
        ));
    }
    let mut awaiting = BTreeSet::new();
    state.backend_data.secondary.serial = state.backend_data.secondary.serial.wrapping_add(1);
    let serial = state.backend_data.secondary.serial;
    for (node, device) in &state.backend_data.secondary.devices {
        if device.removed {
            continue;
        }
        if let Some(commands) = &device.commands {
            commands
                .try_send(Command::Test {
                    serial,
                    profile: profile.clone(),
                })
                .map_err(|_| {
                    error(
                        "worker_unavailable",
                        "Secondary TEST_ONLY command admission is occupied or closed",
                    )
                })?;
            awaiting.insert(node.dev_id());
        }
    }
    if awaiting.is_empty() {
        return Ok(false);
    }
    let reply = reply.take().ok_or_else(|| {
        error(
            "completion_missing",
            "Native preparation requires an owned asynchronous reply",
        )
    })?;
    state.backend_data.secondary.pending_check = Some(PendingCheck {
        serial,
        profile,
        request: request.clone(),
        reply,
        awaiting,
        started: Instant::now(),
    });
    Ok(true)
}
pub(super) fn begin_apply(
    state: &mut State,
    profile: &DisplayProfile,
    remove: Vec<(DrmNode, crtc::Handle)>,
) -> Result<bool, DisplayError> {
    let broker = &mut state.backend_data.secondary;
    if broker
        .devices
        .values()
        .filter(|device| !device.removed)
        .all(|device| device.commands.is_none())
    {
        return Ok(false);
    }
    if broker.completed.as_ref() == Some(profile) && broker.verify(profile).is_ok() {
        return Ok(false);
    }
    if broker
        .cutover
        .as_ref()
        .is_some_and(|cutover| cutover.profile == *profile)
    {
        return Ok(true);
    }
    if !broker.checked(profile) {
        return Err(DisplayError::new(
            "worker_unchecked",
            "Secondary TEST_ONLY has not completed for this exact profile and device lifetime",
        ));
    }
    broker.serial = broker.serial.wrapping_add(1);
    let serial = broker.serial;
    let awaiting = profile
        .outputs
        .iter()
        .filter(|output| output.enabled)
        .map(|output| output.id.clone())
        .collect();
    for device in broker.devices.values() {
        if device.removed {
            continue;
        }
        if let Some(commands) = &device.commands {
            commands
                .try_send(Command::Apply {
                    profile: profile.clone(),
                    transaction: Some(serial),
                    finish_break: false,
                })
                .map_err(|_| {
                    DisplayError::new(
                        "worker_unavailable",
                        "Secondary apply command admission is occupied or closed",
                    )
                })?;
        }
    }
    broker.completed = None;
    broker.cutover = Some(Cutover {
        serial,
        profile: profile.clone(),
        awaiting,
        held: Vec::new(),
        primary_remove: remove,
        breaking: BTreeSet::new(),
        started: Instant::now(),
    });
    Ok(true)
}
pub(super) fn defer_presentation(
    state: &mut State,
    id: &DisplayId,
    transaction: Option<u64>,
    preview: Option<uuid::Uuid>,
) -> bool {
    let Some(cutover) = state.backend_data.secondary.cutover.as_mut() else {
        return false;
    };
    if cutover.presented(id, transaction, preview) {
        finish_break(state);
    }
    true
}
fn finish_break(state: &mut State) {
    let Some(cutover) = state.backend_data.secondary.cutover.as_mut() else {
        return;
    };
    let remove = std::mem::take(&mut cutover.primary_remove);
    let profile = cutover.profile.clone();
    let serial = cutover.serial;
    for (node, crtc) in remove {
        if let Err(error) = native_outputs::disable_surface(state, node, crtc) {
            fail_cutover(state, error.to_string());
            return;
        }
    }
    native_outputs::record_disabled(state, &profile);
    let mut breaking = BTreeSet::new();
    for (node, device) in &state.backend_data.secondary.devices {
        if device.removed {
            continue;
        }
        if let Some(commands) = &device.commands {
            if commands
                .try_send(Command::Apply {
                    profile: profile.clone(),
                    transaction: Some(serial),
                    finish_break: true,
                })
                .is_err()
            {
                fail_cutover(
                    state,
                    "Secondary completion command admission is occupied or closed".into(),
                );
                return;
            }
            breaking.insert(node.dev_id());
        }
    }
    if let Some(cutover) = state.backend_data.secondary.cutover.as_mut() {
        cutover.breaking = breaking;
    }
    if state
        .backend_data
        .secondary
        .cutover
        .as_ref()
        .is_some_and(|cutover| cutover.breaking.is_empty())
    {
        complete_cutover(state);
    }
}
fn complete_cutover(state: &mut State) {
    let Some(cutover) = state.backend_data.secondary.cutover.take() else {
        return;
    };
    state.backend_data.secondary.completed = Some(cutover.profile.clone());
    for (id, preview) in cutover.held {
        state
            .display_control
            .frame_presented(&id, preview, Instant::now());
    }
    crate::backend::recompute_outputs_with_event(state, false);
    if state
        .backend_data
        .secondary
        .restore
        .as_ref()
        .is_some_and(|restore| restore.applying && restore.profile == cutover.profile)
    {
        let result = native_outputs::verify(state, &cutover.profile);
        finish_restore(state, result);
    }
    crate::compositor_control::maintain(state);
}
fn fail_cutover(state: &mut State, reason: String) {
    if state.backend_data.secondary.cutover.take().is_some() {
        // No pending transaction may confirm after a failed worker or lost lifetime.
        state.display_control.backend_failed(reason.clone());
    }
    state.backend_data.secondary.completed = None;
    if state.backend_data.secondary.restore.is_some() {
        finish_restore(
            state,
            Err(DisplayError::new("worker_recovery_failed", reason)),
        );
    }
}
pub(super) fn request_redraw(state: &mut State) {
    let nodes: Vec<_> = state
        .backend_data
        .secondary
        .devices
        .keys()
        .copied()
        .collect();
    for node in nodes {
        request_redraw_node(state, node);
    }
}
fn request_redraw_node(state: &mut State, node: DrmNode) {
    let crtcs: Vec<_> = state
        .backend_data
        .secondary
        .devices
        .get(&node)
        .map(|device| device.outputs.keys().copied().collect())
        .unwrap_or_default();
    for crtc in crtcs {
        state
            .loop_handle
            .insert_idle(move |state| render(state, node, crtc));
    }
}
fn retry(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    let Some(output) = state
        .backend_data
        .secondary
        .devices
        .get_mut(&node)
        .and_then(|device| device.outputs.get_mut(&crtc))
    else {
        return;
    };
    if output.retry.is_some() || output.busy.is_some() {
        return;
    }
    let delay = output
        .output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
        .map_or(Duration::from_millis(16), |mode| {
            Duration::from_secs_f64(1000.0 / f64::from(mode.refresh))
        });
    if let Ok(token) = state.loop_handle.insert_source(
        Timer::from_duration(delay),
        move |_, (), state: &mut State| {
            if let Some(output) = state
                .backend_data
                .secondary
                .devices
                .get_mut(&node)
                .and_then(|device| device.outputs.get_mut(&crtc))
            {
                output.retry = None;
            }
            render(state, node, crtc);
            TimeoutAction::Drop
        },
    ) {
        if let Some(output) = state
            .backend_data
            .secondary
            .devices
            .get_mut(&node)
            .and_then(|device| device.outputs.get_mut(&crtc))
        {
            output.retry = Some(token);
        }
    }
}
fn render(state: &mut State, node: DrmNode, crtc: crtc::Handle) {
    if state.backend_data.paused || state.idle.blanked() {
        return;
    }
    let Some(device) = state
        .backend_data
        .secondary
        .devices
        .get(&node)
        .filter(|device| device.active && device.failure.is_none())
    else {
        return;
    };
    let Some(remote) = device
        .outputs
        .get(&crtc)
        .filter(|output| output.busy.is_none())
    else {
        return;
    };
    let output = remote.output.clone();
    let configuration = remote.configuration.clone();
    let epoch = device.epoch;
    let transaction = state.backend_data.secondary.transaction_for(&configuration);
    let preview = state.display_control.queued_token_for(&configuration);
    let protected = state.session_lock.requires_protected_frame(&output);
    let now_ns = crate::window_bridge::now_monotonic_ns(&state.clock);
    let primary = state.backend_data.primary_gpu;
    let primary_capture = state.backend_data.logical_primary_output_id() == Some(&configuration.id);
    let backend = &mut state.backend_data;
    let Some(allocator) = backend.primary_allocator.as_mut() else {
        return;
    };
    let device = backend
        .secondary
        .devices
        .get_mut(&node)
        .expect("located remote device");
    let remote = device
        .outputs
        .get_mut(&crtc)
        .expect("located remote output");
    let result = (|| -> Result<bool, String> {
        // This is always a SINGLE primary-GPU renderer. Secondary import, copies, waits,
        // KMS and destruction are confined to the owner thread.
        let mut renderer = backend
            .gpus
            .single_renderer(&primary)
            .map_err(|error| error.to_string())?;
        let renderer = &mut renderer;
        let Some(mode) = output.current_mode() else {
            return Err("Secondary protocol output has no mode".into());
        };
        if remote.buffer.is_none() {
            let primary_formats = renderer.as_ref().egl_context().dmabuf_render_formats();
            let format = device.formats.iter().find(|format| CROSS_GPU_COLOR_FORMATS.contains(&format.code) && format.modifier == Modifier::Linear && primary_formats.contains(format)).copied().ok_or("Unsupported cross-GPU transport: no common explicit linear 8-bit DMA-BUF format; no CPU readback fallback")?;
            let buffer = allocator
                .create_buffer(
                    mode.size.w as u32,
                    mode.size.h as u32,
                    format.code,
                    &[format.modifier],
                )
                .map_err(|error| error.to_string())?;
            remote.buffer = Some(buffer.export().map_err(|error| error.to_string())?);
            remote.format = Some(format);
        }
        let scene_output = if !state.session_lock.locked {
            configuration
                .mirror_of
                .as_ref()
                .and_then(|source| {
                    state
                        .space
                        .outputs()
                        .find(|candidate| candidate.name() == source.0)
                })
                .unwrap_or(&output)
        } else {
            &output
        };
        let cursor = state
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
        let scene = crate::scene::collect(
            &mut state.chrome,
            &mut state.wallpaper,
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
        let elements = cursor
            .into_iter()
            .map(crate::scene::SceneElement::Cursor)
            .chain(scene);
        let mut elements: Vec<_> = if !configuration.insets.is_zero() || scene_output != &output {
            crate::viewport::constrain_elements_at_scale(
                &output,
                elements,
                scene_output.current_scale().fractional_scale(),
            )
            .ok_or("Secondary viewport lacks physical mode")?
            .map(crate::scene::NativeSceneElement::Calibrated)
            .collect()
        } else {
            elements
                .map(crate::scene::NativeSceneElement::Plain)
                .collect()
        };
        if !state.session_lock.locked
            && let Some((number, label, calibration)) =
                state.display_control.identification_for(&configuration.id)
            && let Some(element) = crate::chrome::collect_identification(
                &mut state.chrome,
                &output,
                number,
                label,
                calibration,
                renderer,
            )
        {
            elements.insert(0, crate::scene::NativeSceneElement::Identification(element));
        }
        // This renderer belongs to the PRIMARY GPU even for an external-only
        // desktop. The secondary EGL/KMS lane never captures or reads pixels.
        crate::screencopy::drain_pending(
            &mut state.screencopy,
            &state.windows,
            &state.clock,
            state.start_time.elapsed(),
            renderer,
            &output,
            &elements,
            primary_capture,
        );
        if protected || transaction.is_some() {
            remote.tracker = OutputDamageTracker::from_output(&output);
        }
        let buffer = remote.buffer.as_mut().expect("allocated transport buffer");
        let (damaged, sync, states) = {
            let mut target = renderer.bind(buffer).map_err(|error| error.to_string())?;
            let result = remote
                .tracker
                .render_output(
                    renderer,
                    &mut target,
                    1,
                    &elements,
                    if state.session_lock.locked {
                        [0.0, 0.0, 0.0, 1.0]
                    } else {
                        CLEAR_COLOR
                    },
                )
                .map_err(|error| error.to_string())?;
            (result.damage.is_some(), result.sync, result.states)
        };
        track_primary(&state.space, &output, &states);
        if !damaged {
            return Ok(false);
        }
        let requires_fence = sync.contains_fence();
        let fence = if requires_fence {
            Some(sync.export().ok_or("Primary EGL cannot export a native fence; secondary admission refused without an untimed fallback")?)
        } else {
            None
        };
        let wallpaper = if state.session_lock.locked {
            None
        } else {
            state
                .wallpaper
                .frame_token(&scene_output.name(), &states)
                .map(|revision| (scene_output.name(), revision))
        };
        remote.serial = remote.serial.wrapping_add(1);
        let serial = remote.serial;
        let packet = FramePacket {
            epoch,
            serial,
            configuration,
            buffer: buffer.clone(),
            fence,
            requires_fence,
            metadata: FrameUserData {
                lock_generation: state.session_lock.frame_generation(),
                preview,
                wallpaper,
                transaction,
            },
        };
        device
            .commands
            .as_ref()
            .ok_or("Secondary GPU owner exited before frame submission")?
            .try_send(Command::Frame { crtc, packet })
            .map_err(|_| "Secondary frame admission is occupied or closed")?;
        remote.busy = Some((serial, Instant::now()));
        Ok(true)
    })();
    match result {
        Ok(false) => retry(state, node, crtc),
        Ok(true) => {
            for window in state.space.elements() {
                window.send_frame(
                    &output,
                    state.start_time.elapsed(),
                    None,
                    smithay::desktop::utils::surface_primary_scanout_output,
                );
            }
        }
        Err(reason) => {
            if let Some(device) = state.backend_data.secondary.devices.get_mut(&node) {
                device.failure = Some(reason.clone());
            }
            state.wallpaper.frame_failed(&output.name(), &reason);
            fail_cutover(state, reason);
            crate::compositor_control::refresh_inventory(state, false);
            state
                .session_lock
                .sync_physical_state(state.backend_data.unknown_physical_outputs());
        }
    }
}
pub(super) fn set_blanked(state: &mut State, blanked: bool) {
    let mut dormant = Vec::new();
    for (node, device) in &mut state.backend_data.secondary.devices {
        if device.removed || device.stalled {
            continue;
        }
        if device.admitting {
            if blanked {
                device.admission_cancel.store(true, Ordering::Release);
            } else {
                device.restart = true;
            }
            continue;
        }
        device.known = false;
        if let Some(commands) = &device.commands {
            let _ = commands.try_send(Command::Blank(blanked));
        } else if !blanked {
            dormant.push((*node, device.path.clone()));
        }
    }
    for (node, path) in dormant {
        add(state, node, &path);
    }
    if !blanked {
        request_redraw(state);
    }
}
pub(super) fn pause(state: &mut State) {
    let broker = &mut state.backend_data.secondary;
    for device in broker.devices.values_mut() {
        // Acquisition retains its owner epoch until its one FD result is handled.
        if device.admitting {
            device.admission_cancel.store(true, Ordering::Release);
        } else if device.commands.is_some() {
            broker.serial = broker.serial.wrapping_add(1);
            device.epoch = native_policy::Epoch(broker.serial);
        }
        device.active = false;
        device.known = false;
        device.first_presented = false;
        for output in device.outputs.values_mut() {
            output.presented = false;
        }
        if !device.admitting {
            if let Some(commands) = &device.commands {
                let _ = commands.try_send(Command::Pause(device.epoch));
            }
        }
    }
    if let Some(check) = state.backend_data.secondary.pending_check.take() {
        let _ = check.reply.send(ControlReply {
            result: Err(error(
                "inactive_seat",
                "Seat pause superseded secondary TEST_ONLY; its driver work is not canceled",
            )),
        });
    }
    state.backend_data.secondary.checked = None;
    fail_cutover(
        state,
        "Seat pause invalidated secondary GPU transaction lifetime".into(),
    );
}
pub(super) fn resume(state: &mut State) {
    let recovery = state.backend_data.secondary.recovery.clone();
    let events = state.backend_data.secondary.events.clone();
    let runtime = state.backend_data.secondary.runtime.clone();
    let mut dormant = Vec::new();
    let broker = &mut state.backend_data.secondary;
    for (node, device) in &mut broker.devices {
        if device.removed {
            continue;
        }
        if device.commands.is_none() {
            if !device.admitting {
                dormant.push((*node, device.path.clone()));
            }
            continue;
        }
        if device.admitting {
            device.restart = true;
            continue;
        }
        broker.serial = broker.serial.wrapping_add(1);
        device.epoch = native_policy::Epoch(broker.serial);
        device.admission_cancel.store(false, Ordering::Release);
        device.stalled = false;
        device.known = false;
        device.started = Instant::now();
        device.admitting = true;
        device.first_presented = false;
        if let Some(lease) = recovery.clone() {
            let gpu = device.gpu.clone();
            let events = events.clone();
            let node = *node;
            let epoch = device.epoch;
            runtime.spawn(async move {
                let result = tokio::time::timeout(Duration::from_secs(7), lease.resume(&gpu))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                if result.is_err() {
                    let _ = tokio::time::timeout(Duration::from_secs(3), lease.close(&gpu)).await;
                }
                let _ = events.send(Event::Rearmed {
                    node,
                    epoch,
                    result,
                });
            });
        } else {
            device.admitting = false;
            device.failure = Some("Trusted secondary rearm authority unavailable".into());
        }
    }
    for (node, path) in dormant {
        add(state, node, &path);
    }
}
pub(super) fn maintain(state: &mut State) {
    if state
        .backend_data
        .secondary
        .restore
        .as_ref()
        .is_some_and(|restore| {
            restore.started.elapsed() >= crate::display_control::PRESENTATION_TIMEOUT
        })
    {
        fail_cutover(
            state,
            "Secondary GPU rollback timed out; old physical ownership retained".into(),
        );
    }
    if state
        .backend_data
        .secondary
        .pending_check
        .as_ref()
        .is_some_and(|check| check.started.elapsed() >= native_policy::WORKER_TIMEOUT)
    {
        let check = state
            .backend_data
            .secondary
            .pending_check
            .take()
            .expect("timed-out check");
        state.backend_data.secondary.checked = None;
        for (node, device) in &mut state.backend_data.secondary.devices {
            if check.awaiting.contains(&node.dev_id()) {
                device.stalled = true;
                device.known = false;
                device.admission_cancel.store(true, Ordering::Release);
                device.failure = Some(
                    "Secondary TEST_ONLY timed out; its admitted driver owner remains occupied"
                        .into(),
                );
                if let Some(commands) = &device.commands {
                    let _ = commands.try_send(Command::Shutdown);
                }
            }
        }
        let _ = check.reply.send(ControlReply {
            result: Err(error(
                "worker_timeout",
                "Secondary GPU TEST_ONLY timed out; no output changes were admitted",
            )),
        });
    }
    let expired: Vec<_> = state
        .backend_data
        .secondary
        .devices
        .iter()
        .filter(|(_, device)| {
            !device.removed
                && !device.stalled
                && ((device.admitting || device.commands.is_some() && !device.known)
                    && device.started.elapsed() >= native_policy::WORKER_TIMEOUT
                    || device.outputs.values().any(|output| {
                        output.busy.is_some_and(|(_, queued)| {
                            queued.elapsed() >= native_policy::WORKER_TIMEOUT
                        })
                    }))
        })
        .map(|(node, _)| *node)
        .collect();
    for node in expired {
        if let Some(device) = state.backend_data.secondary.devices.get_mut(&node) {
            device.failure = Some("Secondary driver owner timed out; old scanout and in-flight DMA-BUF ownership retained. Primary input/rendering continue; physical output is unknown.".into());
            device.known = false;
            device.stalled = true;
            device.admission_cancel.store(true, Ordering::Release);
            if let Some(commands) = &device.commands {
                let _ = commands.try_send(Command::Shutdown);
            }
            // Do not reuse busy transport buffers, join the driver, or synthesize completion.
        }
        fail_cutover(
            state,
            "Secondary GPU owner timed out; make-before-break retained old output".into(),
        );
        crate::compositor_control::refresh_inventory(state, false);
        state
            .session_lock
            .sync_physical_state(state.backend_data.unknown_physical_outputs());
    }
    if state
        .backend_data
        .secondary
        .cutover
        .as_ref()
        .is_some_and(|cutover| {
            cutover.started.elapsed() >= crate::display_control::PRESENTATION_TIMEOUT
        })
    {
        fail_cutover(
            state,
            "Native cross-device presentation timed out before break".into(),
        );
    }
    if state
        .backend_data
        .secondary
        .cutover
        .as_ref()
        .is_some_and(|cutover| cutover.awaiting.is_empty() && cutover.breaking.is_empty())
    {
        finish_break(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn make_before_break_waits_for_every_target_and_rejects_stale_or_duplicate_frames() {
        let panel = DisplayId("panel".into());
        let tv = DisplayId("tv".into());
        let mut cutover = Cutover {
            serial: 7,
            profile: DisplayProfile::default(),
            awaiting: BTreeSet::from([panel.clone(), tv.clone()]),
            held: Vec::new(),
            primary_remove: Vec::new(),
            breaking: BTreeSet::new(),
            started: Instant::now(),
        };
        assert!(!cutover.presented(&tv, Some(6), None));
        assert_eq!(cutover.awaiting.len(), 2);
        assert!(!cutover.presented(&panel, Some(7), None));
        assert!(!cutover.presented(&panel, Some(7), None));
        assert_eq!(cutover.held.len(), 1);
        assert!(cutover.presented(&tv, Some(7), None));
        assert!(cutover.awaiting.is_empty());
        assert_eq!(cutover.held.len(), 2);
        cutover.breaking.insert(3);
        assert!(!cutover.presented(&tv, Some(7), None));
    }
    #[test]
    fn secondary_command_admission_is_finite_and_nonblocking() {
        let (commands, _owner) = channel::sync_channel(1);
        assert!(commands.try_send(Command::Scan).is_ok());
        assert!(matches!(
            commands.try_send(Command::Scan),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
    }
}
