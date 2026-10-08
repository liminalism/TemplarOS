//! Native connector inventory, exact timing identity, real KMS TEST_ONLY and readback.
use super::*;
use sha2::{Digest, Sha256};
use smithay::reexports::drm::control::{
    self, AtomicCommitFlags, Device as _, ResourceHandle, atomic::AtomicModeReq,
    dumbbuffer::DumbBuffer, framebuffer, property,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use templar_types::{
    display::{
        Display, DisplayError, DisplayId, DisplayMode, DisplayProfile, Insets, OutputConfiguration,
        PhysicalState, Rotation,
    },
    settings::Capability,
};

const MAX_EDID_BYTES: usize = 32768;

fn edid_digest(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty() && bytes.len() <= MAX_EDID_BYTES)
        .then(|| format!("{:x}", Sha256::digest(bytes)))
}

/// Presence admits observation/configuration, not a mode or proof of presentation.
pub(super) fn observed_presence(connected: bool, has_edid: bool, live_route: bool) -> bool {
    connected || has_edid || live_route
}

/// Metadata-only failure is not a fake scanout surface or a claim that the GPU was darkened.
pub(super) struct FailedDevice {
    pub path: std::path::PathBuf,
    pub gpu: String,
    pub reason: String,
    pub connectors_known: bool,
    pub catalog: Vec<Display>,
}
impl FailedDevice {
    pub fn observe(node: DrmNode, path: &std::path::Path, reason: String) -> Self {
        let stable_gpu = gpu_identity(node).ok();
        let gpu = stable_gpu
            .clone()
            .unwrap_or_else(|| path.display().to_string());
        let mut failed = Self {
            path: path.to_owned(),
            gpu: gpu.clone(),
            reason,
            connectors_known: true,
            catalog: Vec::new(),
        };
        let Some(card) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| {
                name.strip_prefix("card")
                    .is_some_and(|number| number.parse::<u32>().is_ok())
            })
        else {
            failed.connectors_known = false;
            return failed;
        };
        let prefix = format!("{card}-");
        let entries = match std::fs::read_dir("/sys/class/drm") {
            Ok(entries) => entries,
            Err(error) => {
                failed.connectors_known = false;
                failed.reason.push_str(&format!(
                    "; kernel connector inventory unavailable: {error}"
                ));
                return failed;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    failed.connectors_known = false;
                    failed
                        .reason
                        .push_str(&format!("; kernel connector entry unavailable: {error}"));
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(connector) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
                continue;
            };
            let status = match std::fs::read_to_string(entry.path().join("status")) {
                Ok(status) => status,
                Err(error) => {
                    failed.connectors_known = false;
                    failed.reason.push_str(&format!(
                        "; {connector} connection state unavailable: {error}"
                    ));
                    continue;
                }
            };
            let connected = match status.trim() {
                "connected" => true,
                "disconnected" => false,
                _ => {
                    failed.connectors_known = false;
                    continue;
                }
            };
            let digest = std::fs::File::open(entry.path().join("edid"))
                .ok()
                .and_then(|file| {
                    let mut bytes = Vec::new();
                    file.take((MAX_EDID_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)
                        .ok()?;
                    edid_digest(&bytes)
                });
            // Dormant observation opens no DRM card. An EDID or already-enabled
            // connector is additional kernel evidence of a present sink.
            let connected = observed_presence(
                connected,
                digest.is_some(),
                std::fs::read_to_string(entry.path().join("enabled"))
                    .is_ok_and(|value| value.trim() == "enabled"),
            );
            let id = DisplayId(format!(
                "{gpu}/{connector}/{}",
                digest.as_deref().unwrap_or("no-edid")
            ));
            let persistence = if stable_gpu.is_some() && digest.is_some() {
                Capability::Available {
                    requires_restart: false,
                }
            } else {
                Capability::Unavailable {
                    reason: "The failed GPU lacks a complete stable physical-device/EDID identity"
                        .into(),
                }
            };
            let unavailable = Capability::Unavailable {
                reason: failed.reason.clone(),
            };
            failed.catalog.push(Display {
                id: id.clone(),
                gpu: gpu.clone(),
                connector: connector.into(),
                make: String::new(),
                model: String::new(),
                serial: None,
                edid_sha256: digest,
                connected,
                internal: connector.starts_with("eDP-")
                    || connector.starts_with("LVDS-")
                    || connector.starts_with("DSI-"),
                // sysfs names are rounded, not exact timings. Do not invent mode identities.
                modes: Vec::new(),
                current: OutputConfiguration {
                    id,
                    enabled: false,
                    mode: None,
                    scale: 1.0,
                    rotation: Rotation::Normal,
                    x: 0,
                    y: 0,
                    mirror_of: None,
                    primary: false,
                    insets: Insets::default(),
                },
                physical: PhysicalState::Unavailable {
                    reason: failed.reason.clone(),
                },
                persistence,
                configure: unavailable.clone(),
                hardware_underscan: unavailable.clone(),
                composition_inset: unavailable,
            });
        }
        failed
    }
    pub fn unknown_physical_outputs(&self) -> bool {
        !self.connectors_known || self.catalog.iter().any(|display| display.connected)
    }
}

