//! Output transaction state lives with the compositor, not the Settings client or daemon.
use std::{collections::BTreeSet, time::{Duration, Instant}};
use tokio::sync::watch::Sender;
use templar_types::{compositor_control::{ActivityObservation, ControlReply, ControlResponse}, display::{self, Display, DisplayError, DisplayId, DisplayObservation, DisplayProfile, PreviewObservation}, settings::{Capability, Revision}};
use uuid::Uuid;

pub const PRESENTATION_TIMEOUT: Duration = Duration::from_secs(5);
pub const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct PreparedPreview {
    pub token: Uuid,
    pub before: DisplayProfile,
    pub target: DisplayProfile,
}
#[derive(Debug)]
pub struct PreparedPanel {
    pub before: DisplayProfile,
    pub target: DisplayProfile,
    pub closed: bool,
}
#[derive(Debug)]
struct PendingPreview {
    token: Uuid,
    before: DisplayProfile,
    target: DisplayProfile,
    awaiting: BTreeSet<DisplayId>,
    deadline: Instant,
    presented: bool,
    topology_invalidated: bool,
    purpose: PendingPurpose,
    rollback_failure: Option<(Revision, String)>,
}
#[derive(Debug)]
enum PendingPurpose {
    Interactive,
    Operational { panel_closed: Option<bool>, reply: Option<tokio::sync::oneshot::Sender<ControlReply>> },
}
#[derive(Clone, Debug)]
pub struct IdentifyState {
    pub outputs: Vec<DisplayId>,
    pub calibration: bool,
    pub until: Instant,
}

#[derive(Debug)]
pub struct DisplayController {
    revision: Revision,
    inventory: Vec<Display>,
    failures: Vec<display::DisplayFailure>,
    physical_unknown: bool,
    pending: Option<PendingPreview>,
    pub identify: Option<IdentifyState>,
    native: bool,
    headless: bool,
    panel_closed: bool,
    panel_open_profile: Option<DisplayProfile>,
    activity: Option<ActivityObservation>,
    activity_dirty: bool,
    activity_subscribers: Vec<Sender<Option<ControlReply>>>,
}
impl Default for DisplayController {
    fn default() -> Self {
        Self { revision: Revision::new_owner(), inventory: Vec::new(), failures: Vec::new(), physical_unknown: false, pending: None, identify: None, native: false, headless: false, panel_closed: false, panel_open_profile: None, activity: None, activity_dirty: false, activity_subscribers: Vec::new() }
    }
}
impl DisplayController {
    pub fn set_native(&mut self) { self.native = true; }
    pub fn revision(&self) -> Revision { self.revision }
    fn changed(&mut self) { self.revision.generation = self.revision.generation.checked_add(1).expect("display generation exhausted"); }
    pub fn capability(&self) -> Capability {
        if self.native { Capability::Available { requires_restart: false } } else { Capability::Unsupported { reason: "Physical output configuration requires the native DRM/KMS backend".into() } }
    }
    pub fn replace_inventory(&mut self, mut inventory: Vec<Display>, topology_event: bool) {
        inventory.sort_by(|left, right| left.id.cmp(&right.id));
        if self.inventory == inventory { return; }
        if topology_event && let Some(pending) = &mut self.pending { pending.topology_invalidated = true; }
        self.inventory = inventory;
        self.changed();
    }
    pub fn replace_failures(&mut self, mut failures: Vec<display::DisplayFailure>, physical_unknown: bool) {
        failures.sort_by(|left, right| (&left.gpu, &left.connector, &left.reason).cmp(&(&right.gpu, &right.connector, &right.reason)));
        if self.failures == failures && self.physical_unknown == physical_unknown { return; }
        self.failures = failures;
        self.physical_unknown = physical_unknown;
        self.changed();
    }
    pub fn backend_failed(&mut self, reason: String) {
        if let Some(pending) = &mut self.pending { pending.topology_invalidated = true; }
        tracing::warn!(%reason, "native output transaction failed before final presentation");
        self.changed();
    }

