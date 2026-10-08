//! Per-source stored drafts are separate from the global native power owner's readback.
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::{fmt::Write, sync::Weak, time::Duration};
use templar_client::proxies::PowerProxy;
use templar_session_core::power::{LidAction, PanelPolicy, PowerFeature, PowerObservation, PowerProfile, PowerSource};
use templar_types::settings::{Capability, OperationError};

#[derive(Debug, Default)]
pub struct PowerState { pub snapshot: Option<PowerObservation>, pub error: Option<OperationError> }
#[derive(Debug)]
pub enum PowerCommand { Refresh, ProducerRestart, Set { source: PowerSource, profile: PowerProfile }, Reset(PowerSource) }
#[derive(Default)]
pub struct PowerController {
    connection: Option<zbus::Connection>,
    watchers: Vec<tokio::task::AbortHandle>,
    sender: Option<Weak<tokio::sync::mpsc::UnboundedSender<PowerCommand>>>,
}
impl Drop for PowerController { fn drop(&mut self) { for watcher in self.watchers.drain(..) { watcher.abort(); } } }
impl PowerController {
    pub async fn run(&mut self, command: PowerCommand) -> PowerState {
        if matches!(&command, PowerCommand::ProducerRestart) {
            for watcher in self.watchers.drain(..) { watcher.abort(); }
            self.connection = None;
            if let Some(sender) = self.sender.clone() && let Err(error) = self.subscribe(sender).await {
                return PowerState { error: Some(OperationError { code: "power-observer".into(), message: error.to_string() }), ..Default::default() };
            }
        }
        if self.connection.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => return PowerState { error: Some(OperationError { code: "session-unavailable".into(), message: error.to_string() }), ..Default::default() },
            }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation = async {
            let proxy = PowerProxy::new(connection).await?;
            match command {
                PowerCommand::Set { source, profile } => proxy.set_profile(source.as_str(), &serde_json::to_string(&profile)?).await?,
                PowerCommand::Reset(source) => proxy.reset_profile(source.as_str()).await?,
                PowerCommand::Refresh | PowerCommand::ProducerRestart => {}
            }
            Ok::<_, anyhow::Error>(())
        };
        let write = tokio::time::timeout(Duration::from_secs(50), operation).await;
        let read = tokio::time::timeout(Duration::from_secs(50), async {
            let proxy = PowerProxy::new(connection).await?;
            Ok::<PowerObservation, anyhow::Error>(serde_json::from_str(&proxy.inspect().await?)?)
        }).await;
        let mut errors = Vec::new();
        match write { Ok(Err(error)) => errors.push(format!("Policy change refused/incomplete: {error:#}")), Err(_) => errors.push("Policy wait expired; stored and observed values below are not confirmation of the request".into()), _ => {} }
        let snapshot = match read { Ok(Ok(snapshot)) => Some(snapshot), Ok(Err(error)) => { errors.push(format!("Power readback unavailable: {error:#}")); None }, Err(_) => { errors.push("Power readback timed out".into()); None } };
        PowerState { snapshot, error: (!errors.is_empty()).then(|| OperationError { code: "power-operation".into(), message: errors.join("; ") }) }
    }
    pub async fn subscribe(&mut self, sender: Weak<tokio::sync::mpsc::UnboundedSender<PowerCommand>>) -> Result<()> {
        if self.connection.is_none() { self.connection = Some(zbus::Connection::session().await?); }
        for watcher in self.watchers.drain(..) { watcher.abort(); }
        self.sender = Some(sender.clone());
        let connection = self.connection.as_ref().expect("connected above").clone();
        let (ready, registered) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let result = async {
                let proxy = PowerProxy::new(&connection).await?;
                let mut changed = proxy.receive_changed().await?;
                let bus = zbus::fdo::DBusProxy::new(&connection).await?;
                let mut owners = bus.receive_name_owner_changed().await?;
                if let Some(ready) = ready.take() { let _ = ready.send(Ok(())); }
                loop {
                    let command = tokio::select! {
                        change = changed.next() => { ensure!(change.is_some(), "power Changed stream ended"); PowerCommand::Refresh }
                        owner = owners.next() => {
                            let owner = owner.context("session owner observation ended")?;
                            if owner.args()?.name().as_str() != "org.templaros.Session" { continue; }
                            PowerCommand::ProducerRestart
                        }
                    };
                    let Some(sender) = sender.upgrade() else { break; };
                    if sender.send(command).is_err() { break; }
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            if let Err(error) = result {
                if let Some(ready) = ready.take() { let _ = ready.send(Err(error)); }
                else if let Some(sender) = sender.upgrade() { let _ = sender.send(PowerCommand::ProducerRestart); }
            }
        });
        self.watchers.push(task.abort_handle());
        match tokio::time::timeout(Duration::from_secs(10), registered).await {
            Ok(Ok(Ok(()))) => Ok(()),
            result => {
                for watcher in self.watchers.drain(..) { watcher.abort(); }
                match result { Ok(Ok(Err(error))) => Err(error), Ok(Err(error)) => Err(error.into()), Err(_) => anyhow::bail!("power observer registration timed out"), Ok(Ok(Ok(()))) => unreachable!() }
            }
        }
    }
}

