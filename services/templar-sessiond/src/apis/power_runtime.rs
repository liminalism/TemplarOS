//! One serialized owner. Timers are policy deadlines derived from native samples, never an input poll.
use super::{PowerApi, PATH, read_profile, store_profile};
use crate::apis::{backlight::BacklightState, device_prompt::authority_owner, lifecycle::{SessionEdge, SessionEdges}};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream::SelectAll};
use std::{sync::Arc, time::Duration};
use templar_client::{compositor_control::{ActivitySubscription, CompositorControl}, proxies::BacklightProxy};
use templar_session_core::power::{LidAction, PanelPolicy, PowerFeature, PowerObservation, PowerProfile, PowerSource, deadline_after, protected_sleep_ready};
use templar_state::Store;
use templar_types::{compositor_control::{ActivityObservation, ControlRequest, ControlResponse}, display::DisplayObservation, settings::{Capability, Revision}};
use tokio::{sync::{mpsc, oneshot, broadcast}, time::Instant};
use zbus::{MessageStream, names::OwnedUniqueName, zvariant::{OwnedFd, OwnedValue}};

const LOGIN: &str = "org.freedesktop.login1";
const LOGIN_PATH: &str = "/org/freedesktop/login1";
const LOGIN_IFACE: &str = "org.freedesktop.login1.Manager";
const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const CALL_DEADLINE: Duration = Duration::from_secs(3);

