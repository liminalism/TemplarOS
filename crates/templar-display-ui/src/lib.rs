//! One display editor for native KMS and standalone RandR; adapters own effects and rollback.
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::{ScrollState, TextFieldState, Ui};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use templar_types::{
    display::{
        self, DisplayId, DisplayObservation, DisplayProfile, Insets, LayoutPreset,
        OutputConfiguration, PhysicalState, PreviewKind, PreviewProof, Rotation,
    },
    settings::{Capability, Revision},
};
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct DisplaysState {
    pub observation: Option<DisplayObservation>,
    pub message: Option<String>,
}
#[derive(Debug)]
pub enum DisplaysCommand {
    Refresh,
    Validate {
        expected: Revision,
        profile: DisplayProfile,
    },
    Preview {
        expected: Revision,
        profile: DisplayProfile,
    },
    Confirm {
        expected: Revision,
        token: Uuid,
    },
    Revert {
        token: Uuid,
    },
    Identify {
        expected: Revision,
        outputs: Vec<DisplayId>,
        calibration: bool,
    },
}
fn available(capability: &Capability) -> bool {
    matches!(capability, Capability::Available { .. })
}
fn reason(capability: &Capability) -> Option<&str> {
    match capability {
        Capability::Available { .. } => None,
        Capability::Unsupported { reason } | Capability::Unavailable { reason } => Some(reason),
    }
}
#[derive(Default, Debug)]
pub struct DisplaysView {
    pub state: DisplaysState,
    draft: Option<DisplayProfile>,
    selected: Option<DisplayId>,
    fields: [TextFieldState; 9],
    drawn_fields: [bool; 9],
    scroll: ScrollState,
    heights: Vec<i32>,
    labels: BTreeMap<DisplayId, String>,
    status: BTreeMap<DisplayId, String>,
    modes: BTreeMap<DisplayId, Vec<String>>,
    failures: Vec<String>,
    preview_label: String,
    drag: Option<(DisplayId, i32, i32, i32, i32, f64)>,
    last_refresh: Option<Instant>,
}
impl DisplaysView {
    pub fn replace(&mut self, state: DisplaysState) -> bool {
        let reset = match (&self.state.observation, &state.observation) {
            (Some(old), Some(new)) => {
                old.revision.owner_epoch != new.revision.owner_epoch
                    || old.profile != new.profile
                    || !display::matches_connected_inventory(&old.profile, &new.displays)
            }
            _ => true,
        };
        if reset {
            self.draft = state
                .observation
                .as_ref()
                .map(|observation| observation.profile.clone());
            self.drag = None;
        }
        if self.selected.as_ref().is_none_or(|id| {
            !state.observation.as_ref().is_some_and(|observation| {
                observation
                    .displays
                    .iter()
                    .any(|display| &display.id == id && display.connected)
            })
        }) {
            self.selected = state.observation.as_ref().and_then(|observation| {
                observation
                    .displays
                    .iter()
                    .find(|display| display.connected)
                    .map(|display| display.id.clone())
            });
        }
        self.labels.clear();
        self.status.clear();
        self.modes.clear();
        self.failures.clear();
        self.preview_label.clear();
        if let Some(observation) = &state.observation {
            for display in &observation.displays {
                self.labels.insert(
                    display.id.clone(),
                    format!("{} — {} {}", display.connector, display.make, display.model),
                );
                let physical = match &display.physical {
                    PhysicalState::NativeEnabled { mode } => format!(
                        "Kernel KMS active: {}",
                        display
                            .modes
                            .iter()
                            .find(|candidate| &candidate.id == mode)
                            .map_or(mode.as_str(), |candidate| candidate.id.as_str())
                    ),
                    PhysicalState::NativeDisabled => "Kernel KMS disabled".into(),
                    PhysicalState::RandRObserved { enabled, .. } => format!(
                        "X server: {}; not a KMS/presentation attestation",
                        if *enabled { "enabled" } else { "disabled" }
                    ),
                    PhysicalState::Unavailable { reason } => {
                        format!("Physical image unknown: {reason}")
                    }
                };
                self.status.insert(display.id.clone(), physical);
                self.modes.insert(
                    display.id.clone(),
                    display
                        .modes
                        .iter()
                        .map(|mode| {
                            format!(
                                "{} × {} · {:.3} Hz{}{}",
                                mode.width,
                                mode.height,
                                f64::from(mode.refresh_millihz) / 1000.0,
                                if mode.interlaced { " interlaced" } else { "" },
                                if mode.preferred { " preferred" } else { "" }
                            )
                        })
                        .collect(),
                );
            }
            self.failures
                .extend(observation.failures.iter().map(|failure| {
                    format!(
                        "{}{}: {}",
                        failure.gpu,
                        failure
                            .connector
                            .as_ref()
                            .map_or(String::new(), |connector| format!(" / {connector}")),
                        failure.reason
                    )
                }));
            if let Some(preview) = &observation.preview {
                self.preview_label = match preview.confirmation_remaining_ms {
                    Some(ms) => format!("{} · {:.1} seconds to confirm; owner reverts independently", match preview.proof { PreviewProof::NativePresented => "Exact native target frames presented", PreviewProof::RandRReadback => "X server configuration read back (not physical frame proof)" }, ms as f64 / 1000.0),
                    None => "Owner is waiting for the exact applied target proof; confirmation is unavailable".into(),
                };
                if let Some(failure) = &preview.rollback_failure {
                    self.preview_label.push_str(" · rollback failed: ");
                    self.preview_label.push_str(failure);
                }
            }
        }
        self.state = state;
        if reset
            && let Some(configuration) = self.selected.as_ref().and_then(|id| {
                self.draft
                    .as_ref()?
                    .outputs
                    .iter()
                    .find(|output| &output.id == id)
            })
        {
            sync_fields(&mut self.fields, configuration);
        }
        self.heights = self
            .state
            .observation
            .as_ref()
            .map_or_else(Vec::new, |observation| {
                observation
                    .displays
                    .iter()
                    .map(|display| {
                        if self.selected.as_ref() == Some(&display.id) {
                            960 + display.modes.len() as i32 * 36
                        } else {
                            124
                        }
                    })
                    .collect()
            });
        true
    }
    pub fn caret_area(&self) -> Option<Rect> {
        self.fields
            .iter()
            .zip(self.drawn_fields)
            .find_map(|(field, drawn)| drawn.then(|| field.caret_area()).flatten())
    }
    fn draw_overview(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        h: i32,
        gap: i32,
    ) -> (Option<DisplaysCommand>, i32, bool) {
        let mut y = area.y;
        let mut command = None;
        if ui.button(Rect::new(area.x, y, ui.px(94), h), "Read actual") {
            command = Some(DisplaysCommand::Refresh);
        }
        ui.label(
            Rect::new(area.x + ui.px(104), y, area.w - ui.px(104), h),
            "Layout and calibration below are drafts until an owned preview is applied",
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
            ui.label(
                Rect::new(area.x, y, area.w, h),
                "Display source unavailable; no saved image is presented as actual state",
                ui.theme.text_dim,
                Align::Left,
            );
            return (command, y, false);
        };
        for failure in &self.failures {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                failure,
                ui.theme.alarm,
                Align::Left,
            );
            y += h + gap;
        }
        let editable =
            observation.session_active && !observation.locked && observation.preview.is_none();
        if let Some(preview) = &observation.preview {
            ui.label(
                Rect::new(area.x, y, area.w, h * 2),
                &self.preview_label,
                ui.theme.text,
                Align::Left,
            );
            y += h * 2 + gap;
            if preview.kind == PreviewKind::Interactive {
                if preview
                    .confirmation_remaining_ms
                    .is_some_and(|remaining| remaining > 0)
                    && preview.rollback_failure.is_none()
                    && ui.button(Rect::new(area.x, y, ui.px(140), h), "Confirm actual")
                {
                    command = Some(DisplaysCommand::Confirm {
                        expected: observation.revision,
                        token: preview.token,
                    });
                }
                if ui.button(
                    Rect::new(area.x + ui.px(150), y, ui.px(110), h),
                    "Revert now",
                ) {
                    command = Some(DisplaysCommand::Revert {
                        token: preview.token,
                    });
                }
            }
            y += h + gap;
        } else if !editable {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                "Configuration is unavailable while the session is inactive or locked",
                ui.theme.text_dim,
                Align::Left,
            );
            y += h + gap;
        }
        let Some(draft) = &mut self.draft else {
            return (command, y, editable);
        };
        let atlas = Rect::new(area.x, y, area.w, ui.px(150));
        y = atlas.bottom() + gap;
        let atlas_changed = draw_atlas(
            ui,
            atlas,
            observation,
            draft,
            &self.labels,
            &mut self.selected,
            &mut self.drag,
            editable,
        );
        if atlas_changed
            && let Some(configuration) = self
                .selected
                .as_ref()
                .and_then(|id| draft.outputs.iter().find(|output| &output.id == id))
        {
            sync_fields(&mut self.fields, configuration);
        }
        if editable {
            let width = ((area.w - gap * 3) / 4).max(1);
            for (index, (label, preset)) in [
                ("Extend", LayoutPreset::Extend),
                ("Duplicate", LayoutPreset::Duplicate),
                ("Internal only", LayoutPreset::InternalOnly),
                ("External only", LayoutPreset::ExternalOnly),
            ]
            .into_iter()
            .enumerate()
            {
                if ui.button(
                    Rect::new(area.x + index as i32 * (width + gap), y, width, h),
                    label,
                ) {
                    match display::arrange_profile(observation, draft, preset) {
                        Ok(changed) => {
                            *draft = changed;
                            if let Some(configuration) = self
                                .selected
                                .as_ref()
                                .and_then(|id| draft.outputs.iter().find(|output| &output.id == id))
                            {
                                sync_fields(&mut self.fields, configuration);
                            }
                        }
                        Err(error) => self.state.message = Some(error.to_string()),
                    }
                }
            }
            y += h + gap;
            if ui.button(Rect::new(area.x, y, ui.px(125), h), "Preview draft") {
                command = Some(DisplaysCommand::Preview {
                    expected: observation.revision,
                    profile: draft.clone(),
                });
            }
            if ui.button(
                Rect::new(area.x + ui.px(135), y, ui.px(125), h),
                "Validate draft",
            ) {
                command = Some(DisplaysCommand::Validate {
                    expected: observation.revision,
                    profile: draft.clone(),
                });
            }
            if available(&observation.can_identify)
                && ui.button(Rect::new(area.x + ui.px(270), y, ui.px(110), h), "Identify")
            {
                command = Some(DisplaysCommand::Identify {
                    expected: observation.revision,
                    outputs: observation
                        .profile
                        .outputs
                        .iter()
                        .filter(|output| output.enabled)
                        .map(|output| output.id.clone())
                        .collect(),
                    calibration: false,
                });
            }
            y += h + gap;
            ui.toggle(Rect::new(area.x,y,area.w,h),&mut draft.allow_headless,"Explicitly allow no enabled display (headless); accidental last-display disable remains rejected");
            y += h + gap;
        }
        (command, y, editable)
    }

    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect) -> (Option<DisplaysCommand>, bool) {
        self.drawn_fields.fill(false);
        let h = ui.px(30);
        let gap = ui.px(6);
        let (mut command, y, editable) = self.draw_overview(ui, area, h, gap);
        let Some(observation) = &self.state.observation else {
            return (command, false);
        };
        let Some(draft) = &mut self.draft else {
            return (command, false);
        };
        let list_area = Rect::new(area.x, y, area.w, (area.bottom() - y).max(h));
        let mut picked = None;
        let mut position = None;
        let mut primary = None;
        for (index, display) in observation.displays.iter().enumerate() {
            self.heights[index] = ui.px(if self.selected.as_ref() == Some(&display.id) {
                1100 + display.modes.len() as i32 * 36
            } else {
                124
            });
        }
        let selected = &self.selected;
        let fields = &mut self.fields;
        let drawn = &mut self.drawn_fields;
        ui.scroll_list(&mut self.scroll, list_area, &self.heights, |ui, index, rect| {
            let display = &observation.displays[index]; let h = ui.px(30); let gap = ui.px(6); let mut y = rect.y;
            if ui.button(Rect::new(rect.x, y, rect.w, h), self.labels.get(&display.id).map_or(display.connector.as_str(), String::as_str)) { picked = Some(display.id.clone()); }
            y += h + gap;
            ui.label(Rect::new(rect.x, y, rect.w, h), self.status.get(&display.id).map_or("Unknown", String::as_str), ui.theme.text_dim, Align::Left); y += h + gap;
            if !display.connected { return; }
            if let Some(reason) = reason(&display.configure) { ui.label(Rect::new(rect.x, y, rect.w, h), reason, ui.theme.alarm, Align::Left); y += h + gap; }
            if selected.as_ref() != Some(&display.id) { return; }
            if let Some(reason) = reason(&display.persistence) { ui.label(Rect::new(rect.x, y, rect.w, h), reason, ui.theme.text_dim, Align::Left); y += h + gap; }
            let Some(configuration) = draft.outputs.iter_mut().find(|output| output.id == display.id) else { return; };
            let can_edit = editable && available(&display.configure);
            if can_edit && ui.toggle(Rect::new(rect.x, y, rect.w, h), &mut configuration.enabled, "Enable this display in the draft") && configuration.enabled && configuration.mode.is_none() { configuration.mode = display.modes.iter().find(|mode| mode.preferred).or_else(|| display.modes.first()).map(|mode| mode.id.clone()); }
            y += h + gap;
            if can_edit {
                for (field, label) in [(0, "X"), (1, "Y"), (2, "Scale")] { let field_area = Rect::new(rect.x + field as i32 * ui.px(130), y, ui.px(120), h); ui.text_field(&mut fields[field], field_area, label, false, false); drawn[field] = true; }
                y += h + gap;
                if ui.button(Rect::new(rect.x, y, ui.px(180), h), "Set coordinates / scale") {
                    match parse_position(fields) { Ok((x, y, scale)) => { position = Some((configuration.id.clone(),x,y)); configuration.scale = scale; }, Err(error) => self.state.message = Some(error.into()) }
                }
                y += h + gap;
                for (index, (label, dx, dy)) in [("Left", -10_i32, 0_i32), ("Right", 10, 0), ("Up", 0, -10), ("Down", 0, 10)].into_iter().enumerate() {
                    if ui.button(Rect::new(rect.x + index as i32 * ui.px(88), y, ui.px(80), h), label) { position = Some((configuration.id.clone(),configuration.x.saturating_add(dx),configuration.y.saturating_add(dy))); }
                }
                y += h + gap;
                if configuration.enabled && configuration.mirror_of.is_none() && ui.button(Rect::new(rect.x,y,ui.px(220),h),"Make this draft source primary") { primary = Some(configuration.id.clone()); }
                y += h + gap;
                for (index, (label, rotation)) in [("Normal", Rotation::Normal), ("Left 90°", Rotation::Left), ("180°", Rotation::Inverted), ("Right 90°", Rotation::Right)].into_iter().enumerate() { if ui.button(Rect::new(rect.x + index as i32 * ui.px(104), y, ui.px(96), h), label) { configuration.rotation = rotation; } }
                y += h + gap;
                ui.label(Rect::new(rect.x, y, rect.w, h), "Exact advertised signal timings (the signal mode is not a fake desktop size)", ui.theme.text_dim, Align::Left); y += h + gap;
                for (mode, label) in display.modes.iter().zip(self.modes.get(&display.id).into_iter().flatten()) { if ui.button(Rect::new(rect.x, y, rect.w, h), label) { configuration.mode = Some(mode.id.clone()); } y += h + gap; }
                let inset_available = available(&display.composition_inset) || available(&display.hardware_underscan);
                if inset_available {
                    ui.label(Rect::new(rect.x, y, rect.w, h), "Signal-pixel margins: left, right, top, bottom (preview before confirming)", ui.theme.text_dim, Align::Left); y += h + gap;
                    for field in 3..7 { ui.text_field(&mut fields[field], Rect::new(rect.x + (field - 3) as i32 * ui.px(104), y, ui.px(96), h), ["Left", "Right", "Top", "Bottom"][field - 3], false, false); drawn[field] = true; } y += h + gap;
                    if ui.button(Rect::new(rect.x, y, ui.px(170), h), "Set margin draft") {
                        match parse_insets(fields) { Ok(insets) if available(&display.composition_inset) || insets.left == insets.right && insets.top == insets.bottom => configuration.insets = insets, Ok(_) => self.state.message = Some("This adapter supports only symmetric physical hardware borders; asymmetric compositor calibration is unavailable".into()), Err(error) => self.state.message = Some(error.into()) }
                    }
                    if ui.button(Rect::new(rect.x + ui.px(180), y, ui.px(125), h), "Reset margins") { configuration.insets = Insets::default(); sync_fields(fields, configuration); } y += h + gap;
                    for (index, (label, numerator, denominator)) in [("16:9", 16, 9), ("4:3", 4, 3), ("21:9", 21, 9)].into_iter().enumerate() {
                        if ui.button(Rect::new(rect.x + index as i32 * ui.px(90), y, ui.px(82), h), label)
                            && let Some(mode) = display.modes.iter().find(|mode| configuration.mode.as_ref() == Some(&mode.id)) {
                            match display::aspect_insets(mode, configuration.rotation, numerator, denominator) {
                                Ok(insets) => { configuration.insets = insets; sync_fields(fields, configuration); },
                                Err(error) => self.state.message = Some(error.to_string()),
                            }
                        }
                    }
                    y += h + gap;
                    for field in 7..9 { ui.text_field(&mut fields[field],Rect::new(rect.x+(field-7) as i32*ui.px(100),y,ui.px(92),h),if field==7{"Aspect width"}else{"Aspect height"},false,false); drawn[field]=true; }
                    if ui.button(Rect::new(rect.x+ui.px(208),y,ui.px(150),h),"Set custom aspect") {
                        let aspect=fields[7].text().parse::<u32>().ok().zip(fields[8].text().parse::<u32>().ok());
                        if let Some((numerator,denominator))=aspect {
                            if let Some(mode)=display.modes.iter().find(|mode|configuration.mode.as_ref()==Some(&mode.id)) {
                                match display::aspect_insets(mode,configuration.rotation,numerator,denominator) { Ok(insets)=>{configuration.insets=insets;sync_fields(fields,configuration);},Err(error)=>self.state.message=Some(error.to_string()) }
                            }
                        }else{self.state.message=Some("Aspect components must be nonzero unsigned integers".into());}
                    }
                    y += h + gap;
                } else if let Some(reason) = reason(&display.composition_inset) { ui.label(Rect::new(rect.x, y, rect.w, h), reason, ui.theme.text_dim, Align::Left); y += h + gap; }
            }
            if available(&observation.can_identify) && display.current.enabled && !observation.locked && ui.button(Rect::new(rect.x, y, ui.px(270), h), "Calibration on actual applied image") { command = Some(DisplaysCommand::Identify { expected: observation.revision, outputs: vec![display.id.clone()], calibration: true }); }
        });
        if let Some((id, x, y)) = position {
            match display::set_output_position(draft, &id, x, y) {
                Ok(()) => {
                    if let Some(configuration) = draft.outputs.iter().find(|output| output.id == id)
                    {
                        sync_fields(fields, configuration);
                    }
                }
                Err(error) => self.state.message = Some(error.to_string()),
            }
        }
        if let Some(id) = primary {
            for output in &mut draft.outputs {
                output.primary = output.id == id;
            }
        }
        if let Some(id) = picked {
            self.selected = Some(id.clone());
            if let Some(configuration) = draft.outputs.iter().find(|output| output.id == id) {
                sync_fields(fields, configuration);
            }
            for (index, display) in observation.displays.iter().enumerate() {
                self.heights[index] = ui.px(if self.selected.as_ref() == Some(&display.id) {
                    1100 + display.modes.len() as i32 * 36
                } else {
                    124
                });
            }
        }
        let now = Instant::now();
        if command.is_none()
            && observation.preview.is_some()
            && self
                .last_refresh
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(300))
        {
            self.last_refresh = Some(now);
            command = Some(DisplaysCommand::Refresh);
        }
        (
            command,
            observation.preview.is_some() || self.drag.is_some(),
        )
    }
}
fn sync_fields(fields: &mut [TextFieldState; 9], configuration: &OutputConfiguration) {
    for (field, value) in fields.iter_mut().zip([
        configuration.x.to_string(),
        configuration.y.to_string(),
        configuration.scale.to_string(),
        configuration.insets.left.to_string(),
        configuration.insets.right.to_string(),
        configuration.insets.top.to_string(),
        configuration.insets.bottom.to_string(),
        "16".into(),
        "9".into(),
    ]) {
        field.set_text(value);
    }
}
fn parse_position(fields: &[TextFieldState; 9]) -> Result<(i32, i32, f64), &'static str> {
    let x = fields[0]
        .text()
        .parse()
        .map_err(|_| "X must be a signed integer")?;
    let y = fields[1]
        .text()
        .parse()
        .map_err(|_| "Y must be a signed integer")?;
    let scale: f64 = fields[2]
        .text()
        .parse()
        .map_err(|_| "Scale must be a finite number")?;
    if !scale.is_finite() || !(0.5..=4.0).contains(&scale) {
        return Err("Scale must be finite, between 0.5 and 4.0");
    }
    Ok((x, y, scale))
}
fn parse_insets(fields: &[TextFieldState; 9]) -> Result<Insets, &'static str> {
    let mut values = [0_u32; 4];
    for (index, value) in values.iter_mut().enumerate() {
        *value = fields[index + 3]
            .text()
            .parse()
            .map_err(|_| "Margins must be nonnegative signal-pixel integers")?;
    }
    Ok(Insets {
        left: values[0],
        right: values[1],
        top: values[2],
        bottom: values[3],
    })
}
fn draw_atlas(
    ui: &mut Ui<'_>,
    area: Rect,
    observation: &DisplayObservation,
    draft: &mut DisplayProfile,
    labels: &BTreeMap<DisplayId, String>,
    selected: &mut Option<DisplayId>,
    drag: &mut Option<(DisplayId, i32, i32, i32, i32, f64)>,
    editable: bool,
) -> bool {
    ui.painter.fill_rect(area, ui.theme.panel_alt);
    let mut changed = false;
    let mut bounds: Option<(i64, i64, i64, i64)> = None;
    for output in draft.outputs.iter().filter(|output| output.enabled) {
        let Some((w, h)) = observation
            .displays
            .iter()
            .find(|display| display.id == output.id)
            .and_then(|display| {
                display
                    .modes
                    .iter()
                    .find(|mode| output.mode.as_ref() == Some(&mode.id))
            })
            .and_then(|mode| display::logical_size(mode, output).ok())
        else {
            continue;
        };
        let (x, y) = (i64::from(output.x), i64::from(output.y));
        bounds = Some(bounds.map_or(
            (x, y, x + i64::from(w), y + i64::from(h)),
            |(l, t, r, b)| {
                (
                    l.min(x),
                    t.min(y),
                    r.max(x + i64::from(w)),
                    b.max(y + i64::from(h)),
                )
            },
        ));
    }
    let Some((left, top, right, bottom)) = bounds else {
        ui.label(
            area,
            "No enabled draft display",
            ui.theme.text_dim,
            Align::Centre,
        );
        return false;
    };
    let factor = (f64::from((area.w - 16).max(1)) / (right - left).max(1) as f64)
        .min(f64::from((area.h - 16).max(1)) / (bottom - top).max(1) as f64);
    for output in draft.outputs.iter().filter(|output| output.enabled) {
        let Some((width, height)) = observation
            .displays
            .iter()
            .find(|display| display.id == output.id)
            .and_then(|display| {
                display
                    .modes
                    .iter()
                    .find(|mode| output.mode.as_ref() == Some(&mode.id))
            })
            .and_then(|mode| display::logical_size(mode, output).ok())
        else {
            continue;
        };
        let rect = Rect::new(
            area.x + 8 + ((i64::from(output.x) - left) as f64 * factor).round() as i32,
            area.y + 8 + ((i64::from(output.y) - top) as f64 * factor).round() as i32,
            (f64::from(width) * factor).round().max(1.0) as i32,
            (f64::from(height) * factor).round().max(1.0) as i32,
        );
        ui.painter.fill_rect(
            rect,
            if selected.as_ref() == Some(&output.id) {
                ui.theme.selection
            } else {
                ui.theme.panel
            },
        );
        ui.painter.stroke_rect(rect, 2, ui.theme.accent);
        ui.label(
            rect,
            labels
                .get(&output.id)
                .map_or(output.id.0.as_str(), String::as_str),
            ui.theme.text,
            Align::Centre,
        );
        if ui.input.take_click(rect) {
            if selected.as_ref() != Some(&output.id) {
                *selected = Some(output.id.clone());
                changed = true;
            }
            if editable && let Some((px, py)) = ui.input.cursor() {
                *drag = Some((output.id.clone(), px, py, output.x, output.y, factor));
            }
        }
    }
    if !ui.input.is_held() {
        *drag = None;
    } else if editable
        && let Some((id, px, py, x, y, drag_scale)) = drag
        && let Some((cx, cy)) = ui.input.cursor()
    {
        let nx = (f64::from(*x) + f64::from(cx - *px) / *drag_scale)
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
        let ny = (f64::from(*y) + f64::from(cy - *py) / *drag_scale)
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
        if draft
            .outputs
            .iter()
            .find(|output| &output.id == id)
            .is_some_and(|output| output.x != nx || output.y != ny)
        {
            changed |= display::set_output_position(draft, id, nx, ny).is_ok();
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_edits_keep_negative_positions_and_reject_nonfinite_scale_or_unsigned_margins() {
        let mut fields = std::array::from_fn(|_| TextFieldState::default());
        let configuration = OutputConfiguration {
            id: DisplayId("pure-editor".into()),
            enabled: true,
            mode: None,
            scale: 1.25,
            rotation: Rotation::Normal,
            x: -1920,
            y: -100,
            mirror_of: None,
            primary: true,
            insets: Insets {
                left: 1,
                right: 2,
                top: 3,
                bottom: 4,
            },
        };
        sync_fields(&mut fields, &configuration);
        assert_eq!(parse_position(&fields).unwrap(), (-1920, -100, 1.25));
        assert_eq!(parse_insets(&fields).unwrap(), configuration.insets);
        fields[2].set_text("NaN");
        assert!(parse_position(&fields).is_err());
        fields[2].set_text("5");
        assert!(parse_position(&fields).is_err());
        fields[3].set_text("-1");
        assert!(parse_insets(&fields).is_err());
    }
}