pub(super) fn refresh_physical(device: &mut DeviceData) {
    if !device.active {
        refresh_catalog(device);
        return;
    }
    let drm = device.drm_output_manager.device();
    for (handle, display) in &mut device.catalog {
        if let Some(info) = device.scanner.connectors().get(handle) {
            display.physical = physical_state(drm, info);
        } else {
            display.physical = PhysicalState::Unavailable {
                reason: "The kernel connector cannot currently be read back".into(),
            };
        }
    }
}
pub(super) fn gpu_identity(node: DrmNode) -> std::io::Result<String> {
    let path = std::fs::canonicalize(format!(
        "/sys/dev/char/{}:{}/device",
        libc::major(node.dev_id()),
        libc::minor(node.dev_id())
    ))?;
    Ok(path
        .strip_prefix("/sys/devices")
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned())
}
pub(super) fn mode_id(mode: &control::Mode) -> String {
    format!(
        "{}:{}x{}:{:?}:{:?}:{}:{}:{}",
        mode.clock(),
        mode.size().0,
        mode.size().1,
        mode.hsync(),
        mode.vsync(),
        mode.hskew(),
        mode.vscan(),
        mode.flags().bits()
    )
}
pub(super) fn rotation(transform: Transform) -> Rotation {
    match transform {
        Transform::_90 => Rotation::Left,
        Transform::_180 => Rotation::Inverted,
        Transform::_270 => Rotation::Right,
        _ => Rotation::Normal,
    }
}
pub(super) fn transform(rotation: Rotation) -> Transform {
    match rotation {
        Rotation::Normal => Transform::Normal,
        Rotation::Left => Transform::_90,
        Rotation::Inverted => Transform::_180,
        Rotation::Right => Transform::_270,
    }
}
fn failure(context: &str, error: impl std::fmt::Display) -> DisplayError {
    DisplayError::new("native_backend", format!("{context}: {error}"))
}
pub(super) fn property<D: control::Device, H: ResourceHandle>(
    device: &D,
    object: H,
    name: &str,
) -> std::io::Result<(property::Handle, property::Info, u64)> {
    for (handle, value) in device.get_properties(object)? {
        let info = device.get_property(handle)?;
        if info.name().to_bytes() == name.as_bytes() {
            return Ok((handle, info, value));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("DRM object lacks {name}"),
    ))
}
fn add<D: control::Device, H: ResourceHandle>(
    device: &D,
    request: &mut AtomicModeReq,
    object: H,
    name: &str,
    value: u64,
) -> std::io::Result<()> {
    request.add_property(
        object,
        property(device, object, name)?.0,
        property::Value::Unknown(value),
    );
    Ok(())
}
fn edid(device: &impl control::Device, connector: connector::Handle) -> Option<String> {
    let (_, info, value) = property(device, connector, "EDID").ok()?;
    let blob = info.value_type().convert_value(value).as_blob()?;
    let bytes = device.get_property_blob(blob).ok()?;
    edid_digest(&bytes)
}
struct Underscan {
    mode: property::Handle,
    horizontal: property::Handle,
    vertical: property::Handle,
    on: u64,
    off: u64,
    horizontal_range: (u64, u64),
    vertical_range: (u64, u64),
}
fn underscan_off(
    device: &impl control::Device,
    connector: connector::Handle,
) -> std::io::Result<Option<(property::Handle, u64)>> {
    let (handle, info, _) = match property(device, connector, "underscan") {
        Ok(property) => property,
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return Ok(None),
        Err(error) => return Err(error),
    };
    let property::ValueType::Enum(values) = info.value_type() else {
        return Err(std::io::Error::other(
            "hardware underscan state is not an enum",
        ));
    };
    let off = values
        .values()
        .1
        .iter()
        .find(|value| value.name().to_bytes() == b"off")
        .ok_or_else(|| std::io::Error::other("hardware underscan cannot explicitly be disabled"))?
        .value();
    Ok(Some((handle, off)))
}

fn underscan(
    device: &impl control::Device,
    connector: connector::Handle,
) -> std::io::Result<Underscan> {
    let (mode, info, _) = property(device, connector, "underscan")?;
    let property::ValueType::Enum(values) = info.value_type() else {
        return Err(std::io::Error::other("underscan is not an enum"));
    };
    let on = values
        .values()
        .1
        .iter()
        .find(|value| value.name().to_bytes() == b"on")
        .ok_or_else(|| std::io::Error::other("underscan lacks on"))?
        .value();
    let off = values
        .values()
        .1
        .iter()
        .find(|value| value.name().to_bytes() == b"off")
        .ok_or_else(|| std::io::Error::other("underscan lacks off"))?
        .value();
    let (horizontal, hinfo, _) = property(device, connector, "underscan hborder")?;
    let (vertical, vinfo, _) = property(device, connector, "underscan vborder")?;
    let property::ValueType::UnsignedRange(hmin, hmax) = hinfo.value_type() else {
        return Err(std::io::Error::other("underscan hborder is not a range"));
    };
    let property::ValueType::UnsignedRange(vmin, vmax) = vinfo.value_type() else {
        return Err(std::io::Error::other("underscan vborder is not a range"));
    };
    Ok(Underscan {
        mode,
        horizontal,
        vertical,
        on,
        off,
        horizontal_range: (hmin, hmax),
        vertical_range: (vmin, vmax),
    })
}
fn hardware_inset(properties: &Underscan, insets: Insets) -> bool {
    !insets.is_zero()
        && insets.left == insets.right
        && insets.top == insets.bottom
        && (properties.horizontal_range.0..=properties.horizontal_range.1)
            .contains(&u64::from(insets.left))
        && (properties.vertical_range.0..=properties.vertical_range.1)
            .contains(&u64::from(insets.top))
}
fn add_underscan(
    request: &mut AtomicModeReq,
    connector: connector::Handle,
    properties: &Underscan,
    insets: Insets,
) -> bool {
    let hardware = hardware_inset(properties, insets);
    request.add_property(
        connector,
        properties.mode,
        property::Value::Unknown(if hardware {
            properties.on
        } else {
            properties.off
        }),
    );
    request.add_property(
        connector,
        properties.horizontal,
        property::Value::Unknown(if hardware {
            u64::from(insets.left)
        } else {
            properties.horizontal_range.0
        }),
    );
    request.add_property(
        connector,
        properties.vertical,
        property::Value::Unknown(if hardware {
            u64::from(insets.top)
        } else {
            properties.vertical_range.0
        }),
    );
    hardware
}

