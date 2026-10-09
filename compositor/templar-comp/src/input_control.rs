//! The event-loop-owned libinput configuration authority. Device objects never cross threads.
use smithay::reexports::input::{self, AsRaw, Device};
use std::collections::BTreeMap;
use templar_types::{
    input::{
        self as policy, AccelerationProfile, ClickMethod, InputCapabilities, InputConfiguration,
        InputDevice, InputError, InputId, InputObservation, ScrollMethod,
    },
    settings::{Capability, Revision},
};

#[derive(Debug)]
pub struct InputController {
    revision: Revision,
    overrides: BTreeMap<InputId, InputConfiguration>,
    identities: BTreeMap<String, InputId>,
}
impl Default for InputController {
    fn default() -> Self {
        Self {
            revision: Revision::new_owner(),
            overrides: BTreeMap::new(),
            identities: BTreeMap::new(),
        }
    }
}
impl InputController {
    pub fn changed(&mut self) {
        self.revision.generation = self
            .revision
            .generation
            .checked_add(1)
            .expect("input generation exhausted");
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    fn identity(&self, device: &Device) -> &InputId {
        self.identities
            .get(device.sysname())
            .expect("tracked libinput device has an event-loop-owned identity")
    }
    pub fn removed(&mut self, sysname: &str) {
        if let Some(id) = self.identities.remove(sysname)
            && id.0.starts_with("temporary:")
        {
            self.overrides.remove(&id);
        }
        self.changed();
    }
    pub fn observation(
        &self,
        devices: &[Device],
        session_active: bool,
        locked: bool,
    ) -> InputObservation {
        let mut observation = InputObservation {
            revision: self.revision,
            devices: devices
                .iter()
                .map(|device| observe_device(device, self.identity(device).clone()))
                .collect(),
            session_active,
            locked,
        };
        for index in 0..observation.devices.len() {
            if observation
                .devices
                .iter()
                .filter(|device| device.id == observation.devices[index].id)
                .count()
                > 1
            {
                let reason = "Multiple present devices expose the same hardware identity; no unique control or persistence target exists";
                observation.devices[index].configuration = Capability::Unavailable {
                    reason: reason.into(),
                };
                observation.devices[index].persistence = Capability::Unavailable {
                    reason: reason.into(),
                };
            }
        }
        observation
    }
    pub fn added(&mut self, device: &mut Device) {
        let id = hardware_identity(device).unwrap_or_else(|| {
            InputId(format!(
                "temporary:{}/{}/{}",
                self.revision.owner_epoch,
                self.revision.generation,
                device.sysname()
            ))
        });
        self.identities.insert(device.sysname().into(), id);
        if let Some(configuration) = self.overrides.get(self.identity(device)) {
            let before = observe_device(device, self.identity(device).clone());
            if let Err(error) = policy::validate_configuration(&before, configuration)
                .and_then(|()| apply_checked(device, configuration))
            {
                let rollback = apply_checked(device, &before.effective);
                tracing::warn!(%error, rollback = ?rollback, device = %device.name(), "input override failed on hotplug");
            }
        }
        self.changed();
    }
    pub fn configure(
        &mut self,
        devices: &mut [Device],
        expected: Revision,
        id: &InputId,
        configuration: &InputConfiguration,
        session_active: bool,
        locked: bool,
    ) -> Result<InputObservation, InputError> {
        self.change(
            devices,
            expected,
            id,
            Some(configuration),
            session_active,
            locked,
        )
    }
    pub fn reset(
        &mut self,
        devices: &mut [Device],
        expected: Revision,
        id: &InputId,
        session_active: bool,
        locked: bool,
    ) -> Result<InputObservation, InputError> {
        self.change(devices, expected, id, None, session_active, locked)
    }
    fn change(
        &mut self,
        devices: &mut [Device],
        expected: Revision,
        id: &InputId,
        requested: Option<&InputConfiguration>,
        session_active: bool,
        locked: bool,
    ) -> Result<InputObservation, InputError> {
        if expected != self.revision {
            return Err(InputError::new(
                "stale_revision",
                "The compositor owner or device inventory changed",
            ));
        }
        if !session_active {
            return Err(InputError::new(
                "session_inactive",
                "Input configuration requires an active session",
            ));
        }
        if locked {
            return Err(InputError::new(
                "session_locked",
                "Input configuration is unavailable while the session is locked",
            ));
        }
        let mut matches = devices
            .iter()
            .enumerate()
            .filter_map(|(index, device)| (self.identity(device) == id).then_some(index));
        let index = matches.next().ok_or_else(|| {
            InputError::new("device_removed", "The input device is no longer present")
        })?;
        if matches.next().is_some() {
            return Err(InputError::new(
                "ambiguous_identity",
                "Multiple input devices expose this identity; no device was modified",
            ));
        }
        let device = &mut devices[index];
        let before = observe_device(device, id.clone());
        if requested.is_none() && !matches!(before.default_restore, Capability::Available { .. }) {
            return Err(InputError::new(
                "unsupported_default",
                "libinput exposes an unrecognized native default; it cannot be restored exactly",
            ));
        }
        let configuration = requested.unwrap_or(&before.defaults);
        policy::validate_configuration(&before, configuration)?;
        if requested.is_some()
            && configuration.scroll_method == Some(ScrollMethod::OnButtonDown)
            && configuration.scroll_button.is_none()
            && before.effective.scroll_button == Some(0)
        {
            return Err(InputError::new(
                "missing_scroll_button",
                "Select a nonzero scroll-trigger button before enabling button scrolling",
            ));
        }
        if let Err(error) = apply_checked(device, configuration) {
            let rollback = apply_checked(device, &before.effective);
            self.changed();
            return Err(InputError::new(
                "apply_failed",
                format!(
                    "{error}; rollback: {}",
                    rollback.map_or_else(
                        |error| format!("failed: {error}"),
                        |()| "restored with matching native readback".into()
                    )
                ),
            ));
        }
        if requested.is_some() {
            policy::merge_configuration(
                self.overrides.entry(id.clone()).or_default(),
                configuration,
            );
        } else {
            self.overrides.remove(id);
        }
        self.changed();
        Ok(self.observation(devices, session_active, locked))
    }
}

fn available(value: bool, property: &str) -> Capability {
    if value {
        Capability::Available {
            requires_restart: false,
        }
    } else {
        Capability::Unsupported {
            reason: format!("libinput does not advertise {property} for this device"),
        }
    }
}

#[allow(unsafe_code)]
fn hardware_identity(device: &Device) -> Option<InputId> {
    // SAFETY: The udev reference remains on the libinput-owning event-loop thread.
    let udev = unsafe { device.udev_device() };
    let discriminator = capability_bits(device);
    if let Some(path) = udev
        .as_ref()
        .and_then(|device| device.property_value("ID_PATH"))
        .filter(|path| !path.is_empty())
    {
        return Some(InputId(format!(
            "udev-path:{}/{:04x}:{:04x}/{discriminator:02x}/{}",
            path.to_string_lossy(),
            device.id_vendor(),
            device.id_product(),
            device.name()
        )));
    }
    if let Some(serial) = udev
        .as_ref()
        .and_then(|device| device.property_value("ID_SERIAL_SHORT"))
        .filter(|serial| !serial.is_empty())
    {
        return Some(InputId(format!(
            "udev-serial:{}/{:04x}:{:04x}/{discriminator:02x}/{}",
            serial.to_string_lossy(),
            device.id_vendor(),
            device.id_product(),
            device.name()
        )));
    }
    // eventN and HID/input descendants are not persistent hardware identities.
    None
}

fn capability_bits(device: &Device) -> u8 {
    let mut bits = 0;
    for (index, capability) in [
        input::DeviceCapability::Keyboard,
        input::DeviceCapability::Pointer,
        input::DeviceCapability::Touch,
        input::DeviceCapability::TabletTool,
        input::DeviceCapability::TabletPad,
        input::DeviceCapability::Gesture,
        input::DeviceCapability::Switch,
    ]
    .into_iter()
    .enumerate()
    {
        if device.has_capability(capability) {
            bits |= 1 << index;
        }
    }
    bits
}

fn accel(profile: input_sys::libinput_config_accel_profile) -> Option<AccelerationProfile> {
    match profile {
        input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_ADAPTIVE => {
            Some(AccelerationProfile::Adaptive)
        }
        input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_FLAT => {
            Some(AccelerationProfile::Flat)
        }
        input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_CUSTOM => {
            Some(AccelerationProfile::Custom)
        }
        _ => None,
    }
}
fn click(method: input_sys::libinput_config_click_method) -> Option<ClickMethod> {
    match method {
        input_sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_BUTTON_AREAS => {
            Some(ClickMethod::ButtonAreas)
        }
        input_sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_CLICKFINGER => {
            Some(ClickMethod::ClickFinger)
        }
        input_sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_NONE => {
            Some(ClickMethod::None)
        }
        _ => None,
    }
}
fn scroll(method: input::ScrollMethod) -> Option<ScrollMethod> {
    match method {
        input::ScrollMethod::TwoFinger => Some(ScrollMethod::TwoFinger),
        input::ScrollMethod::Edge => Some(ScrollMethod::Edge),
        input::ScrollMethod::OnButtonDown => Some(ScrollMethod::OnButtonDown),
        input::ScrollMethod::NoScroll => Some(ScrollMethod::None),
        _ => None,
    }
}

#[allow(unsafe_code)]
fn observe_device(device: &Device, id: InputId) -> InputDevice {
    let speed = device.config_accel_is_available();
    let natural = device.config_scroll_has_natural_scroll();
    let handed = device.config_left_handed_is_available();
    let tap = device.config_tap_finger_count() > 0;
    let dwt = device.config_dwt_is_available();
    // SAFETY: This metadata reference stays on the same libinput-owning event-loop thread.
    let touchpad = unsafe { device.udev_device() }
        .as_ref()
        .is_some_and(|udev| {
            udev.property_value("ID_INPUT_TOUCHPAD")
                .is_some_and(|value| value == "1")
        });
    // SAFETY: All getters use the live Device on its owning event-loop thread; no raw pointer
    // escapes. The Rust wrapper omits click NONE and pre-1.23 Custom profile readback.
    let (profiles, default_profile, current_profile, default_click, current_click) = unsafe {
        let raw = device.as_raw_mut();
        (
            input_sys::libinput_device_config_accel_get_profiles(raw),
            input_sys::libinput_device_config_accel_get_default_profile(raw),
            input_sys::libinput_device_config_accel_get_profile(raw),
            input_sys::libinput_device_config_click_get_default_method(raw),
            input_sys::libinput_device_config_click_get_method(raw),
        )
    };
    let acceleration_profiles = [
        (
            input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_ADAPTIVE,
            AccelerationProfile::Adaptive,
        ),
        (
            input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_FLAT,
            AccelerationProfile::Flat,
        ),
        (
            input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_CUSTOM,
            AccelerationProfile::Custom,
        ),
    ]
    .into_iter()
    .filter_map(|(flag, profile)| (profiles & flag != 0).then_some(profile))
    .collect();
    let mut click_methods: Vec<_> = device
        .config_click_methods()
        .into_iter()
        .filter_map(|method| match method {
            input::ClickMethod::ButtonAreas => Some(ClickMethod::ButtonAreas),
            input::ClickMethod::Clickfinger => Some(ClickMethod::ClickFinger),
            _ => None,
        })
        .collect();
    let clickable = !click_methods.is_empty();
    if clickable {
        click_methods.push(ClickMethod::None);
    }
    let mut scroll_methods: Vec<_> = device
        .config_scroll_methods()
        .into_iter()
        .filter_map(scroll)
        .collect();
    if !scroll_methods.is_empty() && !scroll_methods.contains(&ScrollMethod::None) {
        scroll_methods.push(ScrollMethod::None);
    }
    let scrollable = !scroll_methods.is_empty();
    let button_scroll = scroll_methods.contains(&ScrollMethod::OnButtonDown);
    let capabilities = InputCapabilities {
        speed: available(speed, "pointer acceleration"),
        acceleration_profiles,
        natural_scroll: available(natural, "natural scrolling"),
        left_handed: available(handed, "primary-button swap"),
        tap: available(tap, "tap-to-click"),
        tap_drag: available(tap, "tap drag"),
        tap_drag_lock: available(tap, "tap drag lock"),
        click_methods,
        scroll_methods,
        scroll_button: available(button_scroll, "button-scroll trigger"),
        disable_while_typing: available(dwt, "disable-while-typing"),
    };
    let defaults = InputConfiguration {
        speed: speed.then(|| device.config_accel_default_speed()),
        acceleration_profile: accel(default_profile),
        natural_scroll: natural.then(|| device.config_scroll_default_natural_scroll_enabled()),
        left_handed: handed.then(|| device.config_left_handed_default()),
        tap: tap.then(|| device.config_tap_default_enabled()),
        tap_drag: tap.then(|| device.config_tap_default_drag_enabled()),
        tap_drag_lock: tap.then(|| {
            !matches!(
                device.config_tap_default_drag_lock_enabled(),
                input::DragLockState::Disabled
            )
        }),
        click_method: clickable.then(|| click(default_click)).flatten(),
        scroll_method: scrollable
            .then(|| device.config_scroll_default_method().and_then(scroll))
            .flatten(),
        scroll_button: button_scroll.then(|| device.config_scroll_default_button()),
        disable_while_typing: dwt.then(|| device.config_dwt_default_enabled()),
    };
    let effective = InputConfiguration {
        speed: speed.then(|| device.config_accel_speed()),
        acceleration_profile: accel(current_profile),
        natural_scroll: natural.then(|| device.config_scroll_natural_scroll_enabled()),
        left_handed: handed.then(|| device.config_left_handed()),
        tap: tap.then(|| device.config_tap_enabled()),
        tap_drag: tap.then(|| device.config_tap_drag_enabled()),
        tap_drag_lock: tap.then(|| device.config_tap_drag_lock_enabled()),
        click_method: clickable.then(|| click(current_click)).flatten(),
        scroll_method: scrollable
            .then(|| device.config_scroll_method().and_then(scroll))
            .flatten(),
        scroll_button: button_scroll.then(|| device.config_scroll_button()),
        disable_while_typing: dwt.then(|| device.config_dwt_enabled()),
    };
    let persistence = if id.0.starts_with("temporary:") {
        Capability::Unavailable { reason: "udev exposes neither a stable physical path nor serial; this temporary identity and its overrides end on device removal or compositor restart".into() }
    } else {
        Capability::Available {
            requires_restart: false,
        }
    };
    let known_current = (current_profile
        == input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_NONE
        || effective.acceleration_profile.is_some())
        && (!clickable || effective.click_method.is_some())
        && (!scrollable || effective.scroll_method.is_some());
    let known_defaults = (default_profile
        == input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_NONE
        || defaults.acceleration_profile.is_some())
        && (!clickable || defaults.click_method.is_some())
        && (!scrollable || defaults.scroll_method.is_some());
    let managed = effective != InputConfiguration::default();
    let default_restore = if !managed {
        Capability::Unsupported {
            reason: "libinput advertises no managed pointer/touchpad configuration for this device"
                .into(),
        }
    } else if known_current && known_defaults {
        Capability::Available {
            requires_restart: false,
        }
    } else {
        Capability::Unavailable { reason: "libinput exposes an unrecognized current/default configuration; exact restoration and rollback are unavailable".into() }
    };
    let configuration = if !managed {
        Capability::Unsupported {
            reason: "libinput advertises no managed pointer/touchpad configuration for this device"
                .into(),
        }
    } else if known_current {
        Capability::Available {
            requires_restart: false,
        }
    } else {
        Capability::Unavailable { reason: "libinput exposes an unrecognized current configuration; setters cannot guarantee exact rollback".into() }
    };
    InputDevice {
        id,
        name: device.name().into_owned(),
        kernel_name: device.sysname().into(),
        touchpad,
        persistence,
        default_restore,
        configuration,
        capabilities,
        defaults,
        effective,
    }
}

#[allow(unsafe_code)]
fn apply(device: &mut Device, configuration: &InputConfiguration) -> Result<(), InputError> {
    macro_rules! set {
        ($field:ident, $method:ident) => {
            if let Some(value) = configuration.$field {
                device.$method(value).map_err(|error| {
                    InputError::new(
                        "libinput_rejected",
                        format!("{}: {error:?}", stringify!($field)),
                    )
                })?;
            }
        };
    }
    set!(speed, config_accel_set_speed);
    set!(natural_scroll, config_scroll_set_natural_scroll_enabled);
    set!(left_handed, config_left_handed_set);
    set!(tap, config_tap_set_enabled);
    set!(tap_drag, config_tap_set_drag_enabled);
    set!(disable_while_typing, config_dwt_set_enabled);
    if let Some(value) = configuration.tap_drag_lock {
        device
            .config_tap_set_drag_lock_enabled(if value {
                input::DragLockState::EnabledTimeout
            } else {
                input::DragLockState::Disabled
            })
            .map_err(|error| {
                InputError::new("libinput_rejected", format!("tap drag lock: {error:?}"))
            })?;
    }
    if let Some(value) = configuration.acceleration_profile {
        let profile = match value {
            AccelerationProfile::Adaptive => {
                input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_ADAPTIVE
            }
            AccelerationProfile::Flat => {
                input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_FLAT
            }
            AccelerationProfile::Custom => {
                input_sys::libinput_config_accel_profile_LIBINPUT_CONFIG_ACCEL_PROFILE_CUSTOM
            }
        };
        // SAFETY: Device is live and exclusively borrowed on its owning event-loop thread.
        let status = unsafe {
            input_sys::libinput_device_config_accel_set_profile(device.as_raw_mut(), profile)
        };
        raw_status(status, "acceleration profile")?;
    }
    if let Some(value) = configuration.click_method {
        let method = match value {
            ClickMethod::ButtonAreas => {
                input_sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_BUTTON_AREAS
            }
            ClickMethod::ClickFinger => {
                input_sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_CLICKFINGER
            }
            ClickMethod::None => {
                input_sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_NONE
            }
        };
        // SAFETY: Same owning-thread, exclusive Device borrow; NONE is an actual libinput mode.
        let status = unsafe {
            input_sys::libinput_device_config_click_set_method(device.as_raw_mut(), method)
        };
        raw_status(status, "click method")?;
    }
    set!(scroll_button, config_scroll_set_button);
    if let Some(value) = configuration.scroll_method {
        device
            .config_scroll_set_method(match value {
                ScrollMethod::TwoFinger => input::ScrollMethod::TwoFinger,
                ScrollMethod::Edge => input::ScrollMethod::Edge,
                ScrollMethod::OnButtonDown => input::ScrollMethod::OnButtonDown,
                ScrollMethod::None => input::ScrollMethod::NoScroll,
            })
            .map_err(|error| {
                InputError::new("libinput_rejected", format!("scroll method: {error:?}"))
            })?;
    }
    Ok(())
}
fn raw_status(status: input_sys::libinput_config_status, property: &str) -> Result<(), InputError> {
    if status == input_sys::libinput_config_status_LIBINPUT_CONFIG_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(InputError::new(
            "libinput_rejected",
            format!("{property}: native status {status}"),
        ))
    }
}
#[allow(unsafe_code)]
fn apply_checked(
    device: &mut Device,
    configuration: &InputConfiguration,
) -> Result<(), InputError> {
    apply(device, configuration)?;
    // Read only requested primitive properties. Building a complete inventory here would copy
    // device names/identities and allocate capability vectors merely to check setter readback.
    let actual = InputConfiguration {
        speed: configuration.speed.map(|_| device.config_accel_speed()),
        // SAFETY: Both getters borrow the live Device only on its owning event-loop thread.
        acceleration_profile: configuration.acceleration_profile.and_then(|_| {
            accel(unsafe {
                input_sys::libinput_device_config_accel_get_profile(device.as_raw_mut())
            })
        }),
        natural_scroll: configuration
            .natural_scroll
            .map(|_| device.config_scroll_natural_scroll_enabled()),
        left_handed: configuration
            .left_handed
            .map(|_| device.config_left_handed()),
        tap: configuration.tap.map(|_| device.config_tap_enabled()),
        tap_drag: configuration
            .tap_drag
            .map(|_| device.config_tap_drag_enabled()),
        tap_drag_lock: configuration
            .tap_drag_lock
            .map(|_| device.config_tap_drag_lock_enabled()),
        click_method: configuration.click_method.and_then(|_| {
            click(unsafe {
                input_sys::libinput_device_config_click_get_method(device.as_raw_mut())
            })
        }),
        scroll_method: configuration
            .scroll_method
            .and_then(|_| device.config_scroll_method().and_then(scroll)),
        scroll_button: configuration
            .scroll_button
            .map(|_| device.config_scroll_button()),
        disable_while_typing: configuration
            .disable_while_typing
            .map(|_| device.config_dwt_enabled()),
    };
    if policy::configuration_matches(configuration, &actual) {
        Ok(())
    } else {
        Err(InputError::new(
            "readback_mismatch",
            "libinput accepted a setter but native readback does not match every requested property",
        ))
    }
}
