//! All secondary DRM/GBM/EGL objects are constructed and destroyed on this thread.
//! Only owned file descriptors, DMA-BUFs and plain metadata cross the broker channel.
use super::super::*;
use super::{Command, Event, FramePacket, OutputSnapshot, Snapshot};
use smithay::backend::{
    drm::compositor::{DrmCompositor, PrimaryPlaneElement},
    egl::fence::EGLFence,
    renderer::{
        Renderer,
        element::{Id, Kind, texture::TextureRenderElement},
    },
};
use smithay::reexports::calloop::channel;
use smithay::reexports::drm::control::Device as _;
use std::{
    collections::{BTreeSet, HashMap},
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use templar_types::display::{DisplayError, DisplayProfile, OutputConfiguration, PhysicalState};

type Compositor = DrmCompositor<Allocator, Exporter, FramePacket, DrmDeviceFd>;
struct Surface {
    output: Output,
    configuration: OutputConfiguration,
    connector: connector::Handle,
    compositor: Compositor,
    pending: bool,
    pending_serial: Option<u64>,
    rearmed: crate::flip_recovery::RearmedAt,
    failure: Option<String>,
    readback_pending: bool,
}
impl Surface {
    fn clear(
        &mut self,
        node: DrmNode,
        epoch: native_policy::Epoch,
        crtc: crtc::Handle,
        replies: &channel::Sender<Event>,
    ) -> Result<(), String> {
        self.compositor.clear().map_err(|error| error.to_string())?;
        self.pending = false;
        self.rearmed = now_cutoff();
        if let Some(serial) = self.pending_serial.take() {
            let _ = replies.send(Event::Retired {
                node,
                epoch,
                crtc,
                serial,
            });
        }
        Ok(())
    }
}
struct Worker {
    node: DrmNode,
    epoch: native_policy::Epoch,
    device: DeviceData,
    allocator: Allocator,
    exporter: Exporter,
    gbm: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,
    surfaces: HashMap<crtc::Handle, Surface>,
    replies: channel::Sender<Event>,
    active: bool,
    blanked: bool,
    transaction: Option<u64>,
    late_edid_done: bool,
    late_edid_pending: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ScanAction {
    NoSink,
    Reprobe,
    Wait,
    Ready,
    NoTimings,
}

fn scan_action(
    connectors: impl IntoIterator<Item = (bool, bool)>,
    late_edid_done: bool,
    late_edid_pending: bool,
) -> ScanAction {
    let mut present = false;
    let mut usable = false;
    let mut late = false;
    for (connected, has_modes) in connectors {
        present |= connected;
        usable |= connected && has_modes;
        late |= connected && !has_modes;
    }
    if !present {
        ScanAction::NoSink
    } else if late && !late_edid_done {
        ScanAction::Reprobe
    } else if usable {
        ScanAction::Ready
    } else if late_edid_pending {
        ScanAction::Wait
    } else {
        ScanAction::NoTimings
    }
}
impl Worker {
    fn snapshot(&mut self, topology: bool) {
        native_outputs::refresh_catalog(&mut self.device);
        for (connector, display) in &mut self.device.catalog {
            if let Some(surface) = self
                .surfaces
                .values()
                .find(|surface| surface.connector == *connector)
            {
                display.current = surface.configuration.clone();
                if let Some(reason) = &surface.failure {
                    display.configure = templar_types::settings::Capability::Unavailable {
                        reason: reason.clone(),
                    };
                }
            }
        }
        let outputs = self
            .surfaces
            .iter()
            .map(|(crtc, surface)| OutputSnapshot {
                crtc: *crtc,
                configuration: surface.configuration.clone(),
                mode: surface
                    .output
                    .current_mode()
                    .expect("worker output has an owned mode"),
                physical_size: surface.output.physical_properties().size,
            })
            .collect();
        let failures = self
            .surfaces
            .values()
            .filter_map(|surface| {
                surface
                    .failure
                    .as_ref()
                    .map(|reason| templar_types::display::DisplayFailure {
                        gpu: self.device.gpu_id.clone(),
                        connector: self
                            .device
                            .catalog
                            .get(&surface.connector)
                            .map(|display| display.connector.clone()),
                        reason: reason.clone(),
                    })
            })
            .collect();
        let snapshot = Snapshot {
            catalog: self.device.catalog.values().cloned().collect(),
            outputs,
            formats: self.renderer.dmabuf_formats(),
            active: self.active,
            blanked: self.blanked,
            failures,
            topology,
        };
        let _ = self.replies.send(Event::Snapshot {
            node: self.node,
            epoch: self.epoch,
            snapshot,
        });
    }
    fn plan<'a>(
        &self,
        profile: &'a DisplayProfile,
    ) -> Result<Vec<native_outputs::Plan<'a>>, DisplayError> {
        let drm = self.device.drm_output_manager.device();
        let resources = drm.resource_handles().map_err(|error| {
            DisplayError::new("native_backend", format!("Worker CRTC resources: {error}"))
        })?;
        let mut candidates = Vec::new();
        for configuration in profile
            .outputs
            .iter()
            .filter(|configuration| configuration.enabled)
        {
            let Some((connector, _)) = self
                .device
                .catalog
                .iter()
                .find(|(_, display)| display.id == configuration.id)
            else {
                continue;
            };
            let info = self
                .device
                .scanner
                .connectors()
                .get(connector)
                .ok_or_else(|| DisplayError::new("output_removed", "Worker connector removed"))?;
            let mode = info
                .modes()
                .iter()
                .find(|mode| {
                    configuration
                        .mode
                        .as_ref()
                        .is_some_and(|id| *id == native_outputs::mode_id(mode))
                })
                .copied()
                .ok_or_else(|| {
                    DisplayError::new("mode_removed", "Worker exact mode no longer advertised")
                })?;
            let mut crtcs = Vec::new();
            if let Some((crtc, _)) = self
                .surfaces
                .iter()
                .find(|(_, surface)| surface.connector == *connector)
            {
                crtcs.push(*crtc);
            }
            for encoder in info.encoders() {
                let encoder = drm.get_encoder(*encoder).map_err(|error| {
                    DisplayError::new("native_backend", format!("Worker encoder: {error}"))
                })?;
                for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
                    if !crtcs.contains(&crtc) {
                        crtcs.push(crtc);
                    }
                }
            }
            crtcs.sort_by_key(|crtc| u32::from(*crtc));
            candidates.push((*connector, mode, configuration, crtcs));
        }
        fn assign(
            index: usize,
            candidates: &[(
                connector::Handle,
                smithay::reexports::drm::control::Mode,
                &OutputConfiguration,
                Vec<crtc::Handle>,
            )],
            owners: &mut HashMap<crtc::Handle, usize>,
            seen: &mut BTreeSet<u32>,
        ) -> bool {
            for crtc in &candidates[index].3 {
                if !seen.insert(u32::from(*crtc)) {
                    continue;
                }
                if owners
                    .get(crtc)
                    .copied()
                    .is_none_or(|old| assign(old, candidates, owners, seen))
                {
                    owners.insert(*crtc, index);
                    return true;
                }
            }
            false
        }
        let mut owners = HashMap::new();
        for index in 0..candidates.len() {
            if !assign(index, &candidates, &mut owners, &mut BTreeSet::new()) {
                return Err(DisplayError::new(
                    "crtc_unavailable",
                    "Worker cannot assign distinct CRTCs",
                ));
            }
        }
        Ok(candidates
            .into_iter()
            .enumerate()
            .map(|(index, (connector, mode, configuration, crtcs))| {
                let crtc = crtcs
                    .into_iter()
                    .find(|crtc| owners.get(crtc) == Some(&index))
                    .expect("assigned CRTC");
                native_outputs::Plan {
                    node: self.node,
                    crtc,
                    connector,
                    mode,
                    configuration,
                }
            })
            .collect())
    }
    fn test(&self, profile: &DisplayProfile) -> Result<(), DisplayError> {
        native_outputs::test_device(self.node, &self.device, profile, &self.plan(profile)?, true)
    }
    fn apply(
        &mut self,
        profile: &DisplayProfile,
        transaction: Option<u64>,
        finish_break: bool,
    ) -> Result<(), String> {
        let plans = self.plan(profile).map_err(|error| error.to_string())?;
        native_outputs::test_device(self.node, &self.device, profile, &plans, true)
            .map_err(|error| error.to_string())?;
        // Add or reconfigure target outputs first. Outputs on a different device are never
        // disabled here; FinishBreak arrives only after all target GPUs have presented.
        let conflict: Vec<_> = self
            .surfaces
            .iter()
            .filter(|(crtc, surface)| {
                plans
                    .iter()
                    .any(|plan| plan.crtc == **crtc && plan.connector != surface.connector)
            })
            .map(|(crtc, _)| *crtc)
            .collect();
        for crtc in conflict {
            self.clear_remove(crtc)?;
        }
        for plan in &plans {
            if finish_break
                && self.transaction == transaction
                && self
                    .surfaces
                    .get(&plan.crtc)
                    .is_some_and(|surface| surface.configuration == *plan.configuration)
            {
                continue;
            }
            native_outputs::apply_underscan(
                &self.device,
                plan.connector,
                templar_types::display::Insets::default(),
            )
            .map_err(|error| error.to_string())?;
            native_outputs::apply_eight_bit(&self.device, plan.connector)
                .map_err(|error| error.to_string())?;
            if let Some(surface) = self.surfaces.get_mut(&plan.crtc) {
                if surface.compositor.pending_mode() != plan.mode {
                    surface.clear(self.node, self.epoch, plan.crtc, &self.replies)?;
                    surface
                        .compositor
                        .use_mode(plan.mode)
                        .map_err(|error| error.to_string())?;
                }
                surface.configuration = plan.configuration.clone();
                surface.output.change_current_state(
                    Some(Mode::from(plan.mode)),
                    Some(Transform::Normal),
                    Some(smithay::output::Scale::Fractional(1.0)),
                    Some((0, 0).into()),
                );
                surface.readback_pending = true;
                surface.compositor.reset_buffers();
            } else {
                let definition = self
                    .device
                    .catalog
                    .get(&plan.connector)
                    .ok_or("Worker native display disappeared")?;
                let info = self
                    .device
                    .scanner
                    .connectors()
                    .get(&plan.connector)
                    .ok_or("Worker connector disappeared")?;
                let output = Output::new(
                    definition.id.0.clone(),
                    PhysicalProperties {
                        size: info
                            .size()
                            .map_or((0, 0), |(w, h)| (w as i32, h as i32))
                            .into(),
                        subpixel: info.subpixel().into(),
                        make: definition.make.clone(),
                        model: definition.model.clone(),
                        serial_number: definition.serial.clone().unwrap_or_default(),
                    },
                );
                output.change_current_state(
                    Some(Mode::from(plan.mode)),
                    Some(Transform::Normal),
                    Some(smithay::output::Scale::Fractional(1.0)),
                    Some((0, 0).into()),
                );
                let drm = self.device.drm_output_manager.device_mut();
                let mut planes = drm.planes(&plan.crtc).map_err(|error| error.to_string())?;
                // Owned full-frame transport does not expose client planes or a cursor plane.
                planes.overlay.clear();
                planes.cursor.clear();
                let raw = drm
                    .create_surface(plan.crtc, plan.mode, &[plan.connector])
                    .map_err(|error| error.to_string())?;
                let compositor = DrmCompositor::new(
                    &output,
                    raw,
                    Some(planes),
                    self.allocator.clone(),
                    self.exporter.clone(),
                    CROSS_GPU_COLOR_FORMATS.iter().copied(),
                    self.renderer
                        .egl_context()
                        .dmabuf_render_formats()
                        .iter()
                        .copied(),
                    (64, 64).into(),
                    Some(self.gbm.clone()),
                )
                .map_err(|error| error.to_string())?;
                self.surfaces.insert(
                    plan.crtc,
                    Surface {
                        output,
                        configuration: plan.configuration.clone(),
                        connector: plan.connector,
                        compositor,
                        pending: false,
                        pending_serial: None,
                        rearmed: now_cutoff(),
                        failure: None,
                        readback_pending: true,
                    },
                );
            }
        }
        if finish_break {
            let remove: Vec<_> = self
                .surfaces
                .iter()
                .filter(|(crtc, surface)| {
                    !plans
                        .iter()
                        .any(|plan| plan.crtc == **crtc && plan.connector == surface.connector)
                })
                .map(|(crtc, _)| *crtc)
                .collect();
            for crtc in remove {
                self.clear_remove(crtc)?;
            }
            for configuration in &profile.outputs {
                if !configuration.enabled
                    && let Some(display) = self
                        .device
                        .catalog
                        .values_mut()
                        .find(|display| display.id == configuration.id)
                {
                    display.current = configuration.clone();
                }
            }
        }
        self.transaction = transaction;
        self.snapshot(false);
        Ok(())
    }
    fn clear_remove(&mut self, crtc: crtc::Handle) -> Result<(), String> {
        if let Some(surface) = self.surfaces.get_mut(&crtc) {
            surface.clear(self.node, self.epoch, crtc, &self.replies)?;
        }
        self.surfaces.remove(&crtc);
        Ok(())
    }
    fn scan(
        &mut self,
        loop_handle: &smithay::reexports::calloop::LoopHandle<'static, Self>,
        signal: &smithay::reexports::calloop::LoopSignal,
    ) -> Result<(), String> {
        self.device
            .scanner
            .scan_connectors(self.device.drm_output_manager.device())
            .map_err(|error| error.to_string())?;
        self.device.connector_scan_known = true;
        native_outputs::refresh_catalog(&mut self.device);
        // Raw scanner disconnects are insufficient: EDID/live routing can still
        // attest presence. Reconcile every owned surface on each scan, including
        // later loss of that evidence without another raw status transition.
        let removed: Vec<_> = self
            .surfaces
            .iter()
            .filter(|(_, surface)| {
                !self
                    .device
                    .catalog
                    .get(&surface.connector)
                    .is_some_and(|display| display.connected)
            })
            .map(|(crtc, _)| *crtc)
            .collect();
        for crtc in removed {
            self.clear_remove(crtc)?;
        }
        match scan_action(
            self.device
                .catalog
                .values()
                .map(|display| (display.connected, !display.modes.is_empty())),
            self.late_edid_done,
            self.late_edid_pending,
        ) {
            ScanAction::NoSink => {
                self.snapshot(true);
                // Release GBM/EGL/DRM only on their owner thread, permitting runtime suspend.
                signal.stop();
                return Ok(());
            }
            ScanAction::Reprobe => {
                self.late_edid_done = true;
                self.late_edid_pending = true;
                let handle = loop_handle.clone();
                let signal = signal.clone();
                loop_handle
                    .insert_source(
                        Timer::from_duration(native_policy::LATE_EDID_REPROBE),
                        move |_, (), worker| {
                            worker.late_edid_pending = false;
                            if let Err(error) = worker.scan(&handle, &signal) {
                                worker.fail(error);
                                signal.stop();
                            }
                            TimeoutAction::Drop
                        },
                    )
                    .map_err(|error| error.to_string())?;
            }
            ScanAction::NoTimings => {
                self.snapshot(true);
                self.fail("Secondary connector did not advertise usable timings after its one bounded late-EDID probe".into());
                signal.stop();
                return Ok(());
            }
            ScanAction::Wait | ScanAction::Ready => {}
        }
        self.snapshot(true);
        Ok(())
    }
    fn render(&mut self, crtc: crtc::Handle, packet: FramePacket) -> Result<(), String> {
        if packet.epoch != self.epoch
            || !self.active
            || self.blanked
            || !self
                .surfaces
                .get(&crtc)
                .is_some_and(|surface| surface.configuration == packet.configuration)
        {
            // Delayed work from a replaced configuration is retirement, not a
            // fabricated presentation or a new GPU failure.
            let _ = self.replies.send(Event::Retired {
                node: self.node,
                epoch: self.epoch,
                crtc,
                serial: packet.serial,
            });
            return Ok(());
        }
        let surface = self
            .surfaces
            .get_mut(&crtc)
            .expect("checked owned worker configuration");
        if surface.pending {
            return Err("Worker already owns a pending scanout frame".into());
        }
        let fence = packet
            .fence
            .as_ref()
            .map(|fd| fd.try_clone())
            .transpose()
            .map_err(|error| error.to_string())?;
        if let Some(fence) = fence {
            let imported = EGLFence::import(self.renderer.egl_context().display(), fence)
                .map_err(|error| format!("Native fence import unsupported: {error}"))?;
            if !imported
                .client_wait(Some(native_policy::FENCE_TIMEOUT), false)
                .map_err(|error| error.to_string())?
            {
                return Err("Imported primary-GPU fence timed out; old scanout retained".into());
            }
        } else if packet.requires_fence {
            return Err(
                "Primary-GPU frame has no exportable native fence; CPU fallback is not admitted"
                    .into(),
            );
        }
        let texture = self
            .renderer
            .import_dmabuf(&packet.buffer, None)
            .map_err(|error| {
                format!("Cross-GPU DMA-BUF import unsupported; no CPU fallback: {error}")
            })?;
        let element = TextureRenderElement::from_static_texture(
            Id::new(),
            self.renderer.context_id(),
            (0.0, 0.0),
            texture,
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            Kind::Unspecified,
        );
        surface.compositor.reset_buffers();
        let elements = [element];
        let result = surface
            .compositor
            .render_frame(
                &mut self.renderer,
                &elements,
                CLEAR_COLOR,
                FrameFlags::empty(),
            )
            .map_err(|error| error.to_string())?;
        if result.is_empty {
            return Err("Worker produced no owned frame for the submitted DMA-BUF".into());
        }
        if result.needs_sync()
            && let PrimaryPlaneElement::Swapchain(element) = result.primary_element
        {
            native_fence::bounded(&element.sync)?;
        }
        let serial = packet.serial;
        surface
            .compositor
            .queue_frame(packet)
            .map_err(|error| error.to_string())?;
        surface.pending = true;
        surface.pending_serial = Some(serial);
        Ok(())
    }
    fn presented(
        &mut self,
        crtc: crtc::Handle,
        timestamp: Option<smithay::backend::drm::DrmEventTime>,
    ) {
        let Some(surface) = self.surfaces.get_mut(&crtc) else {
            return;
        };
        if !surface.pending || !surface.rearmed.accepts(timestamp) {
            return;
        }
        match surface.compositor.frame_submitted() {
            Ok(Some(packet)) => {
                surface.pending = false;
                surface.pending_serial = None;
                if packet.configuration != surface.configuration {
                    let _ = self.replies.send(Event::Retired {
                        node: self.node,
                        epoch: self.epoch,
                        crtc,
                        serial: packet.serial,
                    });
                    return;
                }
                surface.failure = None;
                let configuration = surface.configuration.clone();
                // Read back once per owned transaction, not on every vblank/string difference.
                if std::mem::take(&mut surface.readback_pending) {
                    if let Err(error) =
                        native_outputs::verify_eight_bit(&self.device, surface.connector)
                    {
                        surface.failure = Some(error.to_string());
                        self.fail(error.to_string());
                        return;
                    }
                    if let Some(info) = self.device.scanner.connectors().get(&surface.connector) {
                        let drm = self.device.drm_output_manager.device();
                        let actual = drm.get_crtc(crtc).map_err(|error| error.to_string());
                        let requested = info.modes().iter().find(|mode| {
                            configuration
                                .mode
                                .as_ref()
                                .is_some_and(|id| *id == native_outputs::mode_id(mode))
                        });
                        let matches = actual
                            .as_ref()
                            .ok()
                            .and_then(|actual| actual.mode())
                            .zip(requested)
                            .is_some_and(|(actual, requested)| {
                                native_policy::same_signal(requested, &actual)
                            });
                        let route = native_outputs::property(drm, surface.connector, "CRTC_ID")
                            .map(|(_, _, route)| route);
                        if !matches || route.ok() != Some(u64::from(u32::from(crtc))) {
                            let reason = format!(
                                "Secondary timing/routing readback mismatch: requested {:?}, actual {:?}, CRTC {:?}",
                                configuration.mode,
                                actual
                                    .as_ref()
                                    .ok()
                                    .and_then(|actual| actual.mode())
                                    .as_ref()
                                    .map(native_outputs::mode_id),
                                crtc
                            );
                            surface.failure = Some(reason.clone());
                            self.fail(reason);
                            return;
                        }
                        let physical = native_outputs::physical_state(drm, info);
                        if let Some(display) = self.device.catalog.get_mut(&surface.connector) {
                            display.physical = physical;
                        }
                        if !matches!(
                            self.device
                                .catalog
                                .get(&surface.connector)
                                .map(|display| &display.physical),
                            Some(PhysicalState::NativeEnabled { .. })
                        ) {
                            surface.failure = Some(
                                "Secondary presented frame lacks an active physical readback"
                                    .into(),
                            );
                            self.fail(
                                "Secondary presented frame lacks an active physical readback"
                                    .into(),
                            );
                            return;
                        }
                    } else {
                        self.fail(
                            "Secondary connector disappeared before owned presentation readback"
                                .into(),
                        );
                        return;
                    }
                    self.snapshot(false);
                }
                let _ = self.replies.send(Event::Presented {
                    node: self.node,
                    epoch: self.epoch,
                    crtc,
                    serial: packet.serial,
                    metadata: packet.metadata,
                });
            }
            Ok(None) => {}
            Err(error) => {
                surface.failure = Some(format!(
                    "Actual worker frame-submitted acknowledgement failed: {error}"
                ));
                self.fail(error.to_string());
            }
        }
    }
    fn fail(&mut self, reason: String) {
        let _ = self.replies.send(Event::Failed {
            node: self.node,
            epoch: self.epoch,
            reason,
        });
    }
    fn command(
        &mut self,
        command: Command,
        handle: &smithay::reexports::calloop::LoopHandle<'static, Self>,
        signal: &smithay::reexports::calloop::LoopSignal,
    ) {
        let result = match command {
            Command::Scan => self.scan(handle, signal),
            Command::Test { serial, profile } => {
                let result = self.test(&profile).map_err(|error| error.to_string());
                let _ = self.replies.send(Event::Tested {
                    node: self.node,
                    epoch: self.epoch,
                    serial,
                    result,
                });
                Ok(())
            }
            Command::Apply {
                profile,
                transaction,
                finish_break,
            } => {
                let result = self.apply(&profile, transaction, finish_break);
                if result.is_ok() && finish_break {
                    let _ = self.replies.send(Event::BreakFinished {
                        node: self.node,
                        epoch: self.epoch,
                        transaction,
                    });
                }
                result
            }
            Command::Frame { crtc, packet } => self.render(crtc, packet),
            Command::Blank(blanked) => {
                self.blanked = blanked;
                let result = if blanked {
                    self.surfaces.iter_mut().try_for_each(|(crtc, surface)| {
                        surface.clear(self.node, self.epoch, *crtc, &self.replies)?;
                        Ok(())
                    })
                } else {
                    for surface in self.surfaces.values_mut() {
                        surface.compositor.reset_buffers();
                    }
                    Ok(())
                };
                self.snapshot(false);
                result
            }
            Command::Pause(epoch) => {
                self.epoch = epoch;
                self.active = false;
                self.device.active = false;
                self.device.drm_output_manager.pause();
                for surface in self.surfaces.values_mut() {
                    surface.pending = false;
                    surface.rearmed = now_cutoff();
                }
                self.snapshot(false);
                Ok(())
            }
            Command::Resume(epoch) => {
                self.epoch = epoch;
                let activation = self
                    .device
                    .drm_output_manager
                    .lock()
                    .activate(false)
                    .map_err(|error| error.to_string());
                activation.and_then(|()| {
                    self.active = true;
                    self.device.active = true;
                    for surface in self.surfaces.values() {
                        native_outputs::apply_eight_bit(&self.device, surface.connector)
                            .map_err(|error| error.to_string())?;
                    }
                    for (crtc, surface) in &mut self.surfaces {
                        surface.clear(self.node, self.epoch, *crtc, &self.replies)?;
                        surface
                            .compositor
                            .reset_state()
                            .map_err(|error| error.to_string())?;
                        surface.compositor.reset_buffers();
                        surface.rearmed = now_cutoff();
                        surface.readback_pending = true;
                    }
                    self.scan(handle, signal)
                })
            }
            Command::Shutdown => {
                signal.stop();
                Ok(())
            }
        };
        if let Err(error) = result {
            self.fail(error);
        }
    }
}
fn now_cutoff() -> crate::flip_recovery::RearmedAt {
    let clock = smithay::utils::Clock::<smithay::utils::Monotonic>::new();
    crate::flip_recovery::RearmedAt {
        monotonic: clock.now().into(),
        realtime: std::time::SystemTime::now(),
    }
}