pub(super) enum Command {
    Change { source: PowerSource, profile: Option<PowerProfile>, reply: oneshot::Sender<Result<()>> },
    Inspect(oneshot::Sender<Result<String>>),
}
struct Dimmed { device: String, max_raw: u64, own_raw: u64, before_percent: u32 }
#[derive(Clone, Copy, PartialEq, Eq)]
enum SleepKind { Suspend, Hibernate, External }
impl SleepKind { fn label(self) -> &'static str { match self { Self::Suspend => "suspend", Self::Hibernate => "hibernate", Self::External => "external sleep" } } }
struct PendingSleep { kind: SleepKind, epoch: uuid::Uuid, serial: u64, deadline: Instant, expired: bool }
struct Actor {
    store: Store,
    session: zbus::Connection,
    system: Option<zbus::Connection>,
    commands: mpsc::Receiver<Command>,
    edges: broadcast::Receiver<SessionEdge>,
    sleep_ready: tokio::sync::watch::Receiver<bool>,
    sleep_watch_alive: bool,
    signals: SelectAll<MessageStream>,
    session_signals: SelectAll<MessageStream>,
    control: Option<CompositorControl>,
    activity_stream: Option<ActivitySubscription>,
    activity: Option<ActivityObservation>,
    observed_at: Instant,
    status: PowerObservation,
    login_owner: Option<OwnedUniqueName>,
    upower_owner: Option<OwnedUniqueName>,
    lid_inhibitor: Option<OwnedFd>,
    sleep_delay: Option<OwnedFd>,
    delay_max: Duration,
    dimmed: Option<Dimmed>,
    restore_requested: bool,
    pending_sleep: Option<PendingSleep>,
    dim_done: bool,
    lock_done: bool,
    suspend_done: bool,
    policy_dirty: bool,
    lid_applied: Option<(bool, bool, LidAction, PanelPolicy)>,
    panel_owned: bool,
    sleeping: bool,
}

pub(super) async fn start(store: Store, session: zbus::Connection, system: Option<zbus::Connection>, edges: SessionEdges, sleep_ready: tokio::sync::watch::Receiver<bool>) -> (Arc<mpsc::Sender<Command>>, tokio::task::AbortHandle) {
    let (sender, commands) = mpsc::channel(16);
    let edges = edges.subscribe(); // Before acquiring any delay inhibitor or reading sleep state.
    let (ready, registered) = oneshot::channel();
    let unavailable = || PowerFeature::unavailable("power owner has not observed its producers yet");
    let mut actor = Actor { store, session, system, commands, edges, sleep_ready, sleep_watch_alive: true,
        signals: SelectAll::new(), session_signals: SelectAll::new(), control: None, activity_stream: None, activity: None,
        observed_at: Instant::now(), status: PowerObservation { revision: Revision::new_owner(), battery: None, mains: None,
            selected_source: None, selected_profile: None, session_active: None, inhibited: None, protected_covered: None,
            screen_off_after_ms: None, screen_off_observed: false, screen_off: unavailable(), dim: unavailable(), lock: unavailable(),
            suspend: unavailable(), hibernate: unavailable(), lid: unavailable(), lid_present: None, lid_closed: None, docked: None,
            lid_inhibitor_owned: false, sleep_delay_owned: false, dimmed_percent: None, sleep_pending: None, errors: Vec::new() },
        login_owner: None, upower_owner: None, lid_inhibitor: None, sleep_delay: None, delay_max: Duration::ZERO,
        dimmed: None, restore_requested: false, pending_sleep: None, dim_done: false, lock_done: false, suspend_done: false,
        policy_dirty: true, lid_applied: None, panel_owned: false, sleeping: false };
    let task = tokio::spawn(async move {
        actor.load_profiles();
        if let Err(error) = actor.install_matches().await { actor.error(format!("power producer subscription failed: {error:#}")); }
        actor.reconcile().await;
        actor.changed().await;
        let _ = ready.send(());
        actor.run().await;
    });
    let handle = task.abort_handle();
    // Calls and subscription handshakes themselves have strict deadlines. A late startup
    // remains observable rather than making the API disappear with the host's startup race.
    let _ = tokio::time::timeout(Duration::from_secs(45), registered).await;
    (Arc::new(sender), handle)
}
fn available(capability: &Capability) -> bool { matches!(capability, Capability::Available { .. }) }
fn duration_ms(duration: Option<Duration>) -> Option<u64> { duration.map(|duration| duration.as_millis() as u64) }

impl Actor {
    fn sleep_observed(&self) -> bool { self.sleep_watch_alive && self.sleep_ready.has_changed().is_ok() && *self.sleep_ready.borrow() }
    fn error(&mut self, error: String) {
        if !self.status.errors.contains(&error) {
            if self.status.errors.len() == 16 { self.status.errors.remove(0); }
            self.status.errors.push(error);
        }
    }
    fn load_profiles(&mut self) {
        self.status.battery = match read_profile(&self.store, PowerSource::Battery) { Ok(profile) => Some(profile), Err(error) => { self.error(format!("battery profile unavailable: {error:#}")); None } };
        self.status.mains = match read_profile(&self.store, PowerSource::Mains) { Ok(profile) => Some(profile), Err(error) => { self.error(format!("mains profile unavailable: {error:#}")); None } };
    }
    fn select_profile(&mut self) {
        let profile = match self.status.selected_source { Some(PowerSource::Battery) => &self.status.battery, Some(PowerSource::Mains) => &self.status.mains, None => &None };
        if &self.status.selected_profile != profile { self.status.selected_profile.clone_from(profile); self.policy_dirty = true; }
    }
    fn update_owned_status(&mut self) {
        self.status.lid_inhibitor_owned = self.lid_inhibitor.is_some();
        self.status.sleep_delay_owned = self.sleep_delay.is_some();
        let pending = self.pending_sleep.as_ref().map(|sleep| sleep.kind.label());
        if self.status.sleep_pending.as_deref() != pending { self.status.sleep_pending = pending.map(str::to_owned); }
    }
    async fn changed(&mut self) {
        self.status.revision.generation = self.status.revision.generation.saturating_add(1);
        self.update_owned_status();
        if let Ok(json) = serde_json::to_string(&self.status.revision)
            && let Ok(emitter) = zbus::object_server::SignalEmitter::new(&self.session, PATH) {
            let _ = PowerApi::changed(&emitter, &json).await;
        }
    }
    async fn install_matches(&mut self) -> Result<()> {
        if self.signals.is_empty() && let Some(system) = &self.system {
            let mut installed = Vec::new();
            for (service, path) in [(UPOWER, UPOWER_PATH), (LOGIN, LOGIN_PATH)] {
                let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(service)?.path(path)?.interface("org.freedesktop.DBus.Properties")?.build();
                installed.push(tokio::time::timeout(CALL_DEADLINE, MessageStream::for_match_rule(rule, system, Some(16))).await??);
            }
            let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender("org.freedesktop.DBus")?.interface("org.freedesktop.DBus")?.member("NameOwnerChanged")?.build();
            installed.push(tokio::time::timeout(CALL_DEADLINE, MessageStream::for_match_rule(rule, system, Some(32))).await??);
            self.signals.extend(installed);
        }
        if self.session_signals.is_empty() {
            let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender("org.freedesktop.systemd1")?.interface("org.freedesktop.DBus.Properties")?.build();
            self.session_signals.push(tokio::time::timeout(CALL_DEADLINE, MessageStream::for_match_rule(rule, &self.session, Some(16))).await??);
        }
        Ok(())
    }
    async fn login_proxy(&self) -> Result<zbus::Proxy<'static>> {
        let system = self.system.as_ref().context("system D-Bus unavailable")?;
        let owner = self.login_owner.clone().context("logind producer unavailable")?;
        Ok(zbus::Proxy::new(system, owner, LOGIN_PATH, LOGIN_IFACE).await?)
    }
    async fn observe_system(&mut self) -> Result<()> {
        ensure!(!self.signals.is_empty(), "power producer signal matches are not installed");
        let system = self.system.as_ref().context("system D-Bus unavailable; automatic power and lid handling disabled")?;
        let login = tokio::time::timeout(CALL_DEADLINE, authority_owner(system, LOGIN)).await??;
        let upower = tokio::time::timeout(CALL_DEADLINE, authority_owner(system, UPOWER)).await??;
        if self.login_owner != login {
            self.lid_inhibitor = None; self.sleep_delay = None; self.pending_sleep = None;
            self.lid_applied = None; self.policy_dirty = true;
        }
        if self.upower_owner != upower { self.lid_inhibitor = None; self.lid_applied = None; self.policy_dirty = true; }
        self.login_owner = login; self.upower_owner = upower;
        let manager = self.login_proxy().await?;
        let (suspend, hibernate, delay): (String, String, u64) = tokio::try_join!(
            async { Ok::<_, anyhow::Error>(tokio::time::timeout(CALL_DEADLINE, manager.call("CanSuspend", &())).await??) },
            async { Ok::<_, anyhow::Error>(tokio::time::timeout(CALL_DEADLINE, manager.call("CanHibernate", &())).await??) },
            async { Ok::<_, anyhow::Error>(tokio::time::timeout(CALL_DEADLINE, manager.get_property("InhibitDelayMaxUSec")).await??) },
        )?;
        drop(manager);
        self.delay_max = Duration::from_micros(delay);
        let suspend_error = self.status.suspend.error.take();
        let hibernate_error = self.status.hibernate.error.take();
        self.status.suspend = match suspend.as_str() {
            "yes" => PowerFeature::available(!self.policy_dirty && self.status.selected_profile.as_ref().is_some_and(|profile| profile.suspend_after.is_some())),
            "na" => PowerFeature::unsupported("logind reports suspension is not applicable on this machine"),
            _ => PowerFeature::unavailable(format!("logind CanSuspend={suspend}; automatic suspension requires noninteractive authorization")),
        };
        self.status.hibernate = match hibernate.as_str() {
            "yes" => PowerFeature::available(false),
            "na" => PowerFeature::unsupported("logind reports hibernation is not applicable on this machine"),
            _ => PowerFeature::unavailable(format!("logind CanHibernate={hibernate}; hibernation is not assumed supported")),
        };
        self.status.suspend.error = suspend_error; self.status.hibernate.error = hibernate_error;
        if !self.sleep_observed() {
            self.status.suspend = PowerFeature::unavailable("sole PrepareForSleep observer is not ready; automatic sleep disabled");
            self.status.hibernate = PowerFeature::unavailable("sole PrepareForSleep observer is not ready; automatic hibernation disabled");
        }
        if let Err(error) = self.observe_source().await {
            self.source_unavailable(format!("power-source readback unavailable; automatic profiles/lid policy disabled: {error:#}"));
        }
        Ok(())
    }
    async fn observe_source(&mut self) -> Result<()> {
        let system = self.system.as_ref().context("system D-Bus unavailable")?;
        let owner = self.upower_owner.clone().context("UPower is unavailable; install/start the declared power-state producer")?;
        let properties = zbus::Proxy::new(system, owner, UPOWER_PATH, "org.freedesktop.DBus.Properties").await?;
        let fields: std::collections::HashMap<String, OwnedValue> = tokio::time::timeout(CALL_DEADLINE, properties.call("GetAll", &(UPOWER,))).await??;
        let boolean = |key: &str| -> Result<bool> { Ok(bool::try_from(fields.get(key).with_context(|| format!("UPower omitted {key}"))?)?) };
        let source = if boolean("OnBattery")? { PowerSource::Battery } else { PowerSource::Mains };
        let lid_present = boolean("LidIsPresent")?;
        let lid_closed = boolean("LidIsClosed")?;
        drop(properties);
        if self.status.selected_source != Some(source) { self.status.selected_source = Some(source); self.policy_dirty = true; }
        self.status.lid_present = Some(lid_present); self.status.lid_closed = Some(lid_closed);
        self.select_profile();
        Ok(())
    }
    fn source_unavailable(&mut self, reason: String) {
        self.lid_inhibitor = None; self.lid_applied = None;
        if self.pending_sleep.as_ref().is_some_and(|sleep| sleep.kind != SleepKind::External) { self.pending_sleep = None; }
        self.status.selected_source = None; self.status.selected_profile = None; self.policy_dirty = true;
        self.status.lid_present = None; self.status.lid_closed = None;
        self.status.lid = PowerFeature::unavailable(&reason);
        self.status.suspend.armed = false; self.status.hibernate.armed = false;
        // UPower supplies profile/lid inputs, not the native lock guard. Keep a healthy
        // login1/native/SleepWatch delay inhibitor for external PrepareForSleep protection.
        self.error(reason);
    }
    fn system_unavailable(&mut self, reason: String) {
        self.sleep_delay = None; self.pending_sleep = None; self.delay_max = Duration::ZERO;
        self.status.suspend = PowerFeature::unavailable(&reason); self.status.hibernate = PowerFeature::unavailable(&reason);
        self.source_unavailable(reason);
    }
    async fn native_sample(&self) -> Result<ActivityObservation> {
        let control = self.control.as_ref().context("compositor owner transport unavailable")?;
        let response = control.request(ControlRequest::Activity).await?;
        match response { ControlResponse::Activity(activity) => Ok(activity), _ => anyhow::bail!("compositor returned incompatible activity readback") }
    }
    fn observe_activity(&mut self, activity: ActivityObservation) -> bool {
        if let Some(old) = &self.activity
            && old.revision.owner_epoch == activity.revision.owner_epoch && old.revision.generation > activity.revision.generation { return false; }
        let reset = self.activity.as_ref().is_none_or(|old| old.revision.owner_epoch != activity.revision.owner_epoch
            || old.activity_serial != activity.activity_serial || old.session_active != activity.session_active || old.inhibited != activity.inhibited);
        let owner_changed = self.activity.as_ref().is_none_or(|old| old.revision.owner_epoch != activity.revision.owner_epoch);
        let active_changed = self.activity.as_ref().is_some_and(|old| old.session_active != activity.session_active);
        let visible = self.status.session_active != Some(activity.session_active) || self.status.inhibited != Some(activity.inhibited)
            || self.status.protected_covered != Some(activity.protected_covered) || self.status.screen_off_after_ms != activity.idle_timeout_ms;
        if reset {
            self.restore_requested |= self.dimmed.is_some();
            self.dim_done = false; self.lock_done = false; self.suspend_done = false;
            if self.pending_sleep.as_ref().is_some_and(|sleep| sleep.kind != SleepKind::External) { self.pending_sleep = None; }
        }
        if owner_changed || active_changed { self.policy_dirty = true; self.lid_applied = None; }
        let armed = !self.policy_dirty && activity.session_active && !activity.inhibited;
        if let Some(profile) = &self.status.selected_profile {
            self.status.dim.armed = armed && available(&self.status.dim.capability) && profile.dim_after.is_some();
            self.status.lock.armed = armed && available(&self.status.lock.capability) && profile.lock_after.is_some();
            self.status.suspend.armed = armed && available(&self.status.suspend.capability) && profile.suspend_after.is_some();
        }
        self.status.session_active = Some(activity.session_active); self.status.inhibited = Some(activity.inhibited);
        self.status.protected_covered = Some(activity.protected_covered);
        self.status.screen_off_after_ms = activity.idle_timeout_ms; self.status.screen_off_observed = true;
        self.observed_at = Instant::now(); self.activity = Some(activity);
        owner_changed || visible
    }
    async fn connect_native(&mut self) -> Result<()> {
        if self.control.is_none() { self.control = Some(CompositorControl::from_runtime()?); }
        if self.activity_stream.is_none() {
            let mut stream = self.control.as_ref().expect("created above").subscribe_activity().await?;
            let initial = stream.next().await.context("native activity subscription ended before its initial sample")??;
            self.activity_stream = Some(stream);
            self.observe_activity(initial);
        }
        Ok(())
    }
    async fn displays(&self) -> Result<DisplayObservation> {
        match self.control.as_ref().context("native display owner unavailable")?.request(ControlRequest::Displays).await? {
            ControlResponse::Displays(displays) => { ensure!(displays.session_active, "native display session is inactive"); Ok(displays) },
            _ => anyhow::bail!("compositor returned incompatible display readback"),
        }
    }
    async fn brightness(&self) -> Result<BacklightState> {
        let destination = self.session.unique_name().context("session owner connection has no unique name")?;
        let proxy = BacklightProxy::builder(&self.session).destination(destination.as_str())?.build().await?;
        Ok(serde_json::from_str(&tokio::time::timeout(CALL_DEADLINE, proxy.get()).await??)?)
    }
    async fn set_brightness(&self, percent: u32) -> Result<()> {
        let destination = self.session.unique_name().context("session owner connection has no unique name")?;
        let proxy = BacklightProxy::builder(&self.session).destination(destination.as_str())?.build().await?;
        tokio::time::timeout(CALL_DEADLINE, proxy.set_brightness(percent)).await??;
        Ok(())
    }
    async fn restore_dim(&mut self) -> Result<()> {
        let Some(dimmed) = self.dimmed.take() else { return Ok(()); };
        let current = match self.brightness().await { Ok(current) => current, Err(error) => { self.dimmed = Some(dimmed); return Err(error); } };
        if current.present && current.device == dimmed.device && current.max_raw == dimmed.max_raw && current.raw == dimmed.own_raw {
            if let Err(error) = self.set_brightness(dimmed.before_percent).await { self.dimmed = Some(dimmed); return Err(error); }
            let restored = self.brightness().await?;
            ensure!(restored.present && restored.device == dimmed.device && restored.percent == dimmed.before_percent, "backlight restore was not confirmed by actual readback");
        }
        // A different actual raw value belongs to the external writer; do not overwrite it.
        self.status.dimmed_percent = None;
        Ok(())
    }
    async fn dim(&mut self) -> Result<()> {
        let before = self.brightness().await?;
        ensure!(before.present, "this machine has no display backlight");
        let wanted = (before.percent / 2).max(1);
        let own_raw = u64::from(u32::try_from(crate::apis::backlight::raw_of(wanted, before.max_raw)).unwrap_or(u32::MAX));
        if own_raw >= before.raw { return Ok(()); }
        // Track the exact owner's issued target before awaiting the reply/readback.
        // Transport uncertainty must not discard later conditional restore ownership.
        self.dimmed = Some(Dimmed { device: before.device, max_raw: before.max_raw, own_raw, before_percent: before.percent });
        if let Err(error) = self.set_brightness(wanted).await {
            if matches!(error.downcast_ref::<zbus::Error>(), Some(zbus::Error::MethodError(_, _, _))) { self.dimmed = None; }
            return Err(error);
        }
        let actual = self.brightness().await?;
        let dimmed = self.dimmed.as_ref().expect("issued above");
        if !actual.present || actual.device != dimmed.device || actual.max_raw != dimmed.max_raw || actual.raw != own_raw {
            self.dimmed = None;
            anyhow::bail!("backlight dim target was not confirmed by actual readback; different actual brightness is not owned");
        }
        self.status.dimmed_percent = Some(actual.percent);
        Ok(())
    }
    async fn request_lock(&self) -> Result<()> {
        self.session.emit_signal(None::<&str>, templar_types::dbus::session::LIFECYCLE_PATH,
            "org.templaros.Session.Lifecycle1", "LockRequested", &()).await.context("emitting LockRequested")
    }
    async fn inhibit(&self, what: &str, mode: &str) -> Result<OwnedFd> {
        let manager = self.login_proxy().await?;
        Ok(tokio::time::timeout(CALL_DEADLINE, manager.call("Inhibit", &(what, "Templar session power", "Native compositor-verified session power policy", mode))).await??)
    }
    async fn apply_policy(&mut self) -> Result<()> {
        if !self.policy_dirty { return Ok(()); }
        self.restore_dim().await?;
        if self.pending_sleep.as_ref().is_none_or(|sleep| sleep.kind != SleepKind::External) { self.pending_sleep = None; }
        self.dim_done = false; self.lock_done = false; self.suspend_done = false; self.lid_applied = None;
        let activity = self.activity.as_ref().context("no native activity sample")?;
        ensure!(activity.session_active && available(&activity.idle_control), "native idle control unavailable/inactive");
        let profile = self.status.selected_profile.as_ref().context("no actual power source and valid selected profile")?;
        let timeout_ms = duration_ms(profile.screen_off_after);
        let (screen_armed, lock_armed, suspend_armed, dim_armed) = (profile.screen_off_after.is_some(), profile.lock_after.is_some(), profile.suspend_after.is_some(), profile.dim_after.is_some());
        let expected = activity.revision;
        let response = self.control.as_ref().context("native idle owner unavailable")?.request(ControlRequest::SetIdleTimeout { expected, timeout_ms }).await?;
        let ControlResponse::Activity(actual) = response else { anyhow::bail!("native idle owner returned incompatible apply readback"); };
        ensure!(actual.idle_timeout_ms == timeout_ms && actual.session_active && available(&actual.idle_control), "native screen-off policy was not confirmed by readback");
        let timers_enabled = !actual.inhibited;
        self.observe_activity(actual);
        self.policy_dirty = false;
        self.status.screen_off = PowerFeature::available(screen_armed);
        self.status.lock = PowerFeature::available(timers_enabled && lock_armed);
        self.status.suspend.armed = timers_enabled && available(&self.status.suspend.capability) && suspend_armed;
        self.status.dim = match self.brightness().await {
            Ok(state) if state.present => PowerFeature::available(timers_enabled && dim_armed),
            Ok(_) => PowerFeature::unsupported("backlight authority reports no display backlight"),
            Err(error) => PowerFeature::unavailable(format!("backlight owner readback unavailable: {error:#}")),
        };
        Ok(())
    }
    fn lid_action(&self) -> Result<LidAction> {
        let profile = self.status.selected_profile.as_ref().context("selected lid profile unavailable")?;
        Ok(if self.status.docked.context("actual output topology unavailable")? { profile.lid_closed_docked } else { profile.lid_closed })
    }
    async fn reconcile_lid(&mut self) -> Result<()> {
        ensure!(self.status.lid_present == Some(true), "UPower reports no laptop lid");
        let native = self.activity.as_ref().context("native session activity unavailable")?;
        ensure!(native.session_active && available(&native.display_control), "native output control unavailable/inactive; logind retains lid handling");
        let displays = self.displays().await?;
        ensure!(displays.displays.iter().filter(|output| output.current.enabled).all(|output| output.connected && available(&output.configure)), "enabled output inventory/readback is inaccessible");
        self.status.docked = Some(displays.displays.iter().any(|output| output.connected && output.current.enabled && !output.internal));
        self.panel_owned = displays.panel_closed;
        let action = self.lid_action()?;
        match action {
            LidAction::Suspend => ensure!(available(&self.status.suspend.capability), "selected lid suspension is unavailable"),
            LidAction::Hibernate => ensure!(available(&self.status.hibernate.capability), "selected lid hibernation is unavailable"),
            _ => {}
        }
        if self.lid_inhibitor.is_none() { self.lid_inhibitor = Some(self.inhibit("handle-lid-switch", "block").await?); }
        if !available(&self.status.lid.capability) { self.status.lid = PowerFeature::available(true); }
        let closed = self.status.lid_closed.context("UPower lid readback unavailable")?;
        let docked = self.status.docked.expect("read above");
        let profile = self.status.selected_profile.as_ref().context("selected panel policy unavailable")?;
        let panel_policy = if docked { profile.lid_panel_docked } else { profile.lid_panel };
        if self.lid_applied == Some((closed, docked, action, panel_policy)) { return Ok(()); }
        let result: Result<()> = async {
            if !closed {
                if self.panel_owned { self.panel(false, false).await?; }
                if self.pending_sleep.as_ref().is_some_and(|sleep| sleep.kind != SleepKind::External) { self.pending_sleep = None; }
            } else {
                match action {
                    LidAction::Ignore | LidAction::Lock => {
                        if action == LidAction::Lock { self.request_lock().await?; }
                        match panel_policy {
                            PanelPolicy::Keep => { if self.panel_owned { self.panel(false, false).await?; } },
                            PanelPolicy::ExternalOnly => self.panel(true, false).await?,
                            PanelPolicy::AllowHeadless => self.panel(true, true).await?,
                        }
                    },
                    LidAction::Suspend => self.begin_sleep(SleepKind::Suspend).await?,
                    LidAction::Hibernate => self.begin_sleep(SleepKind::Hibernate).await?,
                }
            }
            Ok(())
        }.await;
        // Failure of an available owned effect is not permission for logind's
        // fallback to bypass this policy's coverage barrier or idle inhibitor.
        self.status.lid.error = result.err().map(|error| error.to_string());
        self.lid_applied = Some((closed, docked, action, panel_policy));
        Ok(())
    }
    async fn panel(&mut self, closed: bool, allow_headless: bool) -> Result<()> {
        let activity = self.native_sample().await?;
        let response = self.control.as_ref().context("native display owner unavailable")?.request(ControlRequest::SetPanelClosed {
            expected: activity.display_revision, closed, allow_headless }).await?;
        let ControlResponse::Displays(actual) = response else { anyhow::bail!("closed-panel owner returned incompatible readback"); };
        ensure!(actual.panel_closed == closed, "closed-panel state was not confirmed by actual display readback");
        self.panel_owned = actual.panel_closed;
        let fresh = self.native_sample().await?; self.observe_activity(fresh);
        Ok(())
    }
    async fn reconcile(&mut self) {
        if let Err(error) = self.install_matches().await { self.system_unavailable(format!("producer subscriptions unavailable: {error:#}")); }
        if let Err(error) = self.observe_system().await { self.system_unavailable(format!("power-state/logind readback unavailable: {error:#}")); }
        if let Err(error) = self.connect_native().await {
            self.native_unavailable(format!("native power owner unavailable: {error:#}")); return;
        }
        let sample = self.native_sample().await;
        match sample { Ok(sample) => { self.observe_activity(sample); }, Err(error) => { self.native_unavailable(format!("native activity readback failed: {error:#}")); return; } }
        if let Err(error) = self.apply_policy().await {
            let reason = format!("selected idle policy not confirmed: {error:#}");
            self.status.screen_off = PowerFeature::unavailable(&reason); self.status.lock = PowerFeature::unavailable(&reason); self.status.dim = PowerFeature::unavailable(&reason);
            self.error(reason);
        }
        if let Err(error) = self.reconcile_lid().await {
            self.lid_inhibitor = None; self.lid_applied = None; self.status.lid = PowerFeature::unavailable(error.to_string());
            self.error(format!("native lid handling unavailable; logind retains its fallback: {error:#}"));
        }
        let native_ready = self.activity.as_ref().is_some_and(|activity| activity.session_active && available(&activity.idle_control) && available(&activity.display_control));
        if native_ready && self.sleep_observed() && self.login_owner.is_some()
            && self.sleep_delay.is_none() && !self.sleeping && self.delay_max > Duration::ZERO {
            match self.inhibit("sleep", "delay").await {
                Ok(fd) => { if self.sleep_observed() { self.sleep_delay = Some(fd); } }, Err(error) => self.error(format!("sleep delay inhibitor unavailable: {error:#}")),
            }
        } else if (!native_ready || !self.sleep_observed()) && self.pending_sleep.as_ref().is_none_or(|sleep| sleep.kind != SleepKind::External) { self.sleep_delay = None; }
    }
    fn native_unavailable(&mut self, reason: String) {
        self.activity_stream = None; self.activity = None; self.policy_dirty = true;
        self.lid_inhibitor = None; self.lid_applied = None;
        if self.pending_sleep.as_ref().is_none_or(|sleep| sleep.kind != SleepKind::External) { self.pending_sleep = None; self.sleep_delay = None; }
        self.status.session_active = None; self.status.inhibited = None; self.status.protected_covered = None; self.status.screen_off_observed = false; self.status.docked = None;
        self.status.screen_off = PowerFeature::unavailable(&reason); self.status.dim = PowerFeature::unavailable(&reason); self.status.lock = PowerFeature::unavailable(&reason); self.status.lid = PowerFeature::unavailable(&reason);
        self.status.suspend.armed = false;
        self.error(reason);
    }
    fn elapsed(&self) -> Option<Duration> {
        let activity = self.activity.as_ref()?;
        if self.policy_dirty || self.sleeping || !activity.session_active || activity.inhibited { return None; }
        Some(Duration::from_millis(activity.idle_elapsed_ms).saturating_add(self.observed_at.elapsed()))
    }
    fn next_wake(&self) -> Option<Instant> {
        let sleep_deadline = self.pending_sleep.as_ref().filter(|sleep| !sleep.expired).map(|sleep| sleep.deadline);
        let policy = self.status.selected_profile.as_ref().zip(self.elapsed()).and_then(|(profile, elapsed)| {
            [deadline_after(profile.dim_after, elapsed, self.dim_done || !self.status.dim.armed),
             deadline_after(profile.lock_after, elapsed, self.lock_done || !self.status.lock.armed),
             deadline_after(profile.suspend_after, elapsed, self.suspend_done || !self.status.suspend.armed)]
                .into_iter().flatten().min().map(|remaining| Instant::now() + remaining)
        });
        [sleep_deadline, policy].into_iter().flatten().min()
    }
    async fn begin_sleep(&mut self, kind: SleepKind) -> Result<()> {
        ensure!(self.sleep_observed(), "sole PrepareForSleep observer is not ready; automatic sleep refused");
        if self.pending_sleep.as_ref().is_some_and(|sleep| sleep.kind == SleepKind::External) { return Ok(()); }
        let fresh = self.native_sample().await?;
        ensure!(fresh.session_active && available(&fresh.idle_control) && available(&fresh.display_control), "native coverage owner unavailable/inactive; automatic sleep refused");
        if kind != SleepKind::External { ensure!(!fresh.inhibited, "native idle inhibitor blocks automatic sleep"); }
        let deadline = if kind == SleepKind::External { self.delay_max.saturating_sub(Duration::from_millis(200)).min(Duration::from_secs(10)) } else { Duration::from_secs(10) };
        let epoch = fresh.revision.owner_epoch; let serial = fresh.activity_serial;
        let covered = protected_sleep_ready(&fresh);
        self.observe_activity(fresh);
        self.pending_sleep = Some(PendingSleep { kind, epoch, serial, deadline: Instant::now() + deadline, expired: false });
        if !covered {
            if let Err(error) = self.request_lock().await {
                if kind != SleepKind::External { self.pending_sleep = None; }
                return Err(error);
            }
        }
        self.finish_sleep().await
    }
    async fn finish_sleep(&mut self) -> Result<()> {
        let Some(pending) = self.pending_sleep.as_ref() else { return Ok(()); };
        ensure!(self.sleep_observed(), "sole PrepareForSleep observer was lost; sleep protection cannot be confirmed");
        if pending.expired || Instant::now() >= pending.deadline { return Ok(()); }
        let kind = pending.kind; let epoch = pending.epoch; let serial = pending.serial; let deadline = pending.deadline;
        // Prepare the effect endpoint before the final native coverage sample.
        let manager = if kind == SleepKind::External { None } else { Some(self.login_proxy().await?) };
        let fresh = self.native_sample().await?;
        if fresh.revision.owner_epoch != epoch || (kind != SleepKind::External && (fresh.activity_serial != serial || fresh.inhibited || !fresh.session_active)) {
            self.pending_sleep = None;
            anyhow::bail!("sleep policy epoch/activity changed; automatic request canceled");
        }
        let covered = protected_sleep_ready(&fresh);
        self.observe_activity(fresh);
        if !covered { return Ok(()); }
        if Instant::now() >= deadline { return Ok(()); }
        ensure!(self.sleep_observed(), "sole PrepareForSleep observer was lost during coverage readback");
        // The just-sampled current native generation covers every required output. A
        // saved SessionEdge::Locked is intentionally not read anywhere in this decision.
        if kind == SleepKind::External {
            self.sleep_delay = None;
            self.pending_sleep = None;
            return Ok(());
        }
        let capability = if kind == SleepKind::Suspend { &self.status.suspend.capability } else { &self.status.hibernate.capability };
        ensure!(available(capability), "logind automatic sleep capability unavailable");
        // Release our own delay only after this fresh coverage barrier, before asking
        // logind to sleep: retaining the FD while awaiting Suspend would put its
        // PrepareForSleep event behind this same serialized owner.
        self.sleep_delay = None;
        let manager = manager.context("automatic sleep effect endpoint was not prepared")?;
        let method = if kind == SleepKind::Suspend { "Suspend" } else { "Hibernate" };
        let result = tokio::time::timeout(CALL_DEADLINE, manager.call::<_, _, ()>(method, &(false,))).await;
        drop(manager);
        self.pending_sleep = None;
        match result {
            Ok(Ok(())) => {}
            result => {
                if self.sleep_observed() {
                    if let Ok(fd) = self.inhibit("sleep", "delay").await {
                        if self.sleep_observed() { self.sleep_delay = Some(fd); }
                    }
                }
                match result { Ok(Err(error)) => return Err(error.into()), Err(error) => return Err(error.into()), Ok(Ok(())) => unreachable!() }
            }
        }
        Ok(())
    }
    async fn deadline(&mut self) {
        // Expire the barrier even when its native producer is gone. Otherwise an external
        // retained inhibitor leaves a past deadline armed and retries failed readback in a loop.
        if let Some(pending) = self.pending_sleep.as_mut()
            && !pending.expired && Instant::now() >= pending.deadline {
            pending.expired = true;
            let kind = pending.kind; let external = kind == SleepKind::External;
            let reason = "current protected-output presentation was not confirmed before the sleep deadline; no automatic sleep was issued";
            match kind {
                SleepKind::Suspend => self.status.suspend.error = Some(reason.into()),
                SleepKind::Hibernate => self.status.hibernate.error = Some(reason.into()),
                SleepKind::External => {}
            }
            self.status.lid.error = Some(reason.into());
            self.error(reason.into());
            // Keep an external delay FD held until resume/lifetime loss. logind can
            // still force external sleep after its own InhibitDelayMaxUSec limit.
            if !external { self.pending_sleep = None; }
        }
        let sample = match self.native_sample().await { Ok(sample) => sample, Err(error) => { self.native_unavailable(format!("power deadline native readback failed: {error:#}")); return; } };
        self.observe_activity(sample);
        let Some(elapsed) = self.elapsed() else { return; };
        let Some(profile) = self.status.selected_profile.as_ref() else { return; };
        let dim_due = !self.dim_done && self.status.dim.armed && profile.dim_after.is_some_and(|after| elapsed >= after);
        let lock_due = !self.lock_done && self.status.lock.armed && profile.lock_after.is_some_and(|after| elapsed >= after);
        let suspend_due = !self.suspend_done && self.status.suspend.armed && profile.suspend_after.is_some_and(|after| elapsed >= after);
        if dim_due { self.dim_done = true; if let Err(error) = self.dim().await { self.status.dim.error = Some(error.to_string()); } }
        if lock_due { self.lock_done = true; if let Err(error) = self.request_lock().await { self.status.lock.error = Some(error.to_string()); } }
        if suspend_due { self.suspend_done = true; if let Err(error) = self.begin_sleep(SleepKind::Suspend).await { self.status.suspend.error = Some(error.to_string()); } }
        if self.pending_sleep.is_some() && let Err(error) = self.finish_sleep().await { self.error(format!("secure sleep refused/incomplete: {error:#}")); }
    }
    async fn run(&mut self) {
        loop {
            if self.restore_requested {
                self.restore_requested = false;
                if let Err(error) = self.restore_dim().await { self.status.dim.error = Some(error.to_string()); }
                self.changed().await;
            }
            let wake = self.next_wake();
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        Command::Inspect(reply) => {
                            if self.activity_stream.is_none() || self.signals.is_empty() || self.login_owner.is_none() || self.upower_owner.is_none() { self.reconcile().await; }
                            else { match self.native_sample().await { Ok(sample) => { self.observe_activity(sample); }, Err(error) => self.native_unavailable(format!("native inspection unavailable: {error:#}")) } }
                            self.update_owned_status();
                            let _ = reply.send(serde_json::to_string(&self.status).map_err(Into::into));
                        }
                        Command::Change { source, profile, reply } => {
                            let result = store_profile(&self.store, source, profile.as_ref());
                            if result.is_ok() { self.load_profiles(); self.select_profile(); self.reconcile().await; self.changed().await; }
                            let result = result.and_then(|_| if self.status.selected_source == Some(source) && self.policy_dirty {
                                Err(anyhow::anyhow!("profile stored, but selected policy is not confirmed by native readback; inspect field capabilities/errors"))
                            } else { Ok(()) });
                            let _ = reply.send(result);
                        }
                    }
                }
                observation = async { match self.activity_stream.as_mut() { Some(stream) => stream.next().await, None => std::future::pending().await } } => {
                    match observation {
                        Some(Ok(activity)) => {
                            let had_pending = self.pending_sleep.is_some();
                            let topology_changed = self.activity.as_ref().is_none_or(|old| old.display_revision != activity.display_revision);
                            let changed = self.observe_activity(activity);
                            if !self.activity.as_ref().is_some_and(|activity| activity.session_active) {
                                self.lid_inhibitor = None;
                                self.status.lid = PowerFeature::unavailable("native session is inactive; logind retains lid handling");
                                if self.pending_sleep.as_ref().is_none_or(|sleep| sleep.kind != SleepKind::External) { self.sleep_delay = None; }
                            }
                            if self.policy_dirty { self.reconcile().await; }
                            else if topology_changed && let Err(error) = self.reconcile_lid().await { self.lid_inhibitor = None; self.status.lid = PowerFeature::unavailable(error.to_string()); }
                            if self.pending_sleep.is_some() && let Err(error) = self.finish_sleep().await { self.error(format!("secure sleep refused/incomplete: {error:#}")); }
                            if changed || topology_changed || had_pending != self.pending_sleep.is_some() { self.changed().await; }
                        }
                        _ => { self.native_unavailable("native activity stream ended; policy paused until actual producer recovery".into()); self.changed().await; }
                    }
                }
                signal = self.signals.next(), if !self.signals.is_empty() => {
                    match signal {
                        Some(Ok(message)) => {
                            let header = message.header();
                            if header.member().is_some_and(|member| member.as_str() == "NameOwnerChanged") {
                                if let Ok((name, _, _)) = message.body().deserialize::<(String, String, String)>() {
                                    if name != UPOWER && name != LOGIN { continue; }
                                } else { continue; }
                            }
                            self.reconcile().await; self.changed().await;
                        }
                        _ => { self.signals.clear(); self.system_unavailable("system power producer stream ended; owned inhibitors released".into()); self.changed().await; }
                    }
                }
                signal = self.session_signals.next(), if !self.session_signals.is_empty() => {
                    match signal {
                        Some(Ok(message)) if message.header().path().is_some_and(|path| path.as_str().ends_with("/templar_2dcomp_2eservice")) => { self.reconcile().await; self.changed().await; }
                        Some(Ok(_)) => {}
                        _ => { self.session_signals.clear(); self.error("native unit recovery observation ended; inspect to reinstall observation".into()); self.changed().await; }
                    }
                }
                ready = self.sleep_ready.changed(), if self.sleep_watch_alive => {
                    if ready.is_err() { self.sleep_watch_alive = false; }
                    if !self.sleep_observed() {
                        self.sleep_delay = None; self.pending_sleep = None;
                        self.status.suspend = PowerFeature::unavailable("sole PrepareForSleep observation ended; delay ownership released");
                        self.status.hibernate = PowerFeature::unavailable("sole PrepareForSleep observation ended; automatic hibernation disabled");
                        self.lid_applied = None;
                    }
                    self.policy_dirty = true;
                    self.reconcile().await;
                    self.changed().await;
                }
                edge = self.edges.recv() => {
                    match edge {
                        Ok(SessionEdge::Sleeping(true)) => {
                            self.sleeping = true;
                            if self.sleep_delay.is_some() {
                                if let Err(error) = self.begin_sleep(SleepKind::External).await { self.error(format!("external sleep protection unconfirmed: {error:#}; logind's finite delay cannot guarantee cancellation")); }
                            } else { self.error("external sleep has no owned delay inhibitor; protection delay cannot be promised".into()); }
                            self.changed().await;
                        }
                        Ok(SessionEdge::Sleeping(false)) => { self.sleeping = false; self.sleep_delay = None; self.pending_sleep = None; self.policy_dirty = true; self.reconcile().await; self.changed().await; }
                        Ok(SessionEdge::Started) => { self.reconcile().await; self.changed().await; }
                        Ok(_) => {} // Locked is accounting only, never sleep authorization.
                        Err(broadcast::error::RecvError::Lagged(_)) => { self.sleep_delay = None; self.pending_sleep = None; self.error("session sleep edges lagged; secure sleep state must be re-observed".into()); self.reconcile().await; self.changed().await; }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = async { match wake { Some(wake) => tokio::time::sleep_until(wake).await, None => std::future::pending().await } } => { self.deadline().await; self.changed().await; }
            }
        }
    }
}

#[cfg(test)]
#[path = "power_native_tests.rs"]
mod native_tests;