    pub fn profile(&self) -> DisplayProfile {
        DisplayProfile { outputs: self.inventory.iter().filter(|display| display.connected).map(|display| display.current.clone()).collect(), allow_headless: self.headless }
    }
    pub fn observation(&self, session_active: bool, locked: bool, now: Instant) -> DisplayObservation {
        DisplayObservation {
            revision: self.revision, displays: self.inventory.clone(), profile: self.profile(), failures: self.failures.clone(),
            preview: self.pending.as_ref().map(|pending| PreviewObservation {
                token: pending.token, kind: if matches!(&pending.purpose, PendingPurpose::Interactive) { display::PreviewKind::Interactive } else { display::PreviewKind::Operational },
                proof: display::PreviewProof::NativePresented,
                confirmation_remaining_ms: (pending.presented && matches!(&pending.purpose, PendingPurpose::Interactive)).then(|| pending.deadline.saturating_duration_since(now).as_millis().min(u128::from(u64::MAX)) as u64),
                confirmed: false, rollback_failure: pending.rollback_failure.as_ref().map(|(_, message)| message.clone()),
            }),
            session_active, locked, panel_closed: self.panel_closed, can_identify: self.capability(),
        }
    }
    pub fn validate(&self, expected: Revision, profile: &DisplayProfile, session_active: bool, locked: bool) -> Result<(), DisplayError> {
        if !self.native { return Err(DisplayError::new("unsupported_backend", "Physical output configuration requires the native DRM/KMS backend")); }
        if self.panel_closed && profile.outputs.iter().any(|output| output.enabled && self.inventory.iter().any(|display| display.id == output.id && display.internal)) { return Err(DisplayError::new("panel_closed", "The logind-owned closed-panel policy keeps the internal display disabled")); }
        display::validate_inventory(self.revision, &self.inventory, session_active, locked, expected, profile)
    }
    pub fn prepare(&self, expected: Revision, profile: DisplayProfile, session_active: bool, locked: bool) -> Result<PreparedPreview, DisplayError> {
        if self.pending.is_some() { return Err(DisplayError::new("preview_active", "Confirm or revert the existing display preview first")); }
        self.validate(expected, &profile, session_active, locked)?;
        Ok(PreparedPreview { token: Uuid::new_v4(), before: self.profile(), target: profile })
    }
    /// Authenticated sessiond restores only a persisted, previously confirmed profile. Unlike
    /// interactive changes this system policy may run while locked; protected full-frame scenes,
    /// exact queued-generation presentation and native readback still gate its deferred reply.
    pub fn prepare_restore(&self, expected: Revision, profile: DisplayProfile, session_active: bool) -> Result<PreparedPreview, DisplayError> {
        self.prepare(expected, profile, session_active, false)
    }
    /// Called only after real backend application has succeeded; every enabled output must present.
    pub fn begin(&mut self, prepared: PreparedPreview, now: Instant) {
        let awaiting: BTreeSet<_> = prepared.target.outputs.iter().filter(|output| output.enabled).map(|output| output.id.clone()).collect();
        let presented = awaiting.is_empty();
        self.headless = prepared.target.allow_headless;
        self.pending = Some(PendingPreview { token: prepared.token, before: prepared.before, target: prepared.target, awaiting, deadline: now + if presented { CONFIRMATION_TIMEOUT } else { PRESENTATION_TIMEOUT }, presented, topology_invalidated: false, purpose: PendingPurpose::Interactive, rollback_failure: None });
        self.changed();
    }
    pub fn begin_operational(&mut self, before: DisplayProfile, target: DisplayProfile, panel_closed: Option<bool>, reply: tokio::sync::oneshot::Sender<ControlReply>, now: Instant) -> Uuid {
        let token = Uuid::new_v4();
        self.begin(PreparedPreview { token, before, target }, now);
        let pending = self.pending.as_mut().expect("begun operational transaction");
        pending.purpose = PendingPurpose::Operational { panel_closed, reply: Some(reply) };
        pending.deadline = now + PRESENTATION_TIMEOUT;
        token
    }
    pub fn interactive_token(&self, token: Uuid) -> bool { self.pending.as_ref().is_some_and(|pending| pending.token == token && matches!(&pending.purpose, PendingPurpose::Interactive)) }
    pub fn pending_allows_locked(&self) -> bool { self.pending.as_ref().is_some_and(|pending| matches!(&pending.purpose, PendingPurpose::Operational { .. })) }
    pub fn operational_ready(&self, now: Instant) -> Option<Uuid> { self.pending.as_ref().filter(|pending| pending.presented && !pending.topology_invalidated && pending.rollback_failure.is_none() && now < pending.deadline && matches!(&pending.purpose, PendingPurpose::Operational { .. })).map(|pending| pending.token) }
    pub fn take_completion(&mut self, token: Uuid) -> Option<tokio::sync::oneshot::Sender<ControlReply>> {
        let pending = self.pending.as_mut().filter(|pending| pending.token == token)?;
        match &mut pending.purpose { PendingPurpose::Operational { reply, .. } => reply.take(), PendingPurpose::Interactive => None }
    }
    pub fn complete_operational(&mut self, token: Uuid) -> Option<tokio::sync::oneshot::Sender<ControlReply>> {
        if !self.pending.as_ref().is_some_and(|pending| pending.token == token && matches!(&pending.purpose, PendingPurpose::Operational { .. })) { return None; }
        let pending = self.pending.take().expect("matching operational transaction");
        let PendingPurpose::Operational { panel_closed, reply } = pending.purpose else { unreachable!("checked operational purpose"); };
        if let Some(closed) = panel_closed { self.panel_closed = closed; if !closed { self.panel_open_profile = None; } }
        self.changed();
        reply
    }
    pub fn queued_token_for(&self, actual: &display::OutputConfiguration) -> Option<Uuid> { self.pending.as_ref().filter(|pending| !pending.topology_invalidated && Instant::now() < pending.deadline && pending.target.outputs.iter().any(|target| target == actual)).map(|pending| pending.token) }
    pub fn preview_target(&self, token: Uuid) -> Result<&DisplayProfile, DisplayError> { self.pending.as_ref().filter(|pending| pending.token == token).map(|pending| &pending.target).ok_or_else(|| DisplayError::new("unknown_preview", "The display transaction is no longer active")) }
    pub fn frame_presented(&mut self, id: &DisplayId, exact_queued_token: Option<Uuid>, now: Instant) {
        let Some(pending) = &mut self.pending else { return; };
        if exact_queued_token != Some(pending.token) || pending.topology_invalidated || pending.presented || now >= pending.deadline { return; }
        pending.awaiting.remove(id);
        if pending.awaiting.is_empty() {
            pending.presented = true;
            if matches!(&pending.purpose, PendingPurpose::Interactive) { pending.deadline = now + CONFIRMATION_TIMEOUT; }
            self.changed();
        }
    }
    pub fn confirmation_target(&self, expected: Revision, token: Uuid, now: Instant) -> Result<&DisplayProfile, DisplayError> {
        if expected != self.revision { return Err(DisplayError::new("stale_revision", "The output owner or topology changed")); }
        let pending = self.pending.as_ref().filter(|pending| pending.token == token).ok_or_else(|| DisplayError::new("unknown_preview", "The display preview is no longer active"))?;
        if !matches!(&pending.purpose, PendingPurpose::Interactive) { return Err(DisplayError::new("operational_transaction", "The compositor owns completion of this display transaction")); }
        if !pending.presented || pending.topology_invalidated || now >= pending.deadline { return Err(DisplayError::new("preview_not_confirmable", "The preview has not been presented, expired, or was invalidated by hotplug")); }
        Ok(&pending.target)
    }
    pub fn confirm(&mut self, expected: Revision, token: Uuid, now: Instant) -> Result<(), DisplayError> {
        self.confirmation_target(expected, token, now)?;
        self.pending = None;
        self.changed();
        Ok(())
    }
    pub fn rollback(&self, token: Uuid) -> Result<DisplayProfile, DisplayError> {
        let pending = self.pending.as_ref().filter(|pending| pending.token == token).ok_or_else(|| DisplayError::new("unknown_preview", "The display preview is no longer active"))?;
        let connected: BTreeSet<_> = self.inventory.iter().filter(|display| display.connected).map(|display| &display.id).collect();
        let mut before = pending.before.clone();
        before.outputs.retain(|output| connected.contains(&output.id));
        // Newly connected displays retain their actual backend state, not an invented saved mode.
        for display in self.inventory.iter().filter(|display| display.connected) {
            if !before.outputs.iter().any(|output| output.id == display.id) { before.outputs.push(display.current.clone()); }
        }
        repair_links(&mut before);
        Ok(before)
    }
    pub fn reverted(&mut self, token: Uuid, profile: &DisplayProfile) {
        if self.pending.as_ref().is_some_and(|pending| pending.token == token) { self.pending = None; self.headless = profile.allow_headless; self.changed(); }
    }
    pub fn rollback_due(&self, now: Instant) -> Option<Uuid> {
        self.pending.as_ref().filter(|pending| (pending.topology_invalidated || now >= pending.deadline) && !pending.rollback_failure.as_ref().is_some_and(|(revision, _)| *revision == self.revision)).map(|pending| pending.token)
    }
    pub fn deadline(&self, token: Uuid) -> Option<Instant> { self.pending.as_ref().filter(|pending| pending.token == token && !pending.rollback_failure.as_ref().is_some_and(|(revision, _)| *revision == self.revision)).map(|pending| pending.deadline) }
    pub fn rollback_failed(&mut self, token: Uuid, message: String) {
        if !self.pending.as_ref().is_some_and(|pending| pending.token == token) { return; }
        self.changed();
        if let Some(pending) = &mut self.pending { pending.topology_invalidated = true; pending.rollback_failure = Some((self.revision, message)); }
    }
    pub fn pending_token(&self) -> Option<Uuid> { self.pending.as_ref().map(|pending| pending.token) }
    pub fn panel_closed(&self) -> bool { self.panel_closed }
    /// Logind's sole lid owner may apply this while locked; normal Settings transactions may not.
    pub fn prepare_panel(&mut self, expected: Revision, closed: bool, allow_headless: bool, session_active: bool) -> Result<PreparedPanel, DisplayError> {
        if !self.native { return Err(DisplayError::new("unsupported_backend", "Closed-panel policy requires physical native outputs")); }
        if expected != self.revision { return Err(DisplayError::new("stale_revision", "The compositor owner or output topology changed")); }
        if !session_active { return Err(DisplayError::new("session_unavailable", "Closed-panel policy requires the active native session")); }
        let before = self.profile();
        if closed && self.panel_open_profile.is_none() { self.panel_open_profile = Some(before.clone()); }
        let mut target = if closed { before.clone() } else { self.panel_open_profile.clone().unwrap_or_else(|| before.clone()) };
        target.outputs.retain(|output| self.inventory.iter().any(|display| display.connected && display.id == output.id));
        for display in self.inventory.iter().filter(|display| display.connected) {
            if !target.outputs.iter().any(|output| output.id == display.id) { target.outputs.push(display.current.clone()); }
        }
        if closed {
            target.allow_headless = allow_headless;
            for output in &mut target.outputs {
                let display = self.inventory.iter().find(|display| display.id == output.id).expect("connected profile inventory");
                if display.internal { output.enabled = false; output.primary = false; output.mirror_of = None; }
                else if !output.enabled && matches!(display.configure, Capability::Available { .. }) {
                    output.enabled = true;
                    if output.mode.is_none() { output.mode = display.modes.iter().find(|mode| mode.preferred).or_else(|| display.modes.first()).map(|mode| mode.id.clone()); }
                }
            }
        }
        repair_links(&mut target);
        display::validate_inventory(self.revision, &self.inventory, session_active, false, self.revision, &target)?;
        Ok(PreparedPanel { before, target, closed })
    }
    pub fn identify(&mut self, expected: Revision, outputs: Vec<DisplayId>, calibration: bool, now: Instant) -> Result<(), DisplayError> {
        if expected != self.revision { return Err(DisplayError::new("stale_revision", "The output owner or topology changed")); }
        if !self.native { return Err(DisplayError::new("unsupported_backend", "Identify requires physical outputs")); }
        if outputs.iter().any(|id| !self.inventory.iter().any(|display| &display.id == id && display.current.enabled)) { return Err(DisplayError::new("unknown_output", "Identify targets must be enabled physical displays")); }
        self.identify = (!outputs.is_empty()).then_some(IdentifyState { outputs, calibration, until: now + Duration::from_secs(if calibration { 60 } else { 5 }) });
        Ok(())
    }
    pub fn identification_for(&self, id: &DisplayId) -> Option<(usize, &str, bool)> {
        let identify = self.identify.as_ref().filter(|identify| identify.outputs.contains(id))?;
        self.inventory.iter().filter(|display| display.connected).enumerate().find(|(_, display)| &display.id == id).map(|(index, display)| (index + 1, display.connector.as_str(), identify.calibration))
    }
    pub fn output_id_for_alias(&self, alias: &str) -> Option<&DisplayId> {
        let mut matches = self.inventory.iter().filter(|display| display.connected && display.current.enabled && (display.id.0 == alias || display.connector == alias));
        let first = matches.next()?;
        matches.next().is_none().then_some(&first.id)
    }
    pub fn primary_id(&self) -> Option<&DisplayId> { self.inventory.iter().find(|display| display.connected && display.current.enabled && display.current.primary).map(|display| &display.id) }
    pub fn subscribe_activity(&mut self, sender: Sender<Option<ControlReply>>) {
        self.activity_subscribers.push(sender);
    }
    pub fn publish_activity(&mut self, mut observation: ActivityObservation) {
        let generation = self.activity.as_ref().map_or(0, |old| old.revision.generation);
        observation.revision = self.revision.at_generation(generation);
        if !self.activity_dirty && self.activity.as_ref() == Some(&observation) { return; }
        observation.revision.generation = generation.checked_add(1).expect("activity generation exhausted");
        self.activity_subscribers.retain(|sender| {
            if sender.is_closed() { return false; }
            sender.send_replace(Some(ControlReply { result: Ok(ControlResponse::Activity(observation.clone())) }));
            true
        });
        self.activity = Some(observation);
        self.activity_dirty = false;
    }
    pub fn activity_revision(&self) -> Revision { self.activity.as_ref().map_or(self.revision.at_generation(0), |activity| activity.revision) }
    pub fn activity_outputs(&self) -> Vec<templar_types::compositor_control::ActivityOutput> {
        self.inventory.iter().filter(|display| display.connected).map(|display| templar_types::compositor_control::ActivityOutput { id: display.id.clone(), enabled: display.current.enabled, internal: display.internal }).collect()
    }
    pub fn activity_is_current(&self, active: bool, locked: bool, lock_generation: Option<u64>, protected_covered: bool, idle: bool, blanked: bool, inhibited: bool, timeout_ms: Option<u64>, input_revision: Revision, activity_serial: u64) -> bool {
        !self.activity_dirty && self.activity.as_ref().is_some_and(|activity| activity.display_revision == self.revision && activity.input_revision == input_revision && activity.activity_serial == activity_serial && activity.session_active == active && activity.locked == locked && activity.lock_generation == lock_generation && activity.protected_covered == protected_covered && activity.idle == idle && activity.blanked == blanked && activity.inhibited == inhibited && activity.idle_timeout_ms == timeout_ms)
    }
    pub fn protection_is_current(&self, generation: Option<u64>, covered: bool) -> bool { self.activity.as_ref().is_some_and(|activity| activity.lock_generation == generation && activity.protected_covered == covered) }
    pub fn policy_epoch_changed(&mut self) { self.activity_dirty = true; }
    pub fn invalidate_preview(&mut self) {
        if let Some(pending) = &mut self.pending { pending.topology_invalidated = true; }
    }
}

