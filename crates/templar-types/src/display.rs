//! Exact, backend-neutral display observations and compositor-owned output transactions.
use crate::settings::{Capability, Revision};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMode {
    /// A complete timing identity, not a rounded width/height/refresh tuple.
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub preferred: bool,
    pub interlaced: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    #[default]
    Normal,
    Left,
    Inverted,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Insets {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}
impl Insets {
    pub fn is_zero(self) -> bool { self == Self::default() }
    pub fn visible_size(self, width: u32, height: u32) -> Option<(u32, u32)> {
        Some((width.checked_sub(self.left.checked_add(self.right)?)?, height.checked_sub(self.top.checked_add(self.bottom)?)?))
            .filter(|(width, height)| *width > 0 && *height > 0)
    }
    pub fn symmetric(horizontal: u32, vertical: u32) -> Self {
        Self { left: horizontal, right: horizontal, top: vertical, bottom: vertical }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfiguration {
    pub id: DisplayId,
    pub enabled: bool,
    pub mode: Option<String>,
    pub scale: f64,
    pub rotation: Rotation,
    pub x: i32,
    pub y: i32,
    /// The source is rendered independently on this output. Overlapping rectangles alone are not duplication.
    pub mirror_of: Option<DisplayId>,
    pub primary: bool,
    pub insets: Insets,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DisplayProfile {
    pub outputs: Vec<OutputConfiguration>,
    /// An explicit policy choice; accidentally disabling the last display is rejected.
    pub allow_headless: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub id: DisplayId,
    pub gpu: String,
    pub connector: String,
    pub make: String,
    pub model: String,
    pub serial: Option<String>,
    pub edid_sha256: Option<String>,
    pub connected: bool,
    pub internal: bool,
    pub modes: Vec<DisplayMode>,
    pub current: OutputConfiguration,
    /// Actual backend observation, independent of the last-owned layout/requested configuration.
    pub physical: PhysicalState,
    pub persistence: Capability,
    pub configure: Capability,
    pub hardware_underscan: Capability,
    pub composition_inset: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayFailure { pub gpu: String, pub connector: Option<String>, pub reason: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PhysicalState {
    /// Native KMS readback; mode is a complete timing identity, not a rounded tuple.
    NativeEnabled { mode: String },
    NativeDisabled,
    /// X server configuration only. This does not attest kernel KMS or physical presentation.
    RandRObserved { enabled: bool, mode: Option<String> },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayObservation {
    pub revision: Revision,
    pub displays: Vec<Display>,
    pub profile: DisplayProfile,
    pub preview: Option<PreviewObservation>,
    pub failures: Vec<DisplayFailure>,
    pub session_active: bool,
    pub locked: bool,
    /// Compositor-owned closed-panel policy, sourced only by the broker's logind lid owner.
    pub panel_closed: bool,
    pub can_identify: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewObservation {
    pub token: Uuid,
    pub kind: PreviewKind,
    pub proof: PreviewProof,
    /// None until the backend's explicit proof gate completes. NativePresented requires exact
    /// queued generation presentation on every enabled physical target; RandRReadback is only
    /// X-server configuration observation and must never protect a native locked session.
    pub confirmation_remaining_ms: Option<u64>,
    pub confirmed: bool,
    pub rollback_failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKind { Interactive, Operational }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewProof { NativePresented, RandRReadback }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPreset { Extend, Duplicate, InternalOnly, ExternalOnly }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayError {
    pub code: String,
    pub message: String,
}
impl DisplayError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}
impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}: {}", self.code, self.message) }
}
impl std::error::Error for DisplayError {}

pub fn logical_size(mode: &DisplayMode, configuration: &OutputConfiguration) -> Result<(i32, i32), DisplayError> {
    logical_size_at(mode, configuration.insets, configuration.rotation, configuration.scale)
}
pub fn logical_size_at(mode: &DisplayMode, insets: Insets, rotation: Rotation, scale: f64) -> Result<(i32, i32), DisplayError> {
    if !scale.is_finite() || !(0.5..=4.0).contains(&scale) {
        return Err(DisplayError::new("invalid_scale", "Scale must be finite and between 0.5 and 4"));
    }
    let (width, height) = insets.visible_size(mode.width, mode.height)
        .ok_or_else(|| DisplayError::new("invalid_inset", "Insets must leave a nonempty visible display"))?;
    let (width, height) = match rotation {
        Rotation::Left | Rotation::Right => (height, width),
        Rotation::Normal | Rotation::Inverted => (width, height),
    };
    let width = (f64::from(width) / scale).round();
    let height = (f64::from(height) / scale).round();
    if width < 1.0 || height < 1.0 || width > f64::from(i32::MAX) || height > f64::from(i32::MAX) {
        return Err(DisplayError::new("invalid_geometry", "Logical display dimensions are out of range"));
    }
    Ok((width as i32, height as i32))
}

/// Validate the whole inventory rather than silently applying a stale subset.
pub fn validate_profile(observation: &DisplayObservation, expected: Revision, profile: &DisplayProfile) -> Result<(), DisplayError> {
    validate_inventory(observation.revision, &observation.displays, observation.session_active, observation.locked, expected, profile)
}

/// Native authorities already own their inventory; validation must not clone an observation.
pub fn validate_inventory(revision: Revision, displays: &[Display], session_active: bool, locked: bool, expected: Revision, profile: &DisplayProfile) -> Result<(), DisplayError> {
    if revision != expected {
        return Err(DisplayError::new("stale_revision", "The compositor owner or output topology changed; refresh before applying"));
    }
    if !session_active || locked {
        return Err(DisplayError::new("session_unavailable", "Display changes require an active, unlocked session"));
    }
    let inventory: BTreeMap<_, _> = displays.iter().filter(|display| display.connected).map(|display| (&display.id, display)).collect();
    let mut configured = BTreeMap::new();
    let mut geometry = BTreeMap::new();
    let mut primary_count = 0;
    for configuration in &profile.outputs {
        if configured.insert(&configuration.id, configuration).is_some() {
            return Err(DisplayError::new("duplicate_output", "A display occurs more than once in the profile"));
        }
        let display = inventory.get(&configuration.id).ok_or_else(|| DisplayError::new("unknown_output", format!("Display {} is no longer connected", configuration.id.0)))?;
        if !configuration.enabled { continue; }
        match &display.configure {
            Capability::Available { .. } => {},
            Capability::Unsupported { reason } | Capability::Unavailable { reason } => return Err(DisplayError::new("unsupported_output", reason.clone())),
        }
        let mode = display.modes.iter().find(|mode| Some(&mode.id) == configuration.mode.as_ref())
            .ok_or_else(|| DisplayError::new("unknown_mode", format!("The exact mode for {} is not advertised", display.connector)))?;
        if !configuration.insets.is_zero() && !matches!(display.hardware_underscan, Capability::Available { .. }) && !matches!(display.composition_inset, Capability::Available { .. }) {
            return Err(DisplayError::new("unsupported_inset", "Neither hardware underscan nor compositor inset is available"));
        }
        let (width, height) = logical_size(mode, configuration)?;
        configuration.x.checked_add(width).ok_or_else(|| DisplayError::new("invalid_geometry", "Display right edge overflows"))?;
        configuration.y.checked_add(height).ok_or_else(|| DisplayError::new("invalid_geometry", "Display bottom edge overflows"))?;
        geometry.insert(&configuration.id, (configuration.x, configuration.y, width, height));
        primary_count += usize::from(configuration.primary);
    }
    if configured.len() != inventory.len() {
        return Err(DisplayError::new("incomplete_topology", "The profile must include every connected display, including disabled displays"));
    }
    if geometry.is_empty() && !profile.allow_headless {
        return Err(DisplayError::new("last_display", "Disabling the last display requires explicit headless policy"));
    }
    if (!geometry.is_empty() && primary_count != 1) || (geometry.is_empty() && primary_count != 0) {
        return Err(DisplayError::new("invalid_primary", "Exactly one enabled display must be primary"));
    }
    for configuration in profile.outputs.iter().filter(|configuration| configuration.enabled) {
        let Some(source_id) = &configuration.mirror_of else { continue; };
        if source_id == &configuration.id { return Err(DisplayError::new("invalid_mirror", "A display cannot mirror itself")); }
        let source = configured.get(source_id).filter(|source| source.enabled && source.mirror_of.is_none())
            .ok_or_else(|| DisplayError::new("invalid_mirror", "A mirror source must be enabled and cannot itself be a mirror"))?;
        if geometry.get(source_id) != geometry.get(&configuration.id) || source.primary && configuration.primary {
            return Err(DisplayError::new("invalid_mirror", "Duplicated outputs must have the same logical position and visible size"));
        }
    }
    Ok(())
}

/// Reconnect applies only confirmed profiles whose full connector/EDID set is still present.
pub fn matches_connected_inventory(profile: &DisplayProfile, displays: &[Display]) -> bool {
    let saved: BTreeSet<_> = profile.outputs.iter().map(|output| &output.id).collect();
    let connected: BTreeSet<_> = displays.iter().filter(|display| display.connected).map(|display| &display.id).collect();
    saved == connected && saved.len() == profile.outputs.len()
}

/// Pure arrangement shared by native Settings and the independent X11 adapter. Disabled
/// connectors stay in the full profile, and duplication names a real scene source explicitly.
pub fn arrange_profile(observation: &DisplayObservation, profile: &DisplayProfile, preset: LayoutPreset) -> Result<DisplayProfile, DisplayError> {
    let mut arranged = profile.clone();
    for output in &mut arranged.outputs {
        let display = observation.displays.iter().find(|display| display.connected && display.id == output.id).ok_or_else(|| DisplayError::new("unknown_output", "An arrangement target was disconnected"))?;
        output.enabled = !(observation.panel_closed && display.internal) && match preset { LayoutPreset::InternalOnly => display.internal, LayoutPreset::ExternalOnly => !display.internal, _ => true };
        output.primary = false;
        output.mirror_of = None;
        if output.enabled && !display.modes.iter().any(|mode| Some(&mode.id) == output.mode.as_ref()) {
            output.mode = display.modes.iter().find(|mode| mode.preferred).or_else(|| display.modes.first()).map(|mode| mode.id.clone());
        }
    }
    let primary = arranged.outputs.iter().position(|output| output.enabled && profile.outputs.iter().any(|old| old.id == output.id && old.primary)).or_else(|| arranged.outputs.iter().position(|output| output.enabled && observation.displays.iter().any(|display| display.id == output.id && display.internal))).or_else(|| arranged.outputs.iter().position(|output| output.enabled)).ok_or_else(|| DisplayError::new("no_layout_target", "No connected display satisfies this layout; headless is a separate explicit policy"))?;
    arranged.outputs[primary].primary = true;
    arranged.allow_headless = false;
    let source = &arranged.outputs[primary];
    let source_display = observation.displays.iter().find(|display| display.id == source.id).expect("connected layout source");
    let source_mode = source_display.modes.iter().find(|mode| Some(&mode.id) == source.mode.as_ref()).ok_or_else(|| DisplayError::new("unknown_mode", "The primary display has no exact advertised timing"))?;
    let source_size = logical_size(source_mode, source)?;
    let mut x = 0i32;
    for index in 0..arranged.outputs.len() {
        if !arranged.outputs[index].enabled { continue; }
        if preset == LayoutPreset::Duplicate {
            let display = observation.displays.iter().find(|display| display.id == arranged.outputs[index].id).expect("connected duplicate target");
            if index != primary {
                let target = &arranged.outputs[index];
                let mut candidate: Option<(&DisplayMode, f64, f64)> = None;
                for mode in &display.modes {
                    let Some((width, height)) = target.insets.visible_size(mode.width, mode.height) else { continue; };
                    let (width, _) = match target.rotation { Rotation::Left | Rotation::Right => (height, width), _ => (width, height) };
                    let scale = f64::from(width) / f64::from(source_size.0);
                    if !(0.5..=4.0).contains(&scale) { continue; }
                    if logical_size_at(mode, target.insets, target.rotation, scale).ok() != Some(source_size) { continue; }
                    let score = (scale - target.scale).abs() + if Some(&mode.id) == target.mode.as_ref() { 0.0 } else { 1.0 };
                    if candidate.as_ref().is_none_or(|(_, _, old_score)| score < *old_score) { candidate = Some((mode, scale, score)); }
                }
                let (mode, scale, _) = candidate.ok_or_else(|| DisplayError::new("no_duplicate_timing", format!("{} has no exact timing/scale matching the source visible aspect and logical size", display.connector)))?;
                arranged.outputs[index].mode = Some(mode.id.clone());
                arranged.outputs[index].scale = scale;
                arranged.outputs[index].mirror_of = Some(arranged.outputs[primary].id.clone());
            }
            arranged.outputs[index].x = 0;
            arranged.outputs[index].y = 0;
        } else {
            let display = observation.displays.iter().find(|display| display.id == arranged.outputs[index].id).expect("connected extend target");
            let mode = display.modes.iter().find(|mode| Some(&mode.id) == arranged.outputs[index].mode.as_ref()).ok_or_else(|| DisplayError::new("unknown_mode", "An enabled target has no exact advertised timing"))?;
            arranged.outputs[index].x = x;
            arranged.outputs[index].y = 0;
            x = x.checked_add(logical_size(mode, &arranged.outputs[index])?.0).ok_or_else(|| DisplayError::new("invalid_geometry", "The extended desktop overflows"))?;
        }
    }
    validate_profile(observation, observation.revision, &arranged)?;
    Ok(arranged)
}

/// Move an entire explicit mirror cluster. Independent overlapping outputs remain independent.
pub fn set_output_position(profile: &mut DisplayProfile, id: &DisplayId, x: i32, y: i32) -> Result<(), DisplayError> {
    let selected = profile.outputs.iter().position(|output| &output.id == id).ok_or_else(|| DisplayError::new("unknown_output", "The arranged output no longer exists"))?;
    let source = if let Some(source) = &profile.outputs[selected].mirror_of { profile.outputs.iter().position(|output| &output.id == source).ok_or_else(|| DisplayError::new("invalid_mirror", "The mirror source no longer exists"))? } else { selected };
    for index in 0..profile.outputs.len() {
        if index == source || profile.outputs[index].mirror_of.as_ref() == Some(&profile.outputs[source].id) {
            profile.outputs[index].x = x;
            profile.outputs[index].y = y;
        }
    }
    Ok(())
}

/// Largest centered visible rectangle with the requested displayed aspect, signal mode intact.
pub fn aspect_insets(mode: &DisplayMode, rotation: Rotation, numerator: u32, denominator: u32) -> Result<Insets, DisplayError> {
    if numerator == 0 || denominator == 0 { return Err(DisplayError::new("invalid_aspect", "Aspect components must be nonzero")); }
    let (numerator, denominator) = match rotation { Rotation::Left | Rotation::Right => (denominator, numerator), _ => (numerator, denominator) };
    let (width, height) = (u64::from(mode.width), u64::from(mode.height));
    let (visible_width, visible_height) = if width * u64::from(denominator) > height * u64::from(numerator) { (height * u64::from(numerator) / u64::from(denominator), height) } else { (width, width * u64::from(denominator) / u64::from(numerator)) };
    if visible_width == 0 || visible_height == 0 { return Err(DisplayError::new("invalid_aspect", "Aspect must leave a nonempty visible rectangle")); }
    let horizontal = (width - visible_width) as u32;
    let vertical = (height - visible_height) as u32;
    Ok(Insets { left: horizontal / 2, right: horizontal - horizontal / 2, top: vertical / 2, bottom: vertical - vertical / 2 })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inset_geometry_uses_visible_area_and_rotation() {
        let mode = DisplayMode { id: "exact".into(), width: 1920, height: 1080, refresh_millihz: 59940, preferred: true, interlaced: false };
        let configuration = OutputConfiguration { id: DisplayId("gpu/HDMI/edid".into()), enabled: true, mode: Some("exact".into()), scale: 2.0, rotation: Rotation::Left, x: 0, y: 0, mirror_of: None, primary: true, insets: Insets::symmetric(20, 10) };
        assert_eq!(logical_size(&mode, &configuration).unwrap(), (530, 940));
        assert_eq!(Insets::symmetric(960, 0).visible_size(1920, 1080), None);
    }
    #[test]
    fn reconnect_requires_the_complete_stable_id_set() {
        assert!(!matches_connected_inventory(&DisplayProfile { outputs: vec![OutputConfiguration { id: DisplayId("gone".into()), enabled: false, mode: None, scale: 1.0, rotation: Rotation::Normal, x: 0, y: 0, mirror_of: None, primary: false, insets: Insets::default() }], allow_headless: false }, &[]));
        assert!(matches_connected_inventory(&DisplayProfile::default(), &[]));
    }
    fn inventory() -> DisplayObservation {
        let mut displays=Vec::new();
        for (id,width,height,internal) in [("internal",1920,1080,true),("external",1280,720,false)] {
            let mode=DisplayMode {id:format!("{id}:exact"),width,height,refresh_millihz:60000,preferred:true,interlaced:false};
            let current=OutputConfiguration {id:DisplayId(id.into()),enabled:true,mode:Some(mode.id.clone()),scale:1.0,rotation:Rotation::Normal,x:0,y:0,mirror_of:None,primary:internal,insets:Insets::default()};
            let available=Capability::Available {requires_restart:false};
            displays.push(Display {id:current.id.clone(),gpu:"pure-inventory".into(),connector:id.into(),make:String::new(),model:String::new(),serial:None,edid_sha256:None,connected:true,internal,modes:vec![mode],current,physical:PhysicalState::Unavailable {reason:"Pure layout test, not hardware proof".into()},persistence:available.clone(),configure:available.clone(),hardware_underscan:available.clone(),composition_inset:available});
        }
        DisplayObservation {revision:Revision::new_owner(),profile:DisplayProfile {outputs:displays.iter().map(|display|display.current.clone()).collect(),allow_headless:false},displays,preview:None,failures:Vec::new(),session_active:true,locked:false,panel_closed:false,can_identify:Capability::Unsupported {reason:"Pure layout test".into()}}
    }
    #[test]
    fn shared_presets_keep_full_topology_and_duplicate_only_compatible_exact_timings() {
        let observation=inventory();
        let extended=arrange_profile(&observation,&observation.profile,LayoutPreset::Extend).unwrap();
        assert_eq!(extended.outputs[1].x,1920);
        let duplicated=arrange_profile(&observation,&observation.profile,LayoutPreset::Duplicate).unwrap();
        assert_eq!(duplicated.outputs[1].mirror_of.as_ref(),Some(&duplicated.outputs[0].id));
        assert_eq!(duplicated.outputs[1].mode,Some("external:exact".into()));
        assert_eq!(logical_size(&observation.displays[1].modes[0],&duplicated.outputs[1]).unwrap(),(1920,1080));
        let external=arrange_profile(&observation,&observation.profile,LayoutPreset::ExternalOnly).unwrap();
        assert_eq!(external.outputs.len(),2);
        assert!(!external.outputs[0].enabled); assert!(external.outputs[1].primary);
        let mut incompatible=observation.clone(); incompatible.displays[1].modes[0].height=1024;
        assert!(arrange_profile(&incompatible,&incompatible.profile,LayoutPreset::Duplicate).is_err());
    }
    #[test]
    fn moving_a_mirror_moves_only_its_explicit_source_cluster() {
        let observation=inventory();
        let mut profile=arrange_profile(&observation,&observation.profile,LayoutPreset::Duplicate).unwrap();
        let mut independent=profile.outputs[0].clone(); independent.id=DisplayId("independent-overlap".into()); independent.primary=false;
        profile.outputs.push(independent);
        let mirror=profile.outputs[1].id.clone();
        set_output_position(&mut profile,&mirror,-250,70).unwrap();
        assert_eq!((profile.outputs[0].x,profile.outputs[0].y),(-250,70));
        assert_eq!((profile.outputs[1].x,profile.outputs[1].y),(-250,70));
        assert_eq!((profile.outputs[2].x,profile.outputs[2].y),(0,0));
    }
    #[test]
    fn aspect_calibration_never_rewrites_the_signal_timing() {
        let mode=DisplayMode {id:"1920x1080-complete-timing".into(),width:1920,height:1080,refresh_millihz:59940,preferred:true,interlaced:false};
        assert_eq!(aspect_insets(&mode,Rotation::Normal,4,3).unwrap(),Insets::symmetric(240,0));
        assert_eq!(aspect_insets(&mode,Rotation::Left,3,4).unwrap(),Insets::symmetric(240,0));
        assert!(aspect_insets(&mode,Rotation::Normal,0,9).is_err());
        assert_eq!((mode.width,mode.height,mode.refresh_millihz),(1920,1080,59940));
    }
}