pub async fn print() -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = PowerProxy::new(&connection).await?;
    let observed: PowerObservation = serde_json::from_str(&proxy.inspect().await?)?;
    println!("{}", serde_json::to_string_pretty(&observed)?);
    Ok(())
}

const LID_ACTIONS: [LidAction; 4] = [LidAction::Ignore, LidAction::Lock, LidAction::Suspend, LidAction::Hibernate];
const LID_LABELS: [&str; 4] = ["Keep running", "Lock", "Suspend (requires coverage)", "Hibernate (requires capability + coverage)"];
const PANEL_POLICIES: [PanelPolicy; 3] = [PanelPolicy::Keep, PanelPolicy::ExternalOnly, PanelPolicy::AllowHeadless];
const PANEL_LABELS: [&str; 3] = ["Keep current outputs", "External only; never disable last output", "Explicitly allow headless closed panel"];

pub struct PowerView {
    pub state: PowerState,
    source: PowerSource,
    intervals: [pixelkit_ui::TextFieldState; 4],
    lid: [LidAction; 2],
    panel: [PanelPolicy; 2],
    dirty: bool,
    draft_loaded: bool,
    local_error: Option<String>,
    fields_drawn: [bool; 4],
    chooser: pixelkit_ui::Dropdown,
    chooser_row: Option<usize>,
    scroll: pixelkit_ui::ScrollState,
    summary: String,
    stored_label: String,
    features: Vec<String>,
    errors: String,
}
impl Default for PowerView {
    fn default() -> Self {
        Self { state: PowerState::default(), source: PowerSource::Mains, intervals: std::array::from_fn(|_| Default::default()),
            lid: [LidAction::Suspend, LidAction::Ignore], panel: [PanelPolicy::Keep; 2], dirty: false, draft_loaded: false,
            local_error: None, fields_drawn: [false; 4], chooser: pixelkit_ui::Dropdown::new(), chooser_row: None,
            scroll: Default::default(), summary: String::new(), stored_label: String::new(), features: Vec::new(), errors: String::new() }
    }
}
impl PowerView {
    pub fn leave(&mut self) { self.fields_drawn = [false; 4]; self.chooser = pixelkit_ui::Dropdown::new(); self.chooser_row = None; }
    pub fn ime_caret(&self) -> Option<pixelkit_raster::Rect> {
        self.intervals.iter().zip(self.fields_drawn).find_map(|(field, drawn)| drawn.then(|| field.caret_area()).flatten())
    }
    fn stored(&self) -> Option<&PowerProfile> {
        self.state.snapshot.as_ref().and_then(|state| match self.source { PowerSource::Battery => state.battery.as_ref(), PowerSource::Mains => state.mains.as_ref() })
    }
    fn load_draft(&mut self) {
        let Some(profile) = self.stored() else { self.draft_loaded = false; return; };
        let intervals = [profile.dim_after, profile.screen_off_after, profile.lock_after, profile.suspend_after];
        let lid = [profile.lid_closed, profile.lid_closed_docked];
        let panel = [profile.lid_panel, profile.lid_panel_docked];
        self.lid = lid;
        self.panel = panel;
        for (field, interval) in self.intervals.iter_mut().zip(intervals) { field.set_text(render_interval(interval)); }
        self.dirty = false; self.draft_loaded = true; self.local_error = None;
    }
    fn draft(&self) -> Result<PowerProfile> {
        ensure!(self.draft_loaded, "stored profile unavailable; reset this source to its distribution default first");
        Ok(PowerProfile { dim_after: parse_interval(self.intervals[0].text())?, screen_off_after: parse_interval(self.intervals[1].text())?,
            lock_after: parse_interval(self.intervals[2].text())?, suspend_after: parse_interval(self.intervals[3].text())?,
            lid_closed: self.lid[0], lid_closed_docked: self.lid[1], lid_panel: self.panel[0], lid_panel_docked: self.panel[1] })
    }
    pub fn replace(&mut self, state: PowerState) -> bool {
        let owner_changed = match (&self.state.snapshot, &state.snapshot) {
            (Some(old), Some(new)) => old.revision.owner_epoch != new.revision.owner_epoch, (None, None) => false, _ => true,
        };
        let stored_changed = match (&self.state.snapshot, &state.snapshot) {
            (Some(old), Some(new)) => match self.source { PowerSource::Battery => old.battery != new.battery, PowerSource::Mains => old.mains != new.mains },
            _ => true,
        };
        self.state = state;
        if owner_changed { self.leave(); self.dirty = false; }
        if owner_changed || !self.draft_loaded || (stored_changed && !self.dirty) { self.load_draft(); }
        self.refresh_labels();
        owner_changed
    }
    fn refresh_labels(&mut self) {
        self.summary.clear(); self.stored_label.clear(); self.errors.clear();
        let _ = write!(self.stored_label, "Editing stored {} policy{}", self.source.as_str(), if self.dirty { " (unapplied draft)" } else { "" });
        let Some(state) = &self.state.snapshot else {
            self.summary.push_str("Power owner readback unavailable; no applied policy is confirmed"); self.features.clear(); return;
        };
        let _ = write!(self.summary, "Selected source: {} · active={:?} · inhibited={:?} · covered={:?}", state.selected_source.map_or("unavailable", PowerSource::as_str), state.session_active, state.inhibited, state.protected_covered);
        self.features.resize_with(6, String::new);
        for (label, (name, feature)) in self.features.iter_mut().zip([
            ("Native screen-off", &state.screen_off), ("Backlight idle dim", &state.dim), ("Idle lock", &state.lock),
            ("Idle suspend", &state.suspend), ("Hibernate capability (lid)", &state.hibernate), ("Scoped lid handling", &state.lid)]) {
            feature_label(label, name, feature);
        }
        if state.screen_off_observed { let _ = write!(self.features[0], " · observed timeout={:?}ms", state.screen_off_after_ms); }
        let _ = write!(self.features[5], " · lid={:?} docked={:?} · owned={}", state.lid_closed, state.docked, state.lid_inhibitor_owned);
        if let Some(percent) = state.dimmed_percent { let _ = write!(self.features[1], " · observed dim target={percent}%"); }
        let _ = write!(self.features[3], " · sleep-delay FD owned={}", state.sleep_delay_owned);
        for error in &state.errors { if !self.errors.is_empty() { self.errors.push_str("; "); } self.errors.push_str(error); }
        if let Some(pending) = &state.sleep_pending { let _ = write!(self.errors, " · pending {pending}; not confirmation of sleep"); }
    }
    pub fn draw(&mut self, ui: &mut pixelkit_ui::Ui<'_>, area: pixelkit_raster::Rect) -> (Option<PowerCommand>, bool) {
        use pixelkit_raster::Rect;
        use pixelkit_text::Align;
        let h = ui.px(32); let gap = ui.px(8); let mut command = None;
        let mut labels_changed = false;
        let mut scroll = std::mem::take(&mut self.scroll);
        self.fields_drawn = [false; 4];
        let rows = [ui.px(42), ui.px(42), ui.px(48), ui.px(32), ui.px(32), ui.px(32), ui.px(32), ui.px(42), ui.px(42), ui.px(42), ui.px(42), ui.px(56), ui.px(42), ui.px(42), ui.px(42), ui.px(42), ui.px(42), ui.px(42), ui.px(96)];
        ui.scroll_list(&mut scroll, area, &rows, |ui, index, row| {
            match index {
                0 => {
                    if ui.button(Rect::new(row.x, row.y, ui.px(88), h), "Refresh") { command = Some(PowerCommand::Refresh); }
                    let old_source = self.source;
                    if ui.button(Rect::new(row.x + ui.px(100), row.y, ui.px(100), h), "Battery") { self.source = PowerSource::Battery; }
                    if ui.button(Rect::new(row.x + ui.px(210), row.y, ui.px(100), h), "Mains") { self.source = PowerSource::Mains; }
                    if old_source != self.source { self.dirty = false; self.load_draft(); self.refresh_labels(); }
                }
                1 => ui.label(Rect::new(row.x, row.y, row.w, h), &self.stored_label, ui.theme.text, Align::Left),
                2 => ui.label(Rect::new(row.x, row.y, row.w, row.h), &self.summary, ui.theme.text_dim, Align::Left),
                3..=6 => {
                    let field = index - 3;
                    let name = ["Dim after (seconds)", "Screen off (seconds)", "Lock after (seconds)", "Suspend after (seconds)"][field];
                    ui.label(Rect::new(row.x, row.y, ui.px(205), h), name, ui.theme.text, Align::Left);
                    let response = ui.text_field(&mut self.intervals[field], Rect::new(row.x + ui.px(215), row.y, (row.w - ui.px(220)).max(0), h), "Never, or seconds[.fraction]", false, false);
                    self.fields_drawn[field] = true;
                    if response { self.dirty = true; labels_changed = true; self.local_error = None; }
                }
                7..=10 => {
                    let choice = index - 7;
                    let name = ["Lid: undocked", "Lid: docked", "Awake panel: undocked", "Awake panel: docked"][choice];
                    ui.label(Rect::new(row.x, row.y, ui.px(205), h), name, ui.theme.text, Align::Left);
                    let rect = Rect::new(row.x + ui.px(215), row.y, (row.w - ui.px(220)).max(0), h);
                    let opened = if choice < 2 { ui.dropdown(&mut self.chooser, rect, &LID_LABELS, LID_ACTIONS.iter().position(|action| *action == self.lid[choice]).unwrap_or(0)) }
                        else { ui.dropdown(&mut self.chooser, rect, &PANEL_LABELS, PANEL_POLICIES.iter().position(|policy| *policy == self.panel[choice - 2]).unwrap_or(0)) };
                    if opened { self.chooser_row = self.chooser.is_open().then_some(choice); }
                }
                11 => {
                    if ui.button(Rect::new(row.x, row.y, ui.px(155), h), "Store policy") {
                        match self.draft() { Ok(profile) => { command = Some(PowerCommand::Set { source: self.source, profile }); self.dirty = false; labels_changed = true; self.local_error = None; }, Err(error) => self.local_error = Some(error.to_string()) }
                    }
                    if ui.button(Rect::new(row.x + ui.px(165), row.y, ui.px(180), h), "Reset stored source") { self.dirty = false; labels_changed = true; self.local_error = None; command = Some(PowerCommand::Reset(self.source)); }
                    ui.label(Rect::new(row.x, row.y + h, row.w, ui.px(22)), "Panel choices apply only to Keep running / Lock. Stored is not applied.", ui.theme.text_dim, Align::Left);
                }
                12..=17 => if let Some(label) = self.features.get(index - 12) { ui.label(Rect::new(row.x, row.y, row.w, row.h - gap), label, ui.theme.text_dim, Align::Left); },
                18 => {
                    let error = self.local_error.as_deref().or_else(|| self.state.error.as_ref().map(|error| error.message.as_str())).unwrap_or(&self.errors);
                    ui.label(Rect::new(row.x, row.y, row.w, row.h), error, ui.theme.text_dim, Align::Left);
                }
                _ => {}
            }
        });
        self.scroll = scroll;
        if let Some(picked) = self.chooser.draw(ui, area) && let Some(row) = self.chooser_row.take() {
            if row < 2 { if let Some(action) = LID_ACTIONS.get(picked) { self.lid[row] = *action; self.dirty = true; labels_changed = true; } }
            else if let Some(panel) = PANEL_POLICIES.get(picked) { self.panel[row - 2] = *panel; self.dirty = true; labels_changed = true; }
        }
        if labels_changed { self.refresh_labels(); }
        (command, false)
    }
}
fn feature_label(label: &mut String, name: &str, feature: &PowerFeature) {
    label.clear(); let _ = write!(label, "{name}: ");
    match &feature.capability { Capability::Available { .. } => { let _ = write!(label, "available · armed={}", feature.armed); },
        Capability::Unavailable { reason } => { let _ = write!(label, "unavailable: {reason}"); }, Capability::Unsupported { reason } => { let _ = write!(label, "unsupported: {reason}"); } }
    if let Some(error) = &feature.error { let _ = write!(label, " · refused/unconfirmed: {error}"); }
}
fn render_interval(interval: Option<Duration>) -> String {
    match interval {
        None => "Never".into(),
        Some(interval) if interval.subsec_nanos() == 0 => interval.as_secs().to_string(),
        Some(interval) => { let mut text = format!("{}.{:09}", interval.as_secs(), interval.subsec_nanos()); while text.ends_with('0') { text.pop(); } text }
    }
}
fn parse_interval(text: &str) -> Result<Option<Duration>> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("never") { return Ok(None); }
    let (seconds, fraction) = text.split_once('.').map_or((text, None), |(seconds, fraction)| (seconds, Some(fraction)));
    ensure!(!seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit()), "interval must be Never or nonnegative seconds");
    let seconds: u64 = seconds.parse().context("seconds exceed the supported range")?;
    let nanos = if let Some(fraction) = fraction {
        ensure!(!fraction.is_empty() && fraction.len() <= 9 && fraction.bytes().all(|byte| byte.is_ascii_digit()), "fraction must contain 1..9 decimal digits");
        fraction.parse::<u32>()? * 10u32.pow(9 - fraction.len() as u32)
    } else { 0 };
    let interval = Duration::new(seconds, nanos);
    ensure!(u64::try_from(interval.as_millis()).is_ok(), "interval exceeds the native millisecond range");
    Ok(Some(interval))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_intervals_round_trip_and_bad_drafts_refuse() {
        for interval in [None, Some(Duration::ZERO), Some(Duration::from_secs(120)), Some(Duration::new(3, 123_456_789))] {
            assert_eq!(parse_interval(&render_interval(interval)).unwrap(), interval);
        }
        for text in ["-1", "NaN", "1e9", "1.1234567890", "1.", "18446744073709551615"] { assert!(parse_interval(text).is_err(), "{text}"); }
    }
    #[test]
    fn leaving_does_not_discard_actual_runtime_errors() {
        let mut view = PowerView::default();
        view.state.error = Some(OperationError { code: "power-operation".into(), message: "coverage unconfirmed".into() });
        view.leave();
        assert_eq!(view.state.error.as_ref().unwrap().message, "coverage unconfirmed");
        assert!(view.ime_caret().is_none());
    }
    fn observed() -> PowerObservation {
        let feature = || PowerFeature::available(true);
        PowerObservation { revision: templar_types::settings::Revision::new_owner(),
            battery: Some(PowerProfile::default_for(PowerSource::Battery)), mains: Some(PowerProfile::default_for(PowerSource::Mains)),
            selected_source: Some(PowerSource::Mains), selected_profile: Some(PowerProfile::default_for(PowerSource::Mains)),
            session_active: Some(true), inhibited: Some(false), protected_covered: Some(false), screen_off_after_ms: Some(900_000), screen_off_observed: true,
            screen_off: feature(), dim: feature(), lock: feature(), suspend: feature(), hibernate: feature(), lid: feature(),
            lid_present: Some(true), lid_closed: Some(false), docked: Some(false), lid_inhibitor_owned: true, sleep_delay_owned: true,
            dimmed_percent: None, sleep_pending: None, errors: Vec::new() }
    }
    #[test]
    fn ordinary_status_changes_preserve_draft_and_structural_focus() {
        let mut view = PowerView::default();
        let mut snapshot = observed();
        assert!(view.replace(PowerState { snapshot: Some(snapshot.clone()), error: None }));
        view.intervals[2].set_text("42.25"); view.dirty = true;
        snapshot.revision.generation += 1; snapshot.protected_covered = Some(true);
        assert!(!view.replace(PowerState { snapshot: Some(snapshot.clone()), error: None }));
        assert_eq!(view.intervals[2].text(), "42.25");
        snapshot.revision = templar_types::settings::Revision::new_owner();
        assert!(view.replace(PowerState { snapshot: Some(snapshot), error: None }));
        assert_eq!(view.intervals[2].text(), "900");
    }
}