pub(super) fn physical_state(
    drm: &impl control::Device,
    connector: &connector::Info,
) -> PhysicalState {
    let read = || -> std::io::Result<PhysicalState> {
        let crtc = match property(drm, connector.handle(), "CRTC_ID") {
            Ok((_, _, 0)) => return Ok(PhysicalState::NativeDisabled),
            Ok((_, _, route)) => {
                let raw = u32::try_from(route)
                    .map_err(|_| std::io::Error::other("invalid connector CRTC route"))?;
                crtc::Handle::from(
                    std::num::NonZeroU32::new(raw)
                        .ok_or_else(|| std::io::Error::other("invalid zero CRTC route"))?,
                )
            }
            // Legacy drivers expose routing through the actual connector/encoder ioctl,
            // never through the last-owned compositor surface or an advertised preferred mode.
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                let Some(encoder) = connector.current_encoder() else {
                    return Ok(PhysicalState::NativeDisabled);
                };
                let Some(crtc) = drm.get_encoder(encoder)?.crtc() else {
                    return Ok(PhysicalState::NativeDisabled);
                };
                crtc
            }
            Err(error) => return Err(error),
        };
        match property(drm, crtc, "ACTIVE") {
            Ok((_, _, 0)) => return Ok(PhysicalState::NativeDisabled),
            Ok((_, _, 1)) => {}
            Ok(_) => return Err(std::io::Error::other("unrecognized CRTC active value")),
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {}
            Err(error) => return Err(error),
        }
        let info = drm.get_crtc(crtc)?;
        match info.mode() {
            Some(mode) => Ok(PhysicalState::NativeEnabled {
                mode: mode_id(&mode),
            }),
            None => Ok(PhysicalState::NativeDisabled),
        }
    };
    read().unwrap_or_else(|error| PhysicalState::Unavailable {
        reason: format!("Actual connector/CRTC readback failed: {error}"),
    })
}

pub(super) fn refresh_catalog(device: &mut DeviceData) {
    if !device.active {
        for display in device.catalog.values_mut() {
            display.physical = PhysicalState::Unavailable { reason: device.activation_failure.as_deref().unwrap_or("This seat does not currently own DRM master; the physical image cannot be attested").into() };
            display.configure = Capability::Unavailable { reason: device.activation_failure.as_deref().unwrap_or("This seat does not currently own DRM master; the physical image cannot be attested").into() };
            display.hardware_underscan = Capability::Unavailable {
                reason: "The inactive DRM device cannot apply or attest hardware underscan".into(),
            };
            display.composition_inset = Capability::Unavailable {
                reason: "The inactive DRM device cannot present a calibrated frame".into(),
            };
        }
        return;
    }
    let drm = device.drm_output_manager.device();
    device
        .catalog
        .retain(|handle, _| device.scanner.connectors().contains_key(handle));
    for info in device.scanner.connectors().values() {
        let connector_name = output_name(info.interface(), info.interface_id());
        let digest = edid(drm, info.handle());
        let id = DisplayId(format!(
            "{}/{}/{}",
            device.gpu_id,
            connector_name,
            digest.as_deref().unwrap_or("no-edid")
        ));
        let metadata = display_info::for_connector(drm, info.handle());
        let modes: Vec<_> = info
            .modes()
            .iter()
            .map(|mode| DisplayMode {
                id: mode_id(mode),
                width: u32::from(mode.size().0),
                height: u32::from(mode.size().1),
                refresh_millihz: Mode::from(*mode).refresh.max(0) as u32,
                preferred: mode.mode_type().contains(control::ModeTypeFlags::PREFERRED),
                interlaced: mode.flags().contains(control::ModeFlags::INTERLACE),
            })
            .collect();
        let mut current = device
            .catalog
            .get(&info.handle())
            .filter(|old| old.id == id)
            .map(|old| old.current.clone())
            .unwrap_or_else(|| OutputConfiguration {
                id: id.clone(),
                enabled: false,
                mode: modes
                    .iter()
                    .find(|mode| mode.preferred)
                    .or_else(|| modes.first())
                    .map(|mode| mode.id.clone()),
                scale: 1.0,
                rotation: Rotation::Normal,
                x: 0,
                y: 0,
                mirror_of: None,
                primary: false,
                insets: Insets::default(),
            });
        current.enabled = device
            .surfaces
            .values()
            .any(|surface| surface.connector == info.handle());
        let configure = if !device.active {
            Capability::Unavailable {
                reason: "This DRM device could not be activated".into(),
            }
        } else if !drm.is_atomic() {
            Capability::Unsupported {
                reason: "The driver has no atomic KMS TEST_ONLY output transaction support".into(),
            }
        } else if modes.is_empty() {
            Capability::Unavailable {
                reason: "The connected monitor has not advertised usable timings".into(),
            }
        } else {
            Capability::Available {
                requires_restart: false,
            }
        };
        let hardware_underscan = match underscan(drm, info.handle()) {
            Ok(properties) => {
                let mut request = AtomicModeReq::new();
                request.add_property(
                    info.handle(),
                    properties.mode,
                    property::Value::Unknown(properties.on),
                );
                request.add_property(
                    info.handle(),
                    properties.horizontal,
                    property::Value::Unknown(properties.horizontal_range.0),
                );
                request.add_property(
                    info.handle(),
                    properties.vertical,
                    property::Value::Unknown(properties.vertical_range.0),
                );
                match drm.atomic_commit(
                    AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
                    request,
                ) {
                    Ok(()) => Capability::Available {
                        requires_restart: false,
                    },
                    Err(error) => Capability::Unsupported {
                        reason: format!(
                            "Advertised hardware underscan failed KMS TEST_ONLY: {error}"
                        ),
                    },
                }
            }
            Err(error) => Capability::Unsupported {
                reason: format!("Hardware underscan is not usable: {error}"),
            },
        };
        let composition_inset = match underscan_off(drm, info.handle()) {
            Ok(None) => Capability::Available {
                requires_restart: false,
            },
            Ok(Some((handle, off))) => {
                let mut request = AtomicModeReq::new();
                request.add_property(info.handle(), handle, property::Value::Unknown(off));
                match drm.atomic_commit(
                    AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
                    request,
                ) {
                    Ok(()) => Capability::Available {
                        requires_restart: false,
                    },
                    Err(error) => Capability::Unavailable {
                        reason: format!(
                            "Compositor calibration cannot safely disable existing hardware underscan: {error}"
                        ),
                    },
                }
            }
            Err(error) => Capability::Unavailable {
                reason: format!(
                    "Compositor calibration cannot attest the hardware underscan-off state: {error}"
                ),
            },
        };
        let physical = physical_state(drm, info);
        let connected = observed_presence(
            info.state() == connector::State::Connected,
            digest.is_some(),
            matches!(&physical, PhysicalState::NativeEnabled { .. }),
        );
        let persistence = if digest.is_some() {
            Capability::Available {
                requires_restart: false,
            }
        } else {
            Capability::Unavailable { reason: "The connector has no readable EDID identity; reconnect cannot identify this physical monitor exactly".into() }
        };
        let configure = if let Some(reason) = device.connector_failures.get(&info.handle()) {
            Capability::Unavailable {
                reason: reason.clone(),
            }
        } else {
            configure
        };
        device.catalog.insert(
            info.handle(),
            Display {
                id,
                gpu: device.gpu_id.clone(),
                connector: connector_name.clone(),
                make: metadata
                    .as_ref()
                    .and_then(|info| info.make())
                    .unwrap_or_else(|| "Unknown".into()),
                model: metadata
                    .as_ref()
                    .and_then(|info| info.model())
                    .unwrap_or_else(|| "Unknown".into()),
                serial: metadata.as_ref().and_then(|info| info.serial()),
                edid_sha256: digest,
                connected,
                internal: connector_name.starts_with("eDP-")
                    || connector_name.starts_with("LVDS-")
                    || connector_name.starts_with("DSI-"),
                modes,
                current,
                physical,
                persistence,
                configure,
                hardware_underscan,
                composition_inset,
            },
        );
    }
}