fn repair_links(profile: &mut DisplayProfile) {
    for index in 0..profile.outputs.len() {
        let invalid = profile.outputs[index].mirror_of.as_ref().is_some_and(|source| !profile.outputs.iter().any(|output| &output.id == source && output.enabled && output.mirror_of.is_none()));
        if invalid { profile.outputs[index].mirror_of = None; }
    }
    let primary = profile.outputs.iter().find(|output| output.enabled && output.primary && output.mirror_of.is_none()).or_else(|| profile.outputs.iter().find(|output| output.enabled && output.mirror_of.is_none())).map(|output| output.id.clone());
    for output in &mut profile.outputs { output.primary = primary.as_ref() == Some(&output.id); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirmation_clock_starts_on_the_exact_presented_preview() {
        let now = Instant::now();
        let mut controller = DisplayController::default();
        let id = DisplayId("gpu/connector/edid".into());
        let token = Uuid::new_v4();
        controller.pending = Some(PendingPreview { token, before: DisplayProfile::default(), target: DisplayProfile::default(), awaiting: BTreeSet::from([id.clone()]), deadline: now + PRESENTATION_TIMEOUT, presented: false, topology_invalidated: false, purpose: PendingPurpose::Interactive, rollback_failure: None });
        controller.frame_presented(&id, Some(Uuid::new_v4()), now);
        assert_eq!(controller.deadline(token), Some(now + PRESENTATION_TIMEOUT));
        controller.frame_presented(&id, Some(token), now + Duration::from_secs(4));
        assert_eq!(controller.deadline(token), Some(now + Duration::from_secs(19)));
        assert_eq!(controller.rollback_due(now + Duration::from_secs(18)), None);
        assert_eq!(controller.rollback_due(now + Duration::from_secs(19)), Some(token));
    }
    #[test]
    fn operational_completion_requires_every_exact_owned_target_frame() {
        let now=Instant::now(); let token=Uuid::new_v4();
        let first=DisplayId("first".into()); let second=DisplayId("second".into());
        let mut controller=DisplayController::default();
        controller.pending=Some(PendingPreview {token,before:DisplayProfile::default(),target:DisplayProfile::default(),awaiting:BTreeSet::from([first.clone(),second.clone()]),deadline:now+PRESENTATION_TIMEOUT,presented:false,topology_invalidated:false,purpose:PendingPurpose::Operational {panel_closed:Some(true),reply:None},rollback_failure:None});
        controller.frame_presented(&first,Some(token),now);
        assert_eq!(controller.operational_ready(now),None);
        controller.frame_presented(&second,Some(Uuid::new_v4()),now);
        assert_eq!(controller.operational_ready(now),None);
        controller.frame_presented(&second,Some(token),now);
        assert_eq!(controller.operational_ready(now),Some(token));
    }
    #[test]
    fn failed_rollback_is_backed_off_only_for_its_actual_revision() {
        let now=Instant::now(); let token=Uuid::new_v4();
        let mut controller=DisplayController::default();
        controller.pending=Some(PendingPreview {token,before:DisplayProfile::default(),target:DisplayProfile::default(),awaiting:BTreeSet::new(),deadline:now,presented:false,topology_invalidated:true,purpose:PendingPurpose::Interactive,rollback_failure:None});
        controller.rollback_failed(token,"Actual ioctl/readback refusal".into());
        assert_eq!(controller.rollback_due(now),None);
        assert_eq!(controller.deadline(token),None);
        controller.changed();
        assert_eq!(controller.rollback_due(now),Some(token));
    }
}
