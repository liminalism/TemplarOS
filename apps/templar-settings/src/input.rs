//! Per-device native inventory/readback. Unsupported properties are never global preferences.
use anyhow::Result;
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::{ScrollState, TextFieldState, Ui};
use std::collections::BTreeMap;
use templar_client::proxies::InputProxy;
use templar_types::{
    input::{
        AccelerationProfile, ClickMethod, InputConfiguration, InputId, InputObservation,
        ScrollMethod,
    },
    settings::{Capability, Revision},
};

#[derive(Debug, Default)]
pub struct InputState {
    pub observation: Option<InputObservation>,
    pub message: Option<String>,
}
#[derive(Debug)]
pub enum InputCommand {
    Refresh,
    Configure {
        expected: Revision,
        id: InputId,
        configuration: InputConfiguration,
    },
    Reset {
        expected: Revision,
        id: InputId,
    },
}
#[derive(Default)]
pub struct InputController {
    connection: Option<zbus::Connection>,
}
impl InputController {
    pub async fn run(&mut self, command: InputCommand) -> InputState {
        if self.connection.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => {
                    return InputState {
                        observation: None,
                        message: Some(format!("Native input broker unavailable: {error}")),
                    };
                }
            }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation: Result<()> = async {
            let proxy = InputProxy::new(connection).await?;
            match command {
                InputCommand::Refresh => {}
                InputCommand::Configure {
                    expected,
                    id,
                    configuration,
                } => {
                    proxy
                        .configure(
                            &serde_json::to_string(&expected)?,
                            &id.0,
                            &serde_json::to_string(&configuration)?,
                        )
                        .await?;
                }
                InputCommand::Reset { expected, id } => {
                    proxy
                        .reset(&serde_json::to_string(&expected)?, &id.0)
                        .await?;
                }
            }
            Ok(())
        }
        .await;
        let readback: Result<_> = async {
            Ok(serde_json::from_str(
                &InputProxy::new(connection).await?.observe().await?,
            )?)
        }
        .await;
        match readback {
            Ok(observation) => InputState { observation: Some(observation), message: operation.err().map(|error| format!("Could not configure this device: {error:#}; the values below are fresh native readback")) },
            Err(error) => InputState { observation: None, message: Some(match operation { Ok(()) => format!("Native input state unavailable: {error:#}"), Err(write) => format!("Input operation failed: {write:#}; readback unavailable: {error:#}") }) },
        }
    }
}
fn available(capability: &Capability) -> bool {
    matches!(capability, Capability::Available { .. })
}
fn reason(capability: &Capability) -> Option<&str> {
    match capability {
        Capability::Available { .. } => None,
        Capability::Unavailable { reason } | Capability::Unsupported { reason } => Some(reason),
    }
}
#[derive(Default)]
pub struct InputView {
    pub state: InputState,
    selected: Option<InputId>,
    speed: TextFieldState,
    button: TextFieldState,
    speed_drawn: bool,
    button_drawn: bool,
    scroll: ScrollState,
    heights: Vec<i32>,
    labels: BTreeMap<InputId, String>,
    actual: BTreeMap<InputId, String>,
    defaults: BTreeMap<InputId, String>,
}
impl InputView {
    pub fn replace(&mut self, state: InputState) -> bool {
        let old_selected = self.selected.clone();
        let selected_values = |observation: &Option<InputObservation>| {
            self.selected
                .as_ref()
                .and_then(|id| {
                    observation
                        .as_ref()?
                        .devices
                        .iter()
                        .find(|device| &device.id == id)
                })
                .map(|device| (device.effective.speed, device.effective.scroll_button))
        };
        let old_values = selected_values(&self.state.observation);
        let new_values = selected_values(&state.observation);
        if self.selected.as_ref().is_none_or(|id| {
            !state.observation.as_ref().is_some_and(|observation| {
                observation.devices.iter().any(|device| &device.id == id)
            })
        }) {
            self.selected = state.observation.as_ref().and_then(|observation| {
                observation.devices.first().map(|device| device.id.clone())
            });
        }
        self.labels.clear();
        self.actual.clear();
        self.defaults.clear();
        if let Some(observation) = &state.observation {
            for device in &observation.devices {
                self.labels.insert(
                    device.id.clone(),
                    format!(
                        "{} · {} · {}",
                        device.name,
                        device.kernel_name,
                        if device.touchpad {
                            "touchpad"
                        } else {
                            "input device"
                        }
                    ),
                );
                self.actual.insert(
                    device.id.clone(),
                    format!(
                        "Effective native values: {}",
                        serde_json::to_string(&device.effective)
                            .expect("finite libinput readback serializes")
                    ),
                );
                self.defaults.insert(
                    device.id.clone(),
                    format!(
                        "Captured native defaults: {}",
                        serde_json::to_string(&device.defaults)
                            .expect("finite libinput defaults serialize")
                    ),
                );
            }
        }
        self.state = state;
        if self.selected != old_selected {
            self.sync_fields();
        } else if let Some((speed, button)) = new_values {
            if old_values.map(|values| values.0) != Some(speed) {
                self.speed
                    .set_text(speed.map_or(String::new(), |speed| speed.to_string()));
            }
            if old_values.map(|values| values.1) != Some(button) {
                self.button
                    .set_text(button.map_or(String::new(), |button| button.to_string()));
            }
        }
        self.heights = self
            .state
            .observation
            .as_ref()
            .map_or_else(Vec::new, |observation| {
                observation
                    .devices
                    .iter()
                    .map(|device| {
                        if self.selected.as_ref() == Some(&device.id) {
                            1180
                        } else {
                            130
                        }
                    })
                    .collect()
            });
        true
    }
    fn sync_fields(&mut self) {
        if let Some(device) = self.selected.as_ref().and_then(|id| {
            self.state
                .observation
                .as_ref()?
                .devices
                .iter()
                .find(|device| &device.id == id)
        }) {
            self.speed.set_text(
                device
                    .effective
                    .speed
                    .map_or(String::new(), |speed| speed.to_string()),
            );
            self.button.set_text(
                device
                    .effective
                    .scroll_button
                    .map_or(String::new(), |button| button.to_string()),
            );
        }
    }
    pub fn caret_area(&self) -> Option<Rect> {
        if self.speed_drawn {
            self.speed.caret_area()
        } else {
            None
        }
        .or_else(|| {
            if self.button_drawn {
                self.button.caret_area()
            } else {
                None
            }
        })
    }
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect) -> (Option<InputCommand>, bool) {
        self.speed_drawn = false;
        self.button_drawn = false;
        let h = ui.px(30);
        let gap = ui.px(6);
        let mut y = area.y;
        let mut command = None;
        if ui.button(Rect::new(area.x, y, ui.px(120), h), "Read native") {
            command = Some(InputCommand::Refresh);
        }
        ui.label(
            Rect::new(area.x + ui.px(130), y, area.w - ui.px(130), h),
            "Properties/defaults are device-specific; no desktop environment is required",
            ui.theme.text_dim,
            Align::Left,
        );
        y += h + gap;
        if let Some(message) = &self.state.message {
            ui.label(
                Rect::new(area.x, y, area.w, h * 2),
                message,
                ui.theme.alarm,
                Align::Left,
            );
            y += h * 2 + gap;
        }
        let Some(observation) = &self.state.observation else {
            return (command, false);
        };
        let active = observation.session_active && !observation.locked;
        if !active {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                "Configuration is unavailable while the native seat is inactive or locked",
                ui.theme.text_dim,
                Align::Left,
            );
            y += h + gap;
        }
        let list_area = Rect::new(area.x, y, area.w, (area.bottom() - y).max(h));
        for (index, device) in observation.devices.iter().enumerate() {
            self.heights[index] = ui.px(if self.selected.as_ref() == Some(&device.id) {
                1180
            } else {
                130
            });
        }
        let mut selected = None;
        ui.scroll_list(&mut self.scroll, list_area, &self.heights, |ui, index, rect| {
            let device = &observation.devices[index]; let capabilities = &device.capabilities; let h = ui.px(30); let gap = ui.px(6); let mut y = rect.y;
            if ui.button(Rect::new(rect.x, y, rect.w, h), self.labels.get(&device.id).map_or(device.name.as_str(), String::as_str)) { selected = Some(device.id.clone()); } y += h + gap;
            ui.label(Rect::new(rect.x, y, rect.w, h), &device.id.0, ui.theme.text_dim, Align::Left); y += h + gap;
            if let Some(reason) = reason(&device.persistence) { ui.label(Rect::new(rect.x, y, rect.w, h), reason, ui.theme.text_dim, Align::Left); y += h + gap; }
            if self.selected.as_ref() != Some(&device.id) { return; }
            for label in [self.actual.get(&device.id), self.defaults.get(&device.id)].into_iter().flatten() { ui.label(Rect::new(rect.x, y, rect.w, h * 2), label, ui.theme.text_dim, Align::Left); y += h * 2 + gap; }
            if let Some(reason) = reason(&device.configuration) { ui.label(Rect::new(rect.x, y, rect.w, h), reason, ui.theme.alarm, Align::Left); y += h + gap; }
            let can_edit = active && available(&device.configuration);
            if can_edit && available(&capabilities.speed) && device.effective.acceleration_profile != Some(AccelerationProfile::Custom) {
                ui.text_field(&mut self.speed, Rect::new(rect.x, y, ui.px(150), h), "Speed -1 to 1", false, false); self.speed_drawn = true;
                if ui.button(Rect::new(rect.x + ui.px(160), y, ui.px(125), h), "Set speed") {
                    match self.speed.text().parse::<f64>() { Ok(speed) if speed.is_finite() && (-1.0..=1.0).contains(&speed) => command = Some(InputCommand::Configure { expected: observation.revision, id: device.id.clone(), configuration: InputConfiguration { speed: Some(speed), ..Default::default() } }), _ => self.state.message = Some("Pointer speed must be finite, between -1 and 1".into()) }
                }
            } else { ui.label(Rect::new(rect.x, y, rect.w, h), if device.effective.acceleration_profile == Some(AccelerationProfile::Custom) { "Custom acceleration uses its native curve, not the stored speed parameter" } else { reason(&capabilities.speed).unwrap_or("Speed configuration unavailable") }, ui.theme.text_dim, Align::Left); }
            y += h + gap;
            for (index, profile) in capabilities.acceleration_profiles.iter().copied().enumerate() {
                let label = match profile { AccelerationProfile::Adaptive => "Adaptive", AccelerationProfile::Flat => "Flat", AccelerationProfile::Custom => "Custom curve" };
                if can_edit && ui.button(Rect::new(rect.x + index as i32 * ui.px(140), y, ui.px(132), h), label) { command = Some(InputCommand::Configure { expected: observation.revision, id: device.id.clone(), configuration: InputConfiguration { acceleration_profile: Some(profile), ..Default::default() } }); }
            }
            if capabilities.acceleration_profiles.is_empty() { ui.label(Rect::new(rect.x, y, rect.w, h), "No acceleration profiles advertised", ui.theme.text_dim, Align::Left); } y += h + gap;
            for (label, capability, value, field) in [
                ("Natural scrolling", &capabilities.natural_scroll, device.effective.natural_scroll, 0),
                ("Swap primary button / left-handed", &capabilities.left_handed, device.effective.left_handed, 1),
                ("Tap to click", &capabilities.tap, device.effective.tap, 2),
                ("Tap drag", &capabilities.tap_drag, device.effective.tap_drag, 3),
                ("Timed tap-drag lock", &capabilities.tap_drag_lock, device.effective.tap_drag_lock, 4),
                ("Disable touchpad while typing", &capabilities.disable_while_typing, device.effective.disable_while_typing, 5),
            ] {
                if can_edit && available(capability) {
                    if let Some(mut value) = value { if ui.toggle(Rect::new(rect.x, y, rect.w, h), &mut value, label) { let mut configuration = InputConfiguration::default(); match field { 0 => configuration.natural_scroll = Some(value), 1 => configuration.left_handed = Some(value), 2 => configuration.tap = Some(value), 3 => configuration.tap_drag = Some(value), 4 => configuration.tap_drag_lock = Some(value), _ => configuration.disable_while_typing = Some(value) } command = Some(InputCommand::Configure { expected: observation.revision, id: device.id.clone(), configuration }); } }
                } else { ui.label(Rect::new(rect.x, y, rect.w, h), reason(capability).unwrap_or(label), ui.theme.text_dim, Align::Left); }
                y += h + gap;
            }
            ui.label(Rect::new(rect.x, y, rect.w, h), "Advertised software click methods", ui.theme.text_dim, Align::Left); y += h + gap;
            for (index, method) in capabilities.click_methods.iter().copied().enumerate() { if can_edit && ui.button(Rect::new(rect.x + index as i32 * ui.px(145), y, ui.px(137), h), match method { ClickMethod::ButtonAreas => "Button areas", ClickMethod::ClickFinger => "Clickfinger", ClickMethod::None => "No software click" }) { command = Some(InputCommand::Configure { expected: observation.revision, id: device.id.clone(), configuration: InputConfiguration { click_method: Some(method), ..Default::default() } }); } }
            if capabilities.click_methods.is_empty() { ui.label(Rect::new(rect.x, y, rect.w, h), "No software click methods advertised", ui.theme.text_dim, Align::Left); } y += h + gap;
            ui.label(Rect::new(rect.x, y, rect.w, h), "Advertised scroll methods", ui.theme.text_dim, Align::Left); y += h + gap;
            for (index, method) in capabilities.scroll_methods.iter().copied().enumerate() { if can_edit && ui.button(Rect::new(rect.x + index as i32 * ui.px(120), y, ui.px(112), h), match method { ScrollMethod::TwoFinger => "Two finger", ScrollMethod::Edge => "Edge", ScrollMethod::OnButtonDown => "Button held", ScrollMethod::None => "No scrolling" }) { command = Some(InputCommand::Configure { expected: observation.revision, id: device.id.clone(), configuration: InputConfiguration { scroll_method: Some(method), ..Default::default() } }); } }
            if capabilities.scroll_methods.is_empty() { ui.label(Rect::new(rect.x, y, rect.w, h), "No scroll methods advertised", ui.theme.text_dim, Align::Left); } y += h + gap;
            if can_edit && available(&capabilities.scroll_button) {
                ui.text_field(&mut self.button, Rect::new(rect.x, y, ui.px(190), h), "Linux button code", false, false); self.button_drawn = true;
                if ui.button(Rect::new(rect.x + ui.px(200), y, ui.px(150), h), "Set scroll trigger") { match self.button.text().parse::<u32>() { Ok(button) => command = Some(InputCommand::Configure { expected: observation.revision, id: device.id.clone(), configuration: InputConfiguration { scroll_button: Some(button), ..Default::default() } }), Err(_) => self.state.message = Some("Use an unsigned Linux input-event button code (274 = middle; 0 disables trigger)".into()) } }
            } else { ui.label(Rect::new(rect.x, y, rect.w, h), reason(&capabilities.scroll_button).unwrap_or("Button-scroll trigger unavailable"), ui.theme.text_dim, Align::Left); } y += h + gap;
            if can_edit && available(&device.default_restore) { if ui.button(Rect::new(rect.x, y, ui.px(230), h), "Restore native device defaults") { command = Some(InputCommand::Reset { expected: observation.revision, id: device.id.clone() }); } }
            else { ui.label(Rect::new(rect.x, y, rect.w, h), reason(&device.default_restore).unwrap_or("Native default restoration unavailable"), ui.theme.text_dim, Align::Left); }
        });
        if let Some(selected) = selected {
            self.selected = Some(selected);
            self.sync_fields();
        }
        (command, false)
    }
}
pub async fn print() -> Result<()> {
    let state = InputController::default().run(InputCommand::Refresh).await;
    if let Some(message) = state.message {
        println!("{message}");
    }
    if let Some(observation) = state.observation {
        println!("{}", serde_json::to_string_pretty(&observation)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation(speed: f64, button: u32) -> InputObservation {
        let available = Capability::Available {
            requires_restart: false,
        };
        let unsupported = Capability::Unsupported {
            reason: "Not advertised in this pure editor fixture".into(),
        };
        let configuration = InputConfiguration {
            speed: Some(speed),
            scroll_button: Some(button),
            ..Default::default()
        };
        InputObservation {
            revision: Revision::new_owner(),
            session_active: true,
            locked: false,
            devices: vec![templar_types::input::InputDevice {
                id: InputId("selected".into()),
                name: "Selected".into(),
                kernel_name: "event-fixture".into(),
                touchpad: false,
                persistence: available.clone(),
                default_restore: available.clone(),
                configuration: available.clone(),
                capabilities: templar_types::input::InputCapabilities {
                    speed: available.clone(),
                    acceleration_profiles: vec![AccelerationProfile::Adaptive],
                    natural_scroll: unsupported.clone(),
                    left_handed: unsupported.clone(),
                    tap: unsupported.clone(),
                    tap_drag: unsupported.clone(),
                    tap_drag_lock: unsupported.clone(),
                    click_methods: Vec::new(),
                    scroll_methods: Vec::new(),
                    scroll_button: available,
                    disable_while_typing: unsupported,
                },
                defaults: configuration.clone(),
                effective: configuration,
            }],
        }
    }
    #[test]
    fn other_device_or_appearance_revision_never_replaces_selected_drafts() {
        let mut view = InputView::default();
        view.replace(InputState {
            observation: Some(observation(0.0, 274)),
            message: None,
        });
        view.speed.set_text("0.7");
        view.button.set_text("275");
        let mut next = observation(0.0, 274);
        next.devices.push(next.devices[0].clone());
        next.devices[1].id = InputId("other".into());
        view.replace(InputState {
            observation: Some(next),
            message: None,
        });
        assert_eq!(view.speed.text(), "0.7");
        assert_eq!(view.button.text(), "275");
        view.replace(InputState {
            observation: Some(observation(0.0, 276)),
            message: None,
        });
        assert_eq!(view.speed.text(), "0.7");
        assert_eq!(view.button.text(), "276");
    }
}