pub(super) struct Plan<'a> {
    pub node: DrmNode,
    pub crtc: crtc::Handle,
    pub connector: connector::Handle,
    pub mode: control::Mode,
    pub configuration: &'a OutputConfiguration,
}
pub(super) fn plan<'a>(
    state: &State,
    profile: &'a DisplayProfile,
) -> Result<Vec<Plan<'a>>, DisplayError> {
    struct Candidate<'a> {
        node: DrmNode,
        connector: connector::Handle,
        mode: control::Mode,
        configuration: &'a OutputConfiguration,
        crtcs: Vec<crtc::Handle>,
    }
    let mut candidates = Vec::new();
    for configuration in profile
        .outputs
        .iter()
        .filter(|configuration| configuration.enabled)
    {
        if state.backend_data.secondary.contains(&configuration.id) {
            continue;
        }
        let (node, device, connector) = state
            .backend_data
            .devices
            .iter()
            .find_map(|(node, device)| {
                device
                    .catalog
                    .iter()
                    .find(|(_, display)| display.id == configuration.id)
                    .map(|(connector, _)| (*node, device, *connector))
            })
            .ok_or_else(|| {
                DisplayError::new("output_removed", "An output was removed during planning")
            })?;
        let info = device.scanner.connectors().get(&connector).ok_or_else(|| {
            DisplayError::new(
                "output_removed",
                "The connector is no longer in the native inventory",
            )
        })?;
        let mode = info
            .modes()
            .iter()
            .find(|mode| {
                configuration
                    .mode
                    .as_ref()
                    .is_some_and(|id| *id == mode_id(mode))
            })
            .copied()
            .ok_or_else(|| {
                DisplayError::new(
                    "mode_removed",
                    "The exact signal mode is no longer advertised",
                )
            })?;
        let drm = device.drm_output_manager.device();
        let resources = drm
            .resource_handles()
            .map_err(|error| failure("Querying CRTC resources", error))?;
        let mut crtcs = Vec::new();
        if let Some((crtc, _)) = device
            .surfaces
            .iter()
            .find(|(_, surface)| surface.connector == connector)
        {
            crtcs.push(*crtc);
        }
        for encoder in info.encoders() {
            let encoder = drm
                .get_encoder(*encoder)
                .map_err(|error| failure("Querying connector encoder", error))?;
            for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
                if !crtcs.contains(&crtc) {
                    crtcs.push(crtc);
                }
            }
        }
        candidates.push(Candidate {
            node,
            connector,
            mode,
            configuration,
            crtcs,
        });
    }
    fn assign(
        index: usize,
        candidates: &[Candidate<'_>],
        owners: &mut BTreeMap<(u64, u32), usize>,
        seen: &mut BTreeSet<(u64, u32)>,
    ) -> bool {
        for crtc in &candidates[index].crtcs {
            let key = (candidates[index].node.dev_id(), u32::from(*crtc));
            if !seen.insert(key) {
                continue;
            }
            if owners
                .get(&key)
                .copied()
                .is_none_or(|previous| assign(previous, candidates, owners, seen))
            {
                owners.insert(key, index);
                return true;
            }
        }
        false
    }
    let mut owners = BTreeMap::new();
    for index in 0..candidates.len() {
        if !assign(index, &candidates, &mut owners, &mut BTreeSet::new()) {
            return Err(DisplayError::new(
                "crtc_unavailable",
                "The requested enabled displays cannot be assigned distinct usable CRTCs",
            ));
        }
    }
    candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            let crtc = candidate
                .crtcs
                .into_iter()
                .find(|crtc| {
                    owners.get(&(candidate.node.dev_id(), u32::from(*crtc))) == Some(&index)
                })
                .ok_or_else(|| {
                    DisplayError::new("crtc_unavailable", "Native CRTC assignment was lost")
                })?;
            Ok(Plan {
                node: candidate.node,
                crtc,
                connector: candidate.connector,
                mode: candidate.mode,
                configuration: candidate.configuration,
            })
        })
        .collect()
}