pub(super) fn run(
    node: DrmNode,
    epoch: native_policy::Epoch,
    fd: OwnedFd,
    commands: channel::Channel<Command>,
    replies: channel::Sender<Event>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut event_loop: EventLoop<Worker> =
        EventLoop::try_new().map_err(|error| error.to_string())?;
    let fd = DrmDeviceFd::new(smithay::utils::DeviceFd::from(fd));
    // Preserve existing scanout until a successfully imported frame is ready.
    let (drm, notifier) = DrmDevice::new(fd.clone(), false).map_err(|error| error.to_string())?;
    let gbm = GbmDevice::new(fd).map_err(|error| error.to_string())?;
    #[allow(unsafe_code)]
    // SAFETY: the worker retains its GBM device until its EGL display/renderer is dropped.
    let display = unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|error| error.to_string())?;
    if !EGLFence::supports_importing(&display) {
        return Err("Unsupported cross-GPU transport: secondary EGL lacks EGL_ANDROID_native_fence_sync; no untimed/CPU fallback admitted".into());
    }
    let render_node = EGLDevice::device_for_display(&display)
        .and_then(|device| device.try_get_render_node())
        .map_err(|error| error.to_string())?
        .unwrap_or(node);
    let context = EGLContext::new(&display).map_err(|error| error.to_string())?;
    #[allow(unsafe_code)]
    // SAFETY: this context and renderer are created, used and dropped exclusively on this thread.
    let renderer = unsafe { GlesRenderer::new(context) }.map_err(|error| error.to_string())?;
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into());
    let manager = DrmOutputManager::new(
        drm,
        allocator.clone(),
        exporter.clone(),
        Some(gbm.clone()),
        CROSS_GPU_COLOR_FORMATS.iter().copied(),
        renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied(),
    );
    let token = event_loop
        .handle()
        .insert_source(
            notifier,
            |event, metadata, worker: &mut Worker| match event {
                DrmEvent::VBlank(crtc) => {
                    worker.presented(crtc, metadata.as_ref().map(|event| event.time))
                }
                DrmEvent::Error(error) => {
                    worker.fail(format!("Actual secondary DRM device event error: {error}"))
                }
            },
        )
        .map_err(|error| error.to_string())?;
    let gpu_id = native_outputs::gpu_identity(node).map_err(|error| error.to_string())?;
    let device = DeviceData {
        drm_output_manager: manager,
        allocator: allocator.clone(),
        exporter: exporter.clone(),
        gbm: gbm.clone(),
        render_node,
        gpu_id,
        catalog: HashMap::new(),
        active: true,
        activation_failure: None,
        connector_failures: HashMap::new(),
        connector_scan_known: false,
        surfaces: HashMap::new(),
        scanner: DrmScanner::new(),
        registration_token: token,
    };
    let mut worker = Worker {
        node,
        epoch,
        device,
        allocator,
        exporter,
        gbm,
        renderer,
        surfaces: HashMap::new(),
        replies,
        active: true,
        blanked: false,
        transaction: None,
        late_edid_done: false,
        late_edid_pending: false,
    };
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();
    let handle_for_commands = handle.clone();
    let signal_for_commands = signal.clone();
    let command_cancel = cancel.clone();
    handle
        .insert_source(
            commands,
            move |event, (), worker: &mut Worker| match event {
                channel::Event::Msg(command) => {
                    if command_cancel.load(Ordering::Acquire) {
                        signal_for_commands.stop();
                    } else {
                        worker.command(command, &handle_for_commands, &signal_for_commands);
                    }
                }
                channel::Event::Closed => signal_for_commands.stop(),
            },
        )
        .map_err(|error| error.to_string())?;
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    worker.scan(&handle, &signal)?;
    event_loop
        .run(Some(Duration::from_secs(1)), &mut worker, move |_| {
            if cancel.load(Ordering::Acquire) {
                signal.stop();
            }
        })
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_status_with_edid_or_live_route_keeps_owned_scan_admissible() {
        for (edid, live_route) in [(true, false), (false, true)] {
            let connected = native_outputs::observed_presence(false, edid, live_route);
            assert_eq!(
                scan_action([(connected, true)], false, false),
                ScanAction::Ready
            );
        }
    }

    #[test]
    fn absent_evidence_stays_lazy_even_with_stale_advertised_timings() {
        let connected = native_outputs::observed_presence(false, false, false);
        assert_eq!(
            scan_action([(connected, true)], false, false),
            ScanAction::NoSink
        );
        assert_eq!(scan_action([], false, false), ScanAction::NoSink);
        assert_eq!(
            scan_action([(connected, false)], true, true),
            ScanAction::NoSink
        );
    }

    #[test]
    fn alternate_presence_without_timings_gets_exactly_one_bounded_reprobe() {
        for (edid, live_route) in [(true, false), (false, true)] {
            let connected = native_outputs::observed_presence(false, edid, live_route);
            assert_eq!(
                scan_action([(connected, false)], false, false),
                ScanAction::Reprobe
            );
            assert_eq!(
                scan_action([(connected, false)], true, true),
                ScanAction::Wait
            );
            assert_eq!(
                scan_action([(connected, false)], true, false),
                ScanAction::NoTimings
            );
            assert_eq!(
                scan_action([(connected, true)], true, false),
                ScanAction::Ready
            );
        }
        assert_eq!(native_policy::LATE_EDID_REPROBE, Duration::from_secs(1));
    }

    #[test]
    fn one_timeless_connector_does_not_reprobe_forever_or_stop_a_usable_sink() {
        let connectors = [(true, true), (true, false)];
        assert_eq!(scan_action(connectors, false, false), ScanAction::Reprobe);
        assert_eq!(scan_action(connectors, true, true), ScanAction::Ready);
        assert_eq!(scan_action(connectors, true, false), ScanAction::Ready);
    }
}
