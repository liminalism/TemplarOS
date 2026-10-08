//! BlueZ readback and caller-owned pairing UI; display codes stay in typed zeroizing buffers.
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::{sync::Weak, time::Duration};
use templar_client::{devices::{BluetoothSnapshot, PromptKind}, proxies::BluetoothProxy};
use templar_types::settings::OperationError;
use zeroize::Zeroizing;

#[derive(Default)]
pub struct BluetoothState {
    pub snapshot: Option<BluetoothSnapshot>,
    pub display: Option<(String, Zeroizing<String>)>,
    pub error: Option<OperationError>,
}
impl std::fmt::Debug for BluetoothState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BluetoothState").field("snapshot", &self.snapshot)
            .field("display", &self.display.as_ref().map(|_| "<redacted>"))
            .field("error", &self.error).finish()
    }
}
pub enum BluetoothCommand {
    Refresh, ProducerRestart, Observe(bool), Power { adapter: String, enabled: bool },
    Discover { adapter: String, enabled: bool }, Pair(String), Cancel, Connect(String), Disconnect(String), Forget(String),
    Trust { device: String, trusted: bool }, Answer { token: String, approved: bool }, Secret { token: String, value: Zeroizing<String> },
}
impl std::fmt::Debug for BluetoothCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self { Self::Refresh => "Refresh", Self::ProducerRestart => "ProducerRestart", Self::Observe(_) => "Observe",
            Self::Power { .. } => "Power", Self::Discover { .. } => "Discover", Self::Pair(_) => "Pair", Self::Cancel => "Cancel",
            Self::Connect(_) => "Connect", Self::Disconnect(_) => "Disconnect", Self::Forget(_) => "Forget", Self::Trust { .. } => "Trust",
            Self::Answer { .. } => "Answer", Self::Secret { .. } => "Secret(<redacted>)" })
    }
}
#[derive(Default)]
pub struct BluetoothController {
    connection: Option<zbus::Connection>, observing: bool, watchers: Vec<tokio::task::AbortHandle>,
    sender: Option<Weak<tokio::sync::mpsc::UnboundedSender<BluetoothCommand>>>,
}
impl BluetoothController {
    pub async fn run(&mut self, command: BluetoothCommand) -> BluetoothState {
        let was_observing = self.observing;
        if matches!(&command, BluetoothCommand::ProducerRestart) {
            for watcher in self.watchers.drain(..) { watcher.abort(); }
            self.connection = None; // Caller loss revokes the old producer's agent lease.
        }
        if let BluetoothCommand::Observe(observing) = &command { self.observing = *observing; }
        if matches!(&command, BluetoothCommand::ProducerRestart)
            && let Some(sender) = self.sender.clone()
            && let Err(error) = self.subscribe(sender).await {
            return BluetoothState { error: Some(OperationError { code: "bluetooth-observer".into(), message: error.to_string() }), ..Default::default() };
        }
        if self.connection.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => return BluetoothState { error: Some(OperationError { code: "session-unavailable".into(), message: error.to_string() }), ..Default::default() },
            }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation = async {
            let proxy = BluetoothProxy::new(connection).await?;
            if !matches!(&command, BluetoothCommand::Observe(false)) && self.observing { proxy.acquire_ui().await?; }
            match command {
                BluetoothCommand::Refresh | BluetoothCommand::ProducerRestart => Ok::<(), anyhow::Error>(()),
                BluetoothCommand::Observe(true) => proxy.acquire_ui().await.map_err(Into::into),
                BluetoothCommand::Observe(false) => if was_observing { proxy.release_ui().await.map_err(Into::into) } else { Ok(()) },
                BluetoothCommand::Power { adapter, enabled } => proxy.set_powered(&adapter, enabled).await.map_err(Into::into),
                BluetoothCommand::Discover { adapter, enabled } => proxy.discover(&adapter, enabled).await.map_err(Into::into),
                BluetoothCommand::Pair(device) => proxy.pair(&device).await.map_err(Into::into),
                BluetoothCommand::Cancel => proxy.cancel().await.map_err(Into::into),
                BluetoothCommand::Connect(device) => proxy.connect(&device).await.map_err(Into::into),
                BluetoothCommand::Disconnect(device) => proxy.disconnect(&device).await.map_err(Into::into),
                BluetoothCommand::Forget(device) => proxy.forget(&device).await.map_err(Into::into),
                BluetoothCommand::Trust { device, trusted } => proxy.set_trusted(&device, trusted).await.map_err(Into::into),
                BluetoothCommand::Answer { token, approved } => proxy.answer(&token, approved).await.map_err(Into::into),
                BluetoothCommand::Secret { token, value } => proxy.answer_secret(&token, templar_client::secret_fd::seal_utf8(&value, 4096)?.into()).await.map_err(Into::into),
            }
        };
        let write = tokio::time::timeout(Duration::from_secs(30), operation).await;
        let observed = async {
            let proxy = BluetoothProxy::new(connection).await?;
            let snapshot: BluetoothSnapshot = serde_json::from_str(&proxy.inventory().await?)?;
            Ok::<_, anyhow::Error>(snapshot)
        };
        let read = tokio::time::timeout(Duration::from_secs(20), observed).await;
        let mut errors = Vec::new();
        match write { Ok(Err(error)) => errors.push(format!("Change refused or incomplete: {error:#}")), Err(_) => errors.push("Change timed out; readback is applied state, not confirmation of the request".into()), _ => {} }
        let snapshot = match read { Ok(Ok(snapshot)) => Some(snapshot), Ok(Err(error)) => { errors.push(format!("Readback unavailable: {error:#}")); None }, Err(_) => { errors.push("Bluetooth readback timed out".into()); None } };
        let display = if let Some(prompt) = snapshot.as_ref().and_then(|snapshot| snapshot.prompts.first())
            .filter(|prompt| matches!(prompt.kind, PromptKind::DisplayPin | PromptKind::DisplayPasskey | PromptKind::Confirmation)) {
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                let descriptor = BluetoothProxy::new(connection).await?.display_secret(&prompt.token).await?;
                templar_client::secret_fd::read_utf8(descriptor, 4096)
            }).await;
            match result {
                Ok(Ok(code)) => Some((prompt.token.clone(), code)),
                Ok(Err(error)) => { errors.push(format!("Pairing code unavailable: {error:#}")); None },
                Err(_) => { errors.push("Pairing code read timed out; device readback remains available".into()); None },
            }
        } else { None };
        BluetoothState { snapshot, display, error: (!errors.is_empty()).then(|| OperationError { code: "bluetooth-operation".into(), message: errors.join("; ") }) }
    }
    /// Parent sends this weak command receiver into the same serialized run() queue.
    pub async fn subscribe(&mut self, sender: Weak<tokio::sync::mpsc::UnboundedSender<BluetoothCommand>>) -> Result<()> {
        if self.connection.is_none() { self.connection = Some(zbus::Connection::session().await?); }
        for watcher in self.watchers.drain(..) { watcher.abort(); }
        self.sender = Some(sender.clone());
        let (ready, registered) = tokio::sync::oneshot::channel();
        let connection = self.connection.as_ref().expect("connected above").clone();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let result = async {
                let proxy = BluetoothProxy::new(&connection).await?;
                let mut changes = proxy.receive_changed().await?;
                let bus = zbus::fdo::DBusProxy::new(&connection).await?;
                let mut owners = bus.receive_name_owner_changed().await?;
                if let Some(ready) = ready.take() { let _ = ready.send(Ok(())); }
                loop {
                    tokio::select! {
                        change = changes.next() => {
                            anyhow::ensure!(change.is_some(), "Bluetooth change stream closed");
                            let Some(sender) = sender.upgrade() else { break };
                            if sender.send(BluetoothCommand::Refresh).is_err() { break; }
                        }
                        owner = owners.next() => {
                            let owner = owner.context("session owner observation stream closed")?;
                            if owner.args()?.name().as_str() != "org.templaros.Session" { continue; }
                            let Some(sender) = sender.upgrade() else { break };
                            if sender.send(BluetoothCommand::ProducerRestart).is_err() { break; }
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            if let Err(error) = result {
                if let Some(ready) = ready.take() { let _ = ready.send(Err(error)); }
                else if let Some(sender) = sender.upgrade() { let _ = sender.send(BluetoothCommand::ProducerRestart); }
            }
        });
        self.watchers.push(task.abort_handle());
        match tokio::time::timeout(Duration::from_secs(10), registered).await {
            Ok(Ok(Ok(()))) => Ok(()),
            result => {
                for watcher in self.watchers.drain(..) { watcher.abort(); }
                match result {
                    Ok(Ok(Err(error))) => Err(error),
                    Ok(Err(error)) => Err(error.into()),
                    Err(_) => anyhow::bail!("Bluetooth observer registration timed out"),
                    Ok(Ok(Ok(()))) => unreachable!(),
                }
            }
        }
    }
}
impl Drop for BluetoothController {
    fn drop(&mut self) {
        for watcher in self.watchers.drain(..) { watcher.abort(); }
        if self.observing && let Some(connection) = self.connection.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { let _ = tokio::time::timeout(Duration::from_secs(15), async {
                BluetoothProxy::new(&connection).await?.release_ui().await
            }).await; });
        }
    }
}
pub async fn print() -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let state: BluetoothSnapshot = serde_json::from_str(&BluetoothProxy::new(&connection).await?.inventory().await?)?;
    println!("Bluetooth: {:?}", state.capability);
    for device in state.devices { println!("{} {} paired={} connected={} trusted={}", device.address, device.label, device.paired, device.connected, device.trusted); }
    Ok(())
}
#[derive(Default)]
pub struct BluetoothView {
    pub state: BluetoothState,
    secret: pixelkit_ui::TextFieldState,
    prompt: Option<String>,
    revealed: bool,
    secret_drawn: bool,
    scroll: pixelkit_ui::ScrollState,
    heights: Vec<i32>,
    adapter_labels: Vec<String>,
    device_labels: Vec<String>,
    radio_labels: Vec<String>,
    prompt_label: String,
    entered_label: String,
    pairing_label: String,
}
impl Drop for BluetoothView { fn drop(&mut self) { self.leave(); } }
impl BluetoothView {
    pub fn leave(&mut self) { self.secret.clear_sensitive(); self.state.display = None; self.prompt = None; self.revealed = false; self.secret_drawn = false; }
    pub fn replace(&mut self, state: BluetoothState) -> bool {
        use std::fmt::Write;
        let owner_changed = match (&self.state.snapshot, &state.snapshot) {
            (Some(old), Some(new)) => old.revision.owner_epoch != new.revision.owner_epoch,
            (None, None) => false, _ => true,
        };
        if owner_changed { self.leave(); }
        let changed = match (&self.state.snapshot, &state.snapshot) {
            (Some(old), Some(new)) => old.revision.owner_epoch != new.revision.owner_epoch
                || old.ui_owned != new.ui_owned
                || old.adapters.iter().map(|adapter| (&adapter.path, adapter.powered))
                    .ne(new.adapters.iter().map(|adapter| (&adapter.path, adapter.powered)))
                || old.devices.iter().map(|device| (&device.path, device.paired))
                    .ne(new.devices.iter().map(|device| (&device.path, device.paired)))
                || old.prompts.iter().map(|prompt| &prompt.token).ne(new.prompts.iter().map(|prompt| &prompt.token)),
            (None, None) => false, _ => true,
        };
        let prompt = state.snapshot.as_ref().and_then(|snapshot| snapshot.prompts.first()).map(|prompt| prompt.token.clone());
        if self.prompt != prompt || self.state.display != state.display { self.secret.clear_sensitive(); self.revealed = false; self.prompt = prompt; }
        if let Some(snapshot) = &state.snapshot {
            self.adapter_labels.resize_with(snapshot.adapters.len(), String::new);
            for (label, adapter) in self.adapter_labels.iter_mut().zip(&snapshot.adapters) {
                label.clear(); let _ = write!(label, "{} · {} · scanning={}", adapter.label, adapter.address, adapter.discovering);
            }
            self.device_labels.resize_with(snapshot.devices.len(), String::new);
            for (label, device) in self.device_labels.iter_mut().zip(&snapshot.devices) {
                label.clear(); let _ = write!(label, "{} · {} · paired={} connected={}", device.label, device.address, device.paired, device.connected);
            }
            self.radio_labels.resize_with(snapshot.radio_blocks.len(), String::new);
            for (label, block) in self.radio_labels.iter_mut().zip(&snapshot.radio_blocks) {
                label.clear(); let _ = write!(label, "{} · software blocked={} · hardware blocked={}", block.name, block.software, block.hardware);
            }
            self.prompt_label.clear(); self.entered_label.clear(); self.pairing_label.clear();
            if let Some(prompt) = snapshot.prompts.first() {
                let label = snapshot.devices.iter().find(|device| device.path == prompt.subject).map_or(prompt.subject.as_str(), |device| device.label.as_str());
                let _ = write!(self.prompt_label, "{label} — {}", prompt.label);
                let _ = write!(self.entered_label, "{} keys entered on other device", prompt.entered);
            }
            if let Some(path) = &snapshot.pairing {
                let label = snapshot.devices.iter().find(|device| &device.path == path).map_or(path.as_str(), |device| device.label.as_str());
                let _ = write!(self.pairing_label, "Native pairing pending: {label}");
            }
        }
        self.state = state;
        changed
    }
    pub fn ime_caret(&self) -> Option<pixelkit_raster::Rect> { self.secret_drawn.then(|| self.secret.caret_area()).flatten() }
    pub fn draw(&mut self, ui: &mut pixelkit_ui::Ui<'_>, area: pixelkit_raster::Rect) -> (Option<BluetoothCommand>, bool) {
        use pixelkit_raster::Rect;
        use pixelkit_text::Align;
        let h = ui.px(32);
        let gap = ui.px(8);
        let mut y = area.y;
        let mut command = None;
        self.secret_drawn = false;
        if ui.button(Rect::new(area.x, y, ui.px(90), h), "Refresh") { command = Some(BluetoothCommand::Refresh); }
        if self.state.snapshot.as_ref().is_some_and(|snapshot| snapshot.ui_owned)
            && ui.button(Rect::new(area.x + ui.px(100), y, ui.px(150), h), "Cancel pairing") { self.secret.clear_sensitive(); self.state.display = None; command = Some(BluetoothCommand::Cancel); }
        y += h + gap;
        if let Some(error) = &self.state.error { ui.label(Rect::new(area.x, y, area.w, h), &error.message, ui.theme.alarm, Align::Left); y += h + gap; }
        let Some(snapshot) = &self.state.snapshot else { ui.label(Rect::new(area.x, y, area.w, h), "Bluetooth readback unavailable", ui.theme.text_dim, Align::Left); return (command, false) };
        if let templar_types::settings::Capability::Unavailable { reason } | templar_types::settings::Capability::Unsupported { reason } = &snapshot.capability {
            ui.label(Rect::new(area.x, y, area.w, h), reason, ui.theme.alarm, Align::Left); y += h + gap;
        }
        if let Some(error) = &snapshot.operation_error { ui.label(Rect::new(area.x, y, area.w, h), error, ui.theme.alarm, Align::Left); y += h + gap; }
        if !self.pairing_label.is_empty() { ui.label(Rect::new(area.x, y, area.w, h), &self.pairing_label, ui.theme.text, Align::Left); y += h + gap; }
        for label in &self.radio_labels {
            ui.label(Rect::new(area.x, y, area.w, h), label, ui.theme.text_dim, Align::Left); y += h + gap;
        }
        if !snapshot.ui_owned {
            if ui.button(Rect::new(area.x, y, ui.px(280), h), "Use native controls / pairing prompts") { command = Some(BluetoothCommand::Observe(true)); }
            y += h + gap;
        }
        if let Some(prompt) = snapshot.prompts.first() {
            ui.label(Rect::new(area.x, y, area.w, h), &self.prompt_label, ui.theme.text, Align::Left); y += h + gap;
            if matches!(prompt.kind, PromptKind::Pin | PromptKind::Passkey | PromptKind::Password) {
                ui.text_field(&mut self.secret, Rect::new(area.x, y, (area.w - ui.px(200)).max(0), h), "PIN / passkey (masked)", false, true);
                self.secret_drawn = true;
                if ui.button(Rect::new(area.right() - ui.px(190), y, ui.px(90), h), "Submit") { command = Some(BluetoothCommand::Secret { token: prompt.token.clone(), value: Zeroizing::new(self.secret.take_text()) }); }
            } else {
                if let Some((token, code)) = &self.state.display && token == &prompt.token {
                    ui.label(Rect::new(area.x, y, ui.px(160), h), if self.revealed { code.as_str() } else { "******" }, ui.theme.text, Align::Left);
                    if ui.button(Rect::new(area.x + ui.px(170), y, ui.px(150), h), if self.revealed { "Hide code" } else { "Reveal code" }) { self.revealed = !self.revealed; }
                    y += h + gap;
                    ui.label(Rect::new(area.x, y, area.w, h), &self.entered_label, ui.theme.text_dim, Align::Left); y += h + gap;
                }
                if matches!(prompt.kind, PromptKind::Confirmation | PromptKind::Authorization)
                    && ui.button(Rect::new(area.x, y, ui.px(150), h), if prompt.kind == PromptKind::Confirmation { "Codes match" } else { "Authorize" }) {
                    self.state.display = None; self.secret.clear_sensitive(); command = Some(BluetoothCommand::Answer { token: prompt.token.clone(), approved: true });
                }
            }
            if ui.button(Rect::new(area.right() - ui.px(90), y, ui.px(90), h), "Reject") {
                self.state.display = None; self.secret.clear_sensitive();
                command = Some(if matches!(prompt.kind, PromptKind::DisplayPin | PromptKind::DisplayPasskey) { BluetoothCommand::Cancel }
                    else { BluetoothCommand::Answer { token: prompt.token.clone(), approved: false } });
            }
            return (command, false);
        }
        ui.label(Rect::new(area.x, y, area.w, h), "Trust is explicit. Audio routing after pairing is under Sound.", ui.theme.text_dim, Align::Left); y += h + gap;
        self.heights.clear();
        self.heights.extend(snapshot.adapters.iter().map(|_| ui.px(100)));
        self.heights.extend(snapshot.devices.iter().map(|_| ui.px(116)));
        let adapter_count = snapshot.adapters.len();
        ui.scroll_list(&mut self.scroll, Rect::new(area.x, y, area.w, (area.bottom() - y).max(0)), &self.heights, |ui, index, rect| {
            if index < adapter_count {
                let adapter = &snapshot.adapters[index];
                ui.label(Rect::new(rect.x, rect.y, rect.w, h), &self.adapter_labels[index], ui.theme.text, Align::Left);
                if snapshot.ui_owned {
                    let mut powered = adapter.powered;
                    if ui.toggle(Rect::new(rect.x, rect.y + h + gap, ui.px(160), h), &mut powered, "Power") { command = Some(BluetoothCommand::Power { adapter: adapter.path.clone(), enabled: powered }); }
                    if adapter.powered && ui.button(Rect::new(rect.x + ui.px(170), rect.y + h + gap, ui.px(170), h), if adapter.discovery_owned { "Stop native discovery" } else { "Discover devices" }) { command = Some(BluetoothCommand::Discover { adapter: adapter.path.clone(), enabled: !adapter.discovery_owned }); }
                }
            } else {
                let device = &snapshot.devices[index - adapter_count];
                ui.label(Rect::new(rect.x, rect.y, rect.w, h), &self.device_labels[index - adapter_count], ui.theme.text, Align::Left);
                if snapshot.ui_owned {
                    let y = rect.y + h + gap;
                    if !device.paired && ui.button(Rect::new(rect.x, y, ui.px(80), h), "Pair") { command = Some(BluetoothCommand::Pair(device.path.clone())); }
                    if device.paired && ui.button(Rect::new(rect.x, y, ui.px(100), h), if device.connected { "Disconnect" } else { "Connect" }) { command = Some(if device.connected { BluetoothCommand::Disconnect(device.path.clone()) } else { BluetoothCommand::Connect(device.path.clone()) }); }
                    if ui.button(Rect::new(rect.x + ui.px(110), y, ui.px(80), h), "Forget") { command = Some(BluetoothCommand::Forget(device.path.clone())); }
                    let mut trusted = device.trusted;
                    if ui.toggle(Rect::new(rect.x + ui.px(200), y, ui.px(200), h), &mut trusted, "Trust this device") { command = Some(BluetoothCommand::Trust { device: device.path.clone(), trusted }); }
                }
            }
        });
        (command, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> BluetoothSnapshot {
        BluetoothSnapshot {
            revision: templar_types::settings::Revision::new_owner(), capability: templar_types::settings::Capability::Available { requires_restart: false },
            ui_owned: true, adapters: vec![templar_client::devices::BluetoothAdapter { path: "/adapter".into(), address: "00:11:22:33:44:55".into(),
                label: "Fixture adapter".into(), powered: true, discovering: true, discovery_owned: false, pairable: true }],
            devices: Vec::new(), radio_blocks: Vec::new(), prompts: vec![templar_client::devices::DevicePrompt { token: "fixture-token".into(),
                subject: "/device".into(), label: "Fixture confirmation".into(), kind: PromptKind::Confirmation, deadline_unix_ms: 0, entered: 0 }],
            operation_error: None, pairing: Some("/device".into()),
        }
    }
    #[test]
    fn unrelated_scanning_is_not_owned_and_code_state_never_enters_debug() {
        let mut view = BluetoothView::default();
        let mut observed = snapshot();
        let state = BluetoothState { snapshot: Some(observed.clone()), display: Some(("fixture-token".into(), Zeroizing::new("123456".into()))), error: None };
        assert!(!format!("{state:?}").contains("123456"));
        assert!(view.replace(state));
        assert!(!view.state.snapshot.as_ref().unwrap().adapters[0].discovery_owned);
        view.revealed = true;
        observed.revision.generation += 1;
        observed.prompts[0].entered = 3;
        assert!(!view.replace(BluetoothState { snapshot: Some(observed.clone()), display: Some(("fixture-token".into(), Zeroizing::new("123456".into()))), error: None }));
        assert!(view.revealed);
        observed.revision = templar_types::settings::Revision::new_owner();
        assert!(view.replace(BluetoothState { snapshot: Some(observed), display: None, error: None }));
        assert!(!view.revealed);
        assert!(view.state.display.is_none());
        view.secret.set_text("fixture-pin");
        view.leave();
        assert!(view.secret.text().is_empty());
    }
}