struct TestResources<'a> {
    device: &'a smithay::backend::drm::DrmDevice,
    buffers: Vec<DumbBuffer>,
    framebuffers: Vec<framebuffer::Handle>,
    blobs: Vec<u64>,
}
impl Drop for TestResources<'_> {
    fn drop(&mut self) {
        for framebuffer in self.framebuffers.drain(..) {
            if let Err(error) = self.device.destroy_framebuffer(framebuffer) {
                tracing::warn!(%error, "destroying KMS test framebuffer");
            }
        }
        for buffer in self.buffers.drain(..) {
            if let Err(error) = self.device.destroy_dumb_buffer(buffer) {
                tracing::warn!(%error, "destroying KMS test buffer");
            }
        }
        for blob in self.blobs.drain(..) {
            if let Err(error) = self.device.destroy_property_blob(blob) {
                tracing::warn!(%error, "destroying KMS test mode blob");
            }
        }
    }
}
pub(super) fn test(state: &State, profile: &DisplayProfile) -> Result<(), DisplayError> {
    if !state.backend_data.secondary.checked(profile) {
        return Err(DisplayError::new(
            "worker_pending",
            "Secondary GPU kernel TEST_ONLY must complete asynchronously before admitting the profile",
        ));
    }
    let plans = plan(state, profile)?;
    test_plans(state, profile, &plans)
}
fn test_plans(
    state: &State,
    profile: &DisplayProfile,
    plans: &[Plan<'_>],
) -> Result<(), DisplayError> {
    for (node, device) in &state.backend_data.devices {
        test_device(*node, device, profile, plans, false)?;
    }
    Ok(())
}
pub(super) fn test_device(
    node: DrmNode,
    device: &DeviceData,
    profile: &DisplayProfile,
    plans: &[Plan<'_>],
    cross_gpu: bool,
) -> Result<(), DisplayError> {
    let changed = device
        .catalog
        .values()
        .filter(|display| display.connected)
        .any(|display| {
            profile
                .outputs
                .iter()
                .find(|output| output.id == display.id)
                .is_none_or(|output| output != &display.current)
        })
        || device.surfaces.values().any(|surface| {
            !profile
                .outputs
                .iter()
                .any(|output| output.id == surface.id && output.enabled)
        });
    if !changed {
        return Ok(());
    }
    if !device.active {
        return Err(DisplayError::new(
            "gpu_unavailable",
            "A participating DRM device cannot be activated; its physical image is unknown",
        ));
    }
    let drm = device.drm_output_manager.device();
    if !drm.is_atomic() {
        return Err(DisplayError::new(
            "unsupported_atomic",
            "A participating DRM device has no atomic KMS support",
        ));
    }
    let mut resources = TestResources {
        device: drm,
        buffers: Vec::new(),
        framebuffers: Vec::new(),
        blobs: Vec::new(),
    };
    let mut request = AtomicModeReq::new();
    let handles = drm
        .resource_handles()
        .map_err(|error| failure("Querying KMS resources", error))?;
    let mut seen_planes = BTreeSet::new();
    for crtc in handles.crtcs() {
        add(drm, &mut request, *crtc, "ACTIVE", 0)
            .map_err(|error| failure("Testing disabled CRTC", error))?;
        add(drm, &mut request, *crtc, "MODE_ID", 0)
            .map_err(|error| failure("Testing disabled mode", error))?;
        let planes = drm
            .planes(crtc)
            .map_err(|error| failure("Querying KMS planes", error))?;
        for plane in planes
            .primary
            .iter()
            .chain(&planes.cursor)
            .chain(&planes.overlay)
        {
            if seen_planes.insert(u32::from(plane.handle)) {
                add(drm, &mut request, plane.handle, "FB_ID", 0)
                    .map_err(|error| failure("Clearing test plane framebuffer", error))?;
                add(drm, &mut request, plane.handle, "CRTC_ID", 0)
                    .map_err(|error| failure("Clearing test plane CRTC", error))?;
            }
        }
    }
    for (connector, display) in &device.catalog {
        if !display.connected {
            continue;
        }
        let target = plans
            .iter()
            .find(|plan| plan.node == node && plan.connector == *connector);
        add(
            drm,
            &mut request,
            *connector,
            "CRTC_ID",
            target.map_or(0, |plan| u64::from(u32::from(plan.crtc))),
        )
        .map_err(|error| failure("Testing connector routing", error))?;
        if cross_gpu
            && target.is_some()
            && let Some(handle) = eight_bit_property(drm, *connector)?
        {
            request.add_property(*connector, handle, property::Value::Unknown(8));
        }
        if let Ok(properties) = underscan(drm, *connector) {
            let insets = target
                .filter(|_| matches!(display.hardware_underscan, Capability::Available { .. }))
                .map_or(Insets::default(), |plan| plan.configuration.insets);
            add_underscan(&mut request, *connector, &properties, insets);
        } else if target.is_some()
            && let Some((handle, off)) = underscan_off(drm, *connector)
                .map_err(|error| failure("Testing hardware underscan-off state", error))?
        {
            request.add_property(*connector, handle, property::Value::Unknown(off));
        }
    }
    let mut assigned_planes = BTreeSet::new();
    for plan in plans.iter().filter(|plan| plan.node == node) {
        let buffer = drm
            .create_dumb_buffer(
                (u32::from(plan.mode.size().0), u32::from(plan.mode.size().1)),
                Fourcc::Xrgb8888,
                32,
            )
            .map_err(|error| failure("Allocating real KMS test buffer", error))?;
        resources.buffers.push(buffer);
        let framebuffer = drm
            .add_framebuffer(
                resources.buffers.last().expect("inserted test buffer"),
                24,
                32,
            )
            .map_err(|error| failure("Exporting real KMS test framebuffer", error))?;
        resources.framebuffers.push(framebuffer);
        let blob = drm
            .create_property_blob(&plan.mode)
            .map_err(|error| failure("Creating exact test timing blob", error))?
            .as_blob()
            .ok_or_else(|| {
                DisplayError::new("invalid_mode_blob", "DRM did not return a mode blob")
            })?;
        resources.blobs.push(blob);
        add(drm, &mut request, plan.crtc, "MODE_ID", blob)
            .map_err(|error| failure("Testing exact timing", error))?;
        add(drm, &mut request, plan.crtc, "ACTIVE", 1)
            .map_err(|error| failure("Testing enabled CRTC", error))?;
        let planes = drm
            .planes(&plan.crtc)
            .map_err(|error| failure("Querying primary planes", error))?;
        let primary = planes
            .primary
            .iter()
            .find(|plane| !assigned_planes.contains(&u32::from(plane.handle)))
            .ok_or_else(|| {
                DisplayError::new(
                    "primary_plane_unavailable",
                    "No distinct primary plane is available",
                )
            })?;
        assigned_planes.insert(u32::from(primary.handle));
        for (name, value) in [
            ("FB_ID", u64::from(u32::from(framebuffer))),
            ("CRTC_ID", u64::from(u32::from(plan.crtc))),
            ("SRC_X", 0),
            ("SRC_Y", 0),
            ("SRC_W", u64::from(plan.mode.size().0) << 16),
            ("SRC_H", u64::from(plan.mode.size().1) << 16),
            ("CRTC_X", 0),
            ("CRTC_Y", 0),
            ("CRTC_W", u64::from(plan.mode.size().0)),
            ("CRTC_H", u64::from(plan.mode.size().1)),
        ] {
            add(drm, &mut request, primary.handle, name, value)
                .map_err(|error| failure("Testing complete primary plane", error))?;
        }
    }
    drm.atomic_commit(
        AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
        request,
    )
    .map_err(|error| {
        failure(
            "Kernel TEST_ONLY rejected the complete device configuration",
            error,
        )
    })?;
    Ok(())
}
fn eight_bit_property(
    drm: &impl control::Device,
    connector: connector::Handle,
) -> Result<Option<property::Handle>, DisplayError> {
    let (handle, info, _) = match property(drm, connector, "max bpc") {
        Ok(property) => property,
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return Ok(None),
        Err(error) => return Err(failure("Reading cross-GPU link depth", error)),
    };
    match info.value_type() {
        property::ValueType::UnsignedRange(minimum, maximum)
            if (minimum..=maximum).contains(&8) =>
        {
            Ok(Some(handle))
        }
        _ => Err(DisplayError::new(
            "unsupported_depth",
            "Secondary connector does not admit an explicit 8-bit link-depth limit",
        )),
    }
}
pub(super) fn apply_eight_bit(
    device: &DeviceData,
    connector: connector::Handle,
) -> Result<(), DisplayError> {
    let drm = device.drm_output_manager.device();
    let Some(handle) = eight_bit_property(drm, connector)? else {
        return Ok(());
    };
    let mut request = AtomicModeReq::new();
    request.add_property(connector, handle, property::Value::Unknown(8));
    drm.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
        .map_err(|error| failure("Applying secondary 8-bit link depth", error))?;
    verify_eight_bit(device, connector)
}
pub(super) fn verify_eight_bit(
    device: &DeviceData,
    connector: connector::Handle,
) -> Result<(), DisplayError> {
    let drm = device.drm_output_manager.device();
    match property(drm, connector, "max bpc") {
        Ok((_, _, 8)) => Ok(()),
        Ok(_) => Err(DisplayError::new(
            "depth_readback",
            "Secondary connector did not retain the 8-bit link-depth limit",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        Err(error) => Err(failure("Reading secondary link-depth limit", error)),
    }
}

pub(super) fn apply_underscan(
    device: &DeviceData,
    connector: connector::Handle,
    insets: Insets,
) -> Result<bool, DisplayError> {
    let drm = device.drm_output_manager.device();
    let properties = match underscan(drm, connector) {
        Ok(properties) => properties,
        Err(_) => {
            let Some((handle, off)) = underscan_off(drm, connector).map_err(|error| {
                failure(
                    "Disabling uninterpretable hardware underscan before composition",
                    error,
                )
            })?
            else {
                return Ok(false);
            };
            let mut request = AtomicModeReq::new();
            request.add_property(connector, handle, property::Value::Unknown(off));
            drm.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
                .map_err(|error| failure("Disabling actual hardware underscan", error))?;
            if property(drm, connector, "underscan")
                .map_err(|error| failure("Reading actual hardware underscan-off state", error))?
                .2
                != off
            {
                return Err(DisplayError::new(
                    "underscan_readback",
                    "The physical underscan-off state did not take effect",
                ));
            }
            return Ok(false);
        }
    };
    let mut request = AtomicModeReq::new();
    let selected =
        if device.catalog.get(&connector).is_some_and(|display| {
            matches!(display.hardware_underscan, Capability::Available { .. })
        }) {
            insets
        } else {
            Insets::default()
        };
    let hardware = add_underscan(&mut request, connector, &properties, selected);
    drm.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
        .map_err(|error| failure("Applying real hardware underscan", error))?;
    let expected = if hardware {
        properties.on
    } else {
        properties.off
    };
    if property(drm, connector, "underscan")
        .map_err(|error| failure("Reading hardware underscan", error))?
        .2
        != expected
    {
        return Err(DisplayError::new(
            "underscan_readback",
            "The hardware underscan mode did not take effect",
        ));
    }
    if hardware
        && (property(drm, connector, "underscan hborder")
            .map_err(|error| failure("Reading horizontal underscan", error))?
            .2
            != u64::from(insets.left)
            || property(drm, connector, "underscan vborder")
                .map_err(|error| failure("Reading vertical underscan", error))?
                .2
                != u64::from(insets.top))
    {
        return Err(DisplayError::new(
            "underscan_readback",
            "Hardware underscan borders differ from the requested values",
        ));
    }
    Ok(hardware)
}
pub(super) fn verify(state: &State, profile: &DisplayProfile) -> Result<(), DisplayError> {
    state.backend_data.secondary.verify(profile)?;
    for configuration in &profile.outputs {
        if state.backend_data.secondary.contains(&configuration.id) {
            continue;
        }
        let device = state
            .backend_data
            .devices
            .values()
            .find(|device| {
                device
                    .catalog
                    .values()
                    .any(|display| display.id == configuration.id)
            })
            .ok_or_else(|| {
                DisplayError::new("output_removed", "A display was unplugged before readback")
            })?;
        if !device.active {
            return Err(DisplayError::new(
                "gpu_unavailable",
                "The last-owned inactive device cannot attest its physical output state",
            ));
        }
        let (connector, display) = device
            .catalog
            .iter()
            .find(|(_, display)| display.id == configuration.id)
            .expect("located native display");
        if &display.current != configuration {
            return Err(DisplayError::new(
                "configuration_readback",
                "The actual compositor mode/layout/mirror/primary configuration differs from the requested target",
            ));
        }
        let drm = device.drm_output_manager.device();
        let route = property(drm, *connector, "CRTC_ID")
            .map_err(|error| failure("Reading actual connector routing", error))?
            .2;
        let surface = device
            .surfaces
            .values()
            .find(|surface| surface.id == configuration.id);
        if configuration.enabled {
            let surface = surface.ok_or_else(|| {
                DisplayError::new(
                    "readback_disabled",
                    "The requested enabled output has no scanout surface",
                )
            })?;
            let crtc = surface.drm_output.crtc();
            if route != u64::from(u32::from(crtc))
                || property(drm, crtc, "ACTIVE")
                    .map_err(|error| failure("Reading active CRTC", error))?
                    .2
                    != 1
            {
                return Err(DisplayError::new(
                    "routing_readback",
                    "The physical connector is not scanning out from its requested active CRTC",
                ));
            }
            let actual = drm
                .get_crtc(crtc)
                .map_err(|error| failure("Reading actual kernel CRTC", error))?;
            let requested = device.scanner.connectors().get(connector).and_then(|info| {
                info.modes().iter().find(|mode| {
                    configuration
                        .mode
                        .as_ref()
                        .is_some_and(|id| *id == mode_id(mode))
                })
            });
            if !requested
                .zip(actual.mode().as_ref())
                .is_some_and(|(requested, actual)| native_policy::same_signal(requested, actual))
            {
                tracing::warn!(requested = ?configuration.mode, actual = ?actual.mode().as_ref().map(mode_id), "native timing readback mismatch");
                return Err(DisplayError::new(
                    "timing_readback",
                    "The kernel's structural signal timing differs from the requested size/refresh/interlace",
                ));
            }
            let location = crate::viewport::output_geometry(&state.space, &surface.output)
                .map(|geometry| geometry.loc);
            if location != Some((configuration.x, configuration.y).into())
                || surface.output.current_scale().fractional_scale() != configuration.scale
                || rotation(surface.output.current_transform()) != configuration.rotation
                || crate::viewport::insets(&surface.output) != configuration.insets
            {
                return Err(DisplayError::new(
                    "geometry_readback",
                    "The compositor's actual viewport differs from the requested geometry",
                ));
            }
            match property(drm, *connector, "underscan") {
                Ok((_, info, value)) => {
                    let property::ValueType::Enum(values) = info.value_type() else {
                        return Err(DisplayError::new(
                            "underscan_readback",
                            "The hardware underscan state is not interpretable",
                        ));
                    };
                    let expected = if crate::viewport::hardware(&surface.output) {
                        b"on".as_slice()
                    } else {
                        b"off".as_slice()
                    };
                    let expected = values
                        .values()
                        .1
                        .iter()
                        .find(|value| value.name().to_bytes() == expected)
                        .ok_or_else(|| {
                            DisplayError::new(
                                "underscan_readback",
                                "The requested hardware underscan state is not advertised",
                            )
                        })?
                        .value();
                    if value != expected {
                        return Err(DisplayError::new(
                            "underscan_readback",
                            "The actual hardware underscan state differs from the final compositor viewport",
                        ));
                    }
                    if crate::viewport::hardware(&surface.output)
                        && (property(drm, *connector, "underscan hborder")
                            .map_err(|error| failure("Reading horizontal border", error))?
                            .2
                            != u64::from(configuration.insets.left)
                            || property(drm, *connector, "underscan vborder")
                                .map_err(|error| failure("Reading vertical border", error))?
                                .2
                                != u64::from(configuration.insets.top))
                    {
                        return Err(DisplayError::new(
                            "underscan_readback",
                            "The actual hardware borders differ from the requested inset",
                        ));
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::Unsupported
                        && !crate::viewport::hardware(&surface.output) => {}
                Err(error) => {
                    return Err(failure("Reading actual hardware underscan state", error));
                }
            }
        } else {
            if surface.is_some() {
                return Err(DisplayError::new(
                    "readback_enabled",
                    "The requested disabled output is still mapped for scanout",
                ));
            }
            if route != 0 {
                let raw = u32::try_from(route).map_err(|_| {
                    DisplayError::new(
                        "routing_readback",
                        "The kernel returned an invalid CRTC handle",
                    )
                })?;
                let crtc = crtc::Handle::from(std::num::NonZeroU32::new(raw).ok_or_else(|| {
                    DisplayError::new(
                        "routing_readback",
                        "The kernel returned an invalid CRTC handle",
                    )
                })?);
                if property(drm, crtc, "ACTIVE")
                    .map_err(|error| failure("Reading disabled physical CRTC", error))?
                    .2
                    != 0
                {
                    return Err(DisplayError::new(
                        "readback_enabled",
                        "The requested disabled physical output is still active",
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn apply(state: &mut State, profile: &DisplayProfile) -> Result<(), DisplayError> {
    apply_native(state, profile, true)
}
/// Reapply only saved primary-GPU targets without queuing work behind a stuck
/// secondary owner. This never claims that the complete rollback succeeded.
pub(super) fn restore_primary(
    state: &mut State,
    profile: &DisplayProfile,
) -> Result<bool, DisplayError> {
    let outputs: Vec<_> = profile
        .outputs
        .iter()
        .filter(|target| {
            state.backend_data.devices.values().any(|device| {
                device
                    .catalog
                    .values()
                    .any(|display| display.id == target.id)
            })
        })
        .cloned()
        .collect();
    if !outputs.iter().any(|target| target.enabled) {
        return Ok(false);
    }
    apply_native(
        state,
        &DisplayProfile {
            outputs,
            allow_headless: profile.allow_headless,
        },
        false,
    )?;
    UdevBackendData::request_redraw(state);
    Ok(true)
}
fn apply_native(
    state: &mut State,
    profile: &DisplayProfile,
    secondary: bool,
) -> Result<(), DisplayError> {
    // Every participating device is TEST_ONLY-checked before the first real effect. Devices
    // commit independently; the compositor control owner rolls the complete saved profile back.
    let plans = plan(state, profile)?;
    test_plans(state, profile, &plans)?;
    state.session_lock.reset_coverage();
    let remove: Vec<_> = state
        .backend_data
        .devices
        .iter()
        .flat_map(|(node, device)| {
            device
                .surfaces
                .iter()
                .filter(|(crtc, surface)| {
                    !plans.iter().any(|plan| {
                        plan.node == *node
                            && plan.crtc == **crtc
                            && plan.connector == surface.connector
                    })
                })
                .map(|(crtc, _)| (*node, *crtc))
        })
        .collect();
    let deferred = if secondary {
        let held_remove = remove
            .iter()
            .copied()
            .filter(|(node, crtc)| {
                !plans
                    .iter()
                    .any(|plan| plan.node == *node && plan.crtc == *crtc)
            })
            .collect();
        native_worker::begin_apply(state, profile, held_remove)?
    } else {
        false
    };
    // CRTC conflicts on the SAME primary device cannot be make-before-break. Cross-device
    // removals stay owned until every target frame and worker readback has succeeded.
    for (node, crtc) in remove {
        if !deferred
            || plans
                .iter()
                .any(|plan| plan.node == node && plan.crtc == crtc)
        {
            disable_surface(state, node, crtc)?;
        }
    }
    for plan in &plans {
        let exists = state
            .backend_data
            .devices
            .get(&plan.node)
            .is_some_and(|device| device.surfaces.contains_key(&plan.crtc));
        if !exists {
            let connector = state
                .backend_data
                .devices
                .get(&plan.node)
                .and_then(|device| device.scanner.connectors().get(&plan.connector))
                .cloned()
                .ok_or_else(|| {
                    DisplayError::new("output_removed", "The native connector disappeared")
                })?;
            connector_connected(
                state,
                plan.node,
                &connector,
                plan.crtc,
                Some(plan.configuration),
            );
        }
        let backend = &mut state.backend_data;
        let device = backend
            .devices
            .get_mut(&plan.node)
            .ok_or_else(|| DisplayError::new("output_removed", "The output GPU disappeared"))?;
        let hardware = apply_underscan(device, plan.connector, plan.configuration.insets)?;
        let surface = device.surfaces.get_mut(&plan.crtc).ok_or_else(|| {
            DisplayError::new(
                "surface_failed",
                "Native output initialization failed; the backend reported the failing stage",
            )
        })?;
        if mode_id(&surface.drm_output.pending_mode()) != mode_id(&plan.mode) {
            surface
                .drm_output
                .clear()
                .map_err(|error| failure("Clearing the old signal mode before rearm", error))?;
            surface.frame_in_flight = false;
            surface.preview_for_next_frame = None;
            if let Some(flip) = &surface.flip {
                flip.cancel();
            }
            surface.rearmed_at = Some(crate::flip_recovery::RearmedAt {
                monotonic: state.clock.now().into(),
                realtime: std::time::SystemTime::now(),
            });
            surface
                .drm_output
                .use_mode(plan.mode)
                .map_err(|error| failure("Applying exact native signal timing", error))?;
        }
        surface.output.change_current_state(
            Some(Mode::from(plan.mode)),
            Some(transform(plan.configuration.rotation)),
            Some(smithay::output::Scale::Fractional(plan.configuration.scale)),
            Some((plan.configuration.x, plan.configuration.y).into()),
        );
        crate::viewport::configure(&surface.output, plan.configuration.insets, hardware);
        state.space.map_output(
            &surface.output,
            (plan.configuration.x, plan.configuration.y),
        );
        surface.drm_output.reset_buffers();
        surface.physical_readback_pending = true;
        if let Some(display) = device.catalog.get_mut(&plan.connector) {
            display.current = plan.configuration.clone();
        }
    }
    if !deferred {
        record_disabled(state, profile);
    }
    for device in state.backend_data.devices.values_mut() {
        refresh_physical(device);
    }
    crate::backend::recompute_outputs(state);
    Ok(())
}

pub(super) fn disable_surface(
    state: &mut State,
    node: DrmNode,
    crtc: crtc::Handle,
) -> Result<(), DisplayError> {
    let device = state.backend_data.devices.get_mut(&node).ok_or_else(|| {
        DisplayError::new(
            "output_removed",
            "The output device disappeared before break",
        )
    })?;
    if let Some(surface) = device.surfaces.get_mut(&crtc) {
        surface
            .drm_output
            .clear()
            .map_err(|error| failure("Disabling the actual scanout surface", error))?;
        surface.frame_in_flight = false;
        if let Some(flip) = &surface.flip {
            flip.cancel();
        }
    }
    connector_disconnected(state, node, crtc);
    Ok(())
}

pub(super) fn record_disabled(state: &mut State, profile: &DisplayProfile) {
    for device in state.backend_data.devices.values_mut() {
        for display in device.catalog.values_mut() {
            if let Some(configuration) = profile
                .outputs
                .iter()
                .find(|configuration| configuration.id == display.id && !configuration.enabled)
            {
                display.current = configuration.clone();
            }
        }
        refresh_physical(device);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_status_admits_bounded_edid_or_live_routing_not_signal_proof() {
        let digest = edid_digest(&[0, 255, 255, 255, 255, 255, 255, 0]);
        assert!(digest.is_some());
        assert!(observed_presence(false, digest.is_some(), false));
        assert!(observed_presence(false, false, true));
        assert!(observed_presence(true, false, false));
        assert!(!native_policy::physical_proof(true, None, false));
        assert!(!native_policy::physical_proof(true, Some(true), false));
    }

    #[test]
    fn absent_or_oversized_edid_is_not_presence_evidence() {
        assert_eq!(edid_digest(&[]), None);
        assert_eq!(edid_digest(&vec![0; MAX_EDID_BYTES + 1]), None);
        assert!(edid_digest(&vec![0; MAX_EDID_BYTES]).is_some());
        assert!(!observed_presence(false, false, false));
        let oversized = edid_digest(&vec![0; MAX_EDID_BYTES + 1]);
        assert!(!observed_presence(false, oversized.is_some(), false));
    }
}
