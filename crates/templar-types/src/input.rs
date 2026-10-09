//! Stable per-device libinput policy and effective readback.
use crate::settings::{Capability, Revision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputId(pub String);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccelerationProfile {
    Adaptive,
    Flat,
    Custom,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickMethod {
    ButtonAreas,
    ClickFinger,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollMethod {
    TwoFinger,
    Edge,
    OnButtonDown,
    None,
}

/// None leaves a device's effective value untouched; unsupported properties are never invented.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputConfiguration {
    /// Stored libinput speed; custom acceleration curves do not use this parameter.
    pub speed: Option<f64>,
    pub acceleration_profile: Option<AccelerationProfile>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    pub tap: Option<bool>,
    pub tap_drag: Option<bool>,
    pub tap_drag_lock: Option<bool>,
    pub click_method: Option<ClickMethod>,
    pub scroll_method: Option<ScrollMethod>,
    /// Linux input-event button code used by OnButtonDown; zero disables its trigger.
    pub scroll_button: Option<u32>,
    pub disable_while_typing: Option<bool>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputCapabilities {
    pub speed: Capability,
    pub acceleration_profiles: Vec<AccelerationProfile>,
    pub natural_scroll: Capability,
    pub left_handed: Capability,
    pub tap: Capability,
    pub tap_drag: Capability,
    pub tap_drag_lock: Capability,
    pub click_methods: Vec<ClickMethod>,
    pub scroll_methods: Vec<ScrollMethod>,
    pub scroll_button: Capability,
    pub disable_while_typing: Capability,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputDevice {
    pub id: InputId,
    pub name: String,
    pub kernel_name: String,
    pub touchpad: bool,
    /// Session-only identities may be configured, but must not create durable overrides.
    pub persistence: Capability,
    pub default_restore: Capability,
    pub configuration: Capability,
    pub capabilities: InputCapabilities,
    pub defaults: InputConfiguration,
    pub effective: InputConfiguration,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputObservation {
    pub revision: Revision,
    pub devices: Vec<InputDevice>,
    pub session_active: bool,
    pub locked: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputError {
    pub code: String,
    pub message: String,
}
impl InputError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for InputError {}

pub fn validate_configuration(
    device: &InputDevice,
    configuration: &InputConfiguration,
) -> Result<(), InputError> {
    fn supported(requested: bool, capability: &Capability, name: &str) -> Result<(), InputError> {
        if requested && !matches!(capability, Capability::Available { .. }) {
            return Err(InputError::new(
                "unsupported_property",
                format!(
                    "{} does not support {name}",
                    match capability {
                        Capability::Unsupported { reason } | Capability::Unavailable { reason } =>
                            reason.as_str(),
                        Capability::Available { .. } => unreachable!(),
                    }
                ),
            ));
        }
        Ok(())
    }
    supported(
        true,
        &device.configuration,
        "addressable device configuration",
    )?;
    if let Some(speed) = configuration.speed
        && (!speed.is_finite() || !(-1.0..=1.0).contains(&speed))
    {
        return Err(InputError::new(
            "invalid_speed",
            "Pointer speed must be finite and between -1 and 1",
        ));
    }
    let capabilities = &device.capabilities;
    supported(
        configuration.speed.is_some(),
        &capabilities.speed,
        "pointer speed",
    )?;
    supported(
        configuration.natural_scroll.is_some(),
        &capabilities.natural_scroll,
        "natural scrolling",
    )?;
    supported(
        configuration.left_handed.is_some(),
        &capabilities.left_handed,
        "primary-button swap",
    )?;
    supported(
        configuration.tap.is_some(),
        &capabilities.tap,
        "tap-to-click",
    )?;
    supported(
        configuration.tap_drag.is_some(),
        &capabilities.tap_drag,
        "tap drag",
    )?;
    supported(
        configuration.tap_drag_lock.is_some(),
        &capabilities.tap_drag_lock,
        "tap drag lock",
    )?;
    supported(
        configuration.disable_while_typing.is_some(),
        &capabilities.disable_while_typing,
        "disable-while-typing",
    )?;
    supported(
        configuration.scroll_button.is_some(),
        &capabilities.scroll_button,
        "button-scroll trigger",
    )?;
    if configuration
        .acceleration_profile
        .is_some_and(|profile| !capabilities.acceleration_profiles.contains(&profile))
    {
        return Err(InputError::new(
            "unsupported_profile",
            "The requested acceleration profile is not advertised",
        ));
    }
    if configuration
        .click_method
        .is_some_and(|method| !capabilities.click_methods.contains(&method))
    {
        return Err(InputError::new(
            "unsupported_click",
            "The requested click method is not advertised",
        ));
    }
    if configuration
        .scroll_method
        .is_some_and(|method| !capabilities.scroll_methods.contains(&method))
    {
        return Err(InputError::new(
            "unsupported_scroll",
            "The requested scroll method is not advertised",
        ));
    }
    Ok(())
}

pub fn merge_configuration(saved: &mut InputConfiguration, change: &InputConfiguration) {
    macro_rules! field {
        ($name:ident) => {
            if change.$name.is_some() {
                saved.$name = change.$name;
            }
        };
    }
    field!(speed);
    field!(acceleration_profile);
    field!(natural_scroll);
    field!(left_handed);
    field!(tap);
    field!(tap_drag);
    field!(tap_drag_lock);
    field!(click_method);
    field!(scroll_method);
    field!(scroll_button);
    field!(disable_while_typing);
}

/// Readback must match every requested property, without inventing values for omitted fields.
pub fn configuration_matches(requested: &InputConfiguration, actual: &InputConfiguration) -> bool {
    macro_rules! matches {
        ($name:ident) => {
            requested.$name.is_none() || requested.$name == actual.$name
        };
    }
    matches!(speed)
        && matches!(acceleration_profile)
        && matches!(natural_scroll)
        && matches!(left_handed)
        && matches!(tap)
        && matches!(tap_drag)
        && matches!(tap_drag_lock)
        && matches!(click_method)
        && matches!(scroll_method)
        && matches!(scroll_button)
        && matches!(disable_while_typing)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn none_is_a_no_change_policy_not_a_synthetic_hardware_default() {
        assert_eq!(InputConfiguration::default().tap, None);
        assert_eq!(InputConfiguration::default().speed, None);
    }
    #[test]
    fn explicit_false_and_zero_are_real_requested_values() {
        let change = InputConfiguration {
            tap: Some(false),
            scroll_button: Some(0),
            ..Default::default()
        };
        let mut saved = InputConfiguration {
            tap: Some(true),
            speed: Some(0.4),
            scroll_button: Some(274),
            ..Default::default()
        };
        merge_configuration(&mut saved, &change);
        assert_eq!(saved.tap, Some(false));
        assert_eq!(saved.scroll_button, Some(0));
        assert_eq!(saved.speed, Some(0.4));
        assert!(configuration_matches(&change, &saved));
        assert!(!configuration_matches(
            &change,
            &InputConfiguration::default()
        ));
    }
}
