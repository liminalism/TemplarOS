//! A persistent caller connection owns the native UI lease. Device values are read back,
//! never assigned from desired writes; the existing network provider remains a real route.
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::{sync::Weak, time::Duration};
use templar_client::{
    devices::{NetworkSnapshot, PromptKind},
    proxies::{LauncherProxy, NetworkProxy, ProvidersProxy},
};
use templar_types::settings::OperationError;
use zeroize::Zeroizing;

#[derive(Debug, Default)]
pub struct NetworkState {
    pub snapshot: Option<NetworkSnapshot>,
    pub error: Option<OperationError>,
}
pub enum NetworkCommand {
    Refresh,
    ProducerRestart,
    Observe(bool),
    SetWireless(bool),
    Scan(String),
    Connect {
        device: String,
        point: String,
        ssid: Vec<u8>,
        security: String,
        hidden: bool,
        session_only: bool,
    },
    Activate {
        profile: String,
        device: String,
    },
    Disconnect(String),
    Cancel,
    Forget(String),
    Autoconnect {
        profile: String,
        enabled: bool,
    },
    Answer {
        token: String,
        approved: bool,
    },
    Secret {
        token: String,
        value: Zeroizing<String>,
    },
    OpenProvider,
}
impl std::fmt::Debug for NetworkCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Refresh => "Refresh",
            Self::ProducerRestart => "ProducerRestart",
            Self::Observe(_) => "Observe",
            Self::SetWireless(_) => "SetWireless",
            Self::Scan(_) => "Scan",
            Self::Connect { .. } => "Connect",
            Self::Activate { .. } => "Activate",
            Self::Disconnect(_) => "Disconnect",
            Self::Cancel => "Cancel",
            Self::Forget(_) => "Forget",
            Self::Autoconnect { .. } => "Autoconnect",
            Self::Answer { .. } => "Answer",
            Self::Secret { .. } => "Secret(<redacted>)",
            Self::OpenProvider => "OpenProvider",
        })
    }
}
#[derive(Default)]
pub struct NetworkController {
    connection: Option<zbus::Connection>,
    observing: bool,
    watchers: Vec<tokio::task::AbortHandle>,
    sender: Option<Weak<tokio::sync::mpsc::UnboundedSender<NetworkCommand>>>,
}
impl NetworkController {
    pub async fn run(&mut self, command: NetworkCommand) -> NetworkState {
        let was_observing = self.observing;
        if matches!(&command, NetworkCommand::ProducerRestart) {
            for watcher in self.watchers.drain(..) {
                watcher.abort();
            }
            self.connection = None; // Revoke the old producer's unique-caller lease before rebinding.
        }
        if let NetworkCommand::Observe(observing) = &command {
            self.observing = *observing;
        }
        if matches!(&command, NetworkCommand::ProducerRestart)
            && let Some(sender) = self.sender.clone()
            && let Err(error) = self.subscribe(sender).await
        {
            return NetworkState {
                error: Some(OperationError {
                    code: "network-observer".into(),
                    message: error.to_string(),
                }),
                ..Default::default()
            };
        }
        if self.connection.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => {
                    return NetworkState {
                        error: Some(OperationError {
                            code: "session-unavailable".into(),
                            message: error.to_string(),
                        }),
                        ..Default::default()
                    };
                }
            }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation = async {
            let proxy = NetworkProxy::new(connection).await?;
            if !matches!(
                &command,
                NetworkCommand::Observe(false) | NetworkCommand::OpenProvider
            ) && self.observing
            {
                proxy.acquire_ui().await?;
            }
            match command {
                NetworkCommand::Refresh | NetworkCommand::ProducerRestart => Ok(()),
                NetworkCommand::Observe(true) => proxy.acquire_ui().await.map_err(Into::into),
                NetworkCommand::Observe(false) => {
                    if was_observing {
                        proxy.release_ui().await.map_err(Into::into)
                    } else {
                        Ok(())
                    }
                }
                NetworkCommand::SetWireless(enabled) => {
                    proxy.set_wireless(enabled).await.map_err(Into::into)
                }
                NetworkCommand::Scan(device) => proxy.scan(&device).await.map_err(Into::into),
                NetworkCommand::Connect {
                    device,
                    point,
                    ssid,
                    security,
                    hidden,
                    session_only,
                } => proxy
                    .connect(&device, &point, &ssid, &security, hidden, session_only)
                    .await
                    .map(|_| ())
                    .map_err(Into::into),
                NetworkCommand::Activate { profile, device } => proxy
                    .activate(&profile, &device)
                    .await
                    .map(|_| ())
                    .map_err(Into::into),
                NetworkCommand::Disconnect(device) => {
                    proxy.disconnect(&device).await.map_err(Into::into)
                }
                NetworkCommand::Cancel => proxy.cancel().await.map_err(Into::into),
                NetworkCommand::Forget(profile) => proxy.forget(&profile).await.map_err(Into::into),
                NetworkCommand::Autoconnect { profile, enabled } => proxy
                    .set_autoconnect(&profile, enabled)
                    .await
                    .map_err(Into::into),
                NetworkCommand::Answer { token, approved } => {
                    proxy.answer(&token, approved).await.map_err(Into::into)
                }
                NetworkCommand::Secret { token, value } => {
                    let descriptor = templar_client::secret_fd::seal_utf8(&value, 4096)?;
                    proxy
                        .answer_secret(&token, descriptor.into())
                        .await
                        .map_err(Into::into)
                }
                NetworkCommand::OpenProvider => {
                    if self.observing {
                        proxy.release_ui().await?;
                        self.observing = false;
                    }
                    open_provider(connection).await
                }
            }
        };
        let write = tokio::time::timeout(Duration::from_secs(30), operation).await;
        let observed = async {
            let proxy = NetworkProxy::new(connection).await?;
            Ok::<NetworkSnapshot, anyhow::Error>(serde_json::from_str(&proxy.inventory().await?)?)
        };
        let read = tokio::time::timeout(Duration::from_secs(20), observed).await;
        let mut errors = Vec::new();
        match write { Ok(Err(error)) => errors.push(format!("Change refused or incomplete: {error:#}")), Err(_) => errors.push("Change timed out; readback below is the applied state, not confirmation of the request".into()), _ => {} }
        let snapshot = match read {
            Ok(Ok(snapshot)) => Some(snapshot),
            Ok(Err(error)) => {
                errors.push(format!("Readback unavailable: {error:#}"));
                None
            }
            Err(_) => {
                errors.push("Network readback timed out".into());
                None
            }
        };
        NetworkState {
            snapshot,
            error: (!errors.is_empty()).then(|| OperationError {
                code: "network-operation".into(),
                message: errors.join("; "),
            }),
        }
    }
    /// Install event observation before the first inventory read. The parent routes this weak
    /// command sender into the same serialized refresh/write queue that calls run().
    pub async fn subscribe(
        &mut self,
        sender: Weak<tokio::sync::mpsc::UnboundedSender<NetworkCommand>>,
    ) -> Result<()> {
        if self.connection.is_none() {
            self.connection = Some(zbus::Connection::session().await?);
        }
        for watcher in self.watchers.drain(..) {
            watcher.abort();
        }
        self.sender = Some(sender.clone());
        let (ready, registered) = tokio::sync::oneshot::channel();
        let connection = self.connection.as_ref().expect("connected above").clone();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let result = async {
                let proxy = NetworkProxy::new(&connection).await?;
                let mut changes = proxy.receive_changed().await?;
                let bus = zbus::fdo::DBusProxy::new(&connection).await?;
                let mut owners = bus.receive_name_owner_changed().await?;
                if let Some(ready) = ready.take() { let _ = ready.send(Ok(())); }
                loop {
                    tokio::select! {
                        change = changes.next() => {
                            anyhow::ensure!(change.is_some(), "network change stream closed");
                            let Some(sender) = sender.upgrade() else { break };
                            if sender.send(NetworkCommand::Refresh).is_err() { break; }
                        }
                        owner = owners.next() => {
                            let owner = owner.context("session owner observation stream closed")?;
                            if owner.args()?.name().as_str() != "org.templaros.Session" { continue; }
                            let Some(sender) = sender.upgrade() else { break };
                            if sender.send(NetworkCommand::ProducerRestart).is_err() { break; }
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            if let Err(error) = result {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error));
                } else if let Some(sender) = sender.upgrade() {
                    let _ = sender.send(NetworkCommand::ProducerRestart);
                }
            }
        });
        self.watchers.push(task.abort_handle());
        match tokio::time::timeout(Duration::from_secs(10), registered).await {
            Ok(Ok(Ok(()))) => Ok(()),
            result => {
                for watcher in self.watchers.drain(..) {
                    watcher.abort();
                }
                match result {
                    Ok(Ok(Err(error))) => Err(error),
                    Ok(Err(error)) => Err(error.into()),
                    Err(_) => anyhow::bail!("network observer registration timed out"),
                    Ok(Ok(Ok(()))) => unreachable!(),
                }
            }
        }
    }
}
impl Drop for NetworkController {
    fn drop(&mut self) {
        for watcher in self.watchers.drain(..) {
            watcher.abort();
        }
        // Dropping the uniquely owned connection also triggers server-side lease cleanup.
        if self.observing
            && let Some(connection) = self.connection.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(15), async {
                    NetworkProxy::new(&connection).await?.release_ui().await
                })
                .await;
            });
        }
    }
}
async fn open_provider(connection: &zbus::Connection) -> Result<()> {
    let report = ProvidersProxy::new(connection).await?.report().await?;
    let (_, json) = report
        .into_iter()
        .find(|(role, _)| role == "network-status-ui")
        .context("no network provider is declared")?;
    let resolution: crate::providers::Resolution = serde_json::from_str(&json)?;
    let app = match resolution {
        crate::providers::Resolution::Selected { app_id }
        | crate::providers::Resolution::DefaultedByPriority { app_id, .. } => app_id,
        _ => anyhow::bail!("existing network provider is unavailable; select it under Providers"),
    };
    LauncherProxy::new(connection)
        .await?
        .launch(app.as_str(), Vec::new(), "")
        .await?;
    Ok(())
}

pub async fn print() -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = NetworkProxy::new(&connection).await?;
    let state: NetworkSnapshot = serde_json::from_str(&proxy.inventory().await?)?;
    println!(
        "Network: {} · {:?}",
        state.connectivity_label(),
        state.capability
    );
    for device in state.devices {
        println!(
            "{} state={} reason={} firmware-missing={} managed={}",
            device.interface, device.state, device.reason, device.firmware_missing, device.managed
        );
    }
    Ok(())
}

#[derive(Default)]
pub struct NetworkView {
    pub state: NetworkState,
    secret: pixelkit_ui::TextFieldState,
    hidden_ssid: pixelkit_ui::TextFieldState,
    selected_device: Option<String>,
    selected_point: Option<String>,
    hidden: bool,
    hidden_hex: bool,
    security: String,
    session_only: bool,
    prompt: Option<String>,
    scroll: pixelkit_ui::ScrollState,
    heights: Vec<i32>,
    secret_drawn: bool,
    hidden_drawn: bool,
    device_labels: Vec<(String, String)>,
    point_labels: Vec<String>,
    profile_labels: Vec<String>,
    prompt_label: String,
    selected_label: String,
}
impl Drop for NetworkView {
    fn drop(&mut self) {
        self.secret.clear_sensitive();
    }
}
impl NetworkView {
    pub fn leave(&mut self) {
        self.secret.clear_sensitive();
        self.prompt = None;
        self.secret_drawn = false;
        self.hidden_drawn = false;
    }
    pub fn replace(&mut self, state: NetworkState) -> bool {
        use std::fmt::Write;
        let owner_changed = match (&self.state.snapshot, &state.snapshot) {
            (Some(old), Some(new)) => old.revision.owner_epoch != new.revision.owner_epoch,
            (None, None) => false,
            _ => true,
        };
        if owner_changed {
            self.leave();
            self.selected_device = None;
            self.selected_point = None;
            self.selected_label.clear();
        }
        let changed = match (&self.state.snapshot, &state.snapshot) {
            (Some(old), Some(new)) => {
                old.revision.owner_epoch != new.revision.owner_epoch
                    || old.ui_owned != new.ui_owned
                    || old
                        .devices
                        .iter()
                        .map(|device| {
                            (
                                &device.path,
                                device.kind,
                                (40..=100).contains(&device.state),
                            )
                        })
                        .ne(new.devices.iter().map(|device| {
                            (
                                &device.path,
                                device.kind,
                                (40..=100).contains(&device.state),
                            )
                        }))
                    || old
                        .access_points
                        .iter()
                        .map(|point| (&point.path, &point.ssid, point.personal_supported))
                        .ne(new
                            .access_points
                            .iter()
                            .map(|point| (&point.path, &point.ssid, point.personal_supported)))
                    || old
                        .profiles
                        .iter()
                        .map(|profile| &profile.path)
                        .ne(new.profiles.iter().map(|profile| &profile.path))
                    || old
                        .prompts
                        .iter()
                        .map(|prompt| &prompt.token)
                        .ne(new.prompts.iter().map(|prompt| &prompt.token))
            }
            (None, None) => false,
            _ => true,
        };
        let prompt = state
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.prompts.first())
            .map(|prompt| prompt.token.clone());
        if self.prompt != prompt {
            self.secret.clear_sensitive();
            self.prompt = prompt;
        }
        if let Some(snapshot) = &state.snapshot {
            self.device_labels
                .resize_with(snapshot.devices.len(), Default::default);
            for ((label, reason), device) in self.device_labels.iter_mut().zip(&snapshot.devices) {
                label.clear();
                reason.clear();
                let _ = write!(
                    label,
                    "{} · {} · {}",
                    device.interface,
                    match device.kind {
                        1 => "Ethernet / USB tether",
                        2 => "Wi-Fi",
                        _ => "Network device",
                    },
                    templar_client::devices::network_device_state(device.state)
                );
                let _ = write!(
                    reason,
                    "{} ({}) · firmware-missing={} managed={}",
                    templar_client::devices::network_reason(device.reason),
                    device.reason,
                    device.firmware_missing,
                    device.managed
                );
            }
            self.point_labels
                .resize_with(snapshot.access_points.len(), String::new);
            for (label, point) in self.point_labels.iter_mut().zip(&snapshot.access_points) {
                label.clear();
                let _ = write!(
                    label,
                    "{} · {}% · {}",
                    point.label,
                    point.strength,
                    point.security()
                );
            }
            self.profile_labels
                .resize_with(snapshot.profiles.len(), String::new);
            for (label, profile) in self.profile_labels.iter_mut().zip(&snapshot.profiles) {
                label.clear();
                let _ = write!(label, "Saved: {} · {}", profile.label, profile.kind);
            }
            self.prompt_label.clear();
            if let Some(prompt) = snapshot.prompts.first() {
                let _ = write!(self.prompt_label, "{} — {}", prompt.label, prompt.subject);
            }
            if self
                .selected_device
                .as_ref()
                .is_none_or(|path| !snapshot.devices.iter().any(|device| &device.path == path))
            {
                self.selected_device = snapshot
                    .devices
                    .iter()
                    .find(|device| device.kind == 2)
                    .map(|device| device.path.clone());
            }
            if self.selected_point.as_ref().is_some_and(|path| {
                !snapshot
                    .access_points
                    .iter()
                    .any(|point| &point.path == path)
            }) {
                self.selected_point = None;
            }
            if let Some(device) = snapshot
                .devices
                .iter()
                .find(|device| self.selected_device.as_deref() == Some(device.path.as_str()))
            {
                self.selected_label.clone_from(&device.interface);
            }
        }
        self.state = state;
        changed
    }
    pub fn ime_caret(&self) -> Option<pixelkit_raster::Rect> {
        self.secret_drawn
            .then(|| self.secret.caret_area())
            .flatten()
            .or_else(|| {
                self.hidden_drawn
                    .then(|| self.hidden_ssid.caret_area())
                    .flatten()
            })
    }
    pub fn draw(
        &mut self,
        ui: &mut pixelkit_ui::Ui<'_>,
        area: pixelkit_raster::Rect,
    ) -> (Option<NetworkCommand>, bool) {
        use pixelkit_raster::Rect;
        use pixelkit_text::Align;
        let h = ui.px(32);
        let gap = ui.px(8);
        let mut y = area.y;
        let mut command = None;
        let mut layout = false;
        self.secret_drawn = false;
        self.hidden_drawn = false;
        if ui.button(Rect::new(area.x, y, ui.px(90), h), "Refresh") {
            command = Some(NetworkCommand::Refresh);
        }
        if ui.button(
            Rect::new(area.x + ui.px(100), y, ui.px(240), h),
            "Existing network provider",
        ) {
            self.leave();
            command = Some(NetworkCommand::OpenProvider);
        }
        y += h + gap;
        if let Some(error) = &self.state.error {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                &error.message,
                ui.theme.alarm,
                Align::Left,
            );
            y += h + gap;
        }
        let Some(snapshot) = &self.state.snapshot else {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                "Network readback unavailable",
                ui.theme.text_dim,
                Align::Left,
            );
            return (command, layout);
        };
        if let templar_types::settings::Capability::Unavailable { reason }
        | templar_types::settings::Capability::Unsupported { reason } = &snapshot.capability
        {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                reason,
                ui.theme.alarm,
                Align::Left,
            );
            y += h + gap;
        }
        if let Some(error) = &snapshot.operation_error {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                error,
                ui.theme.alarm,
                Align::Left,
            );
            y += h + gap;
        }
        ui.label(
            Rect::new(area.x, y, area.w, h),
            if snapshot.state == 0 {
                "Radio/connectivity readback unavailable"
            } else {
                snapshot.connectivity_label()
            },
            ui.theme.text,
            Align::Left,
        );
        y += h + gap;
        if snapshot.state != 0 {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                if snapshot.wireless_hardware_enabled {
                    "Hardware radio available"
                } else {
                    "Hardware radio blocked/unavailable — check hardware switch/rfkill"
                },
                ui.theme.text_dim,
                Align::Left,
            );
            y += h + gap;
        }
        if !self.selected_label.is_empty() {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                &self.selected_label,
                ui.theme.text_dim,
                Align::Left,
            );
            y += h + gap;
        }
        let mut enabled = snapshot.wireless_enabled;
        if snapshot.ui_owned
            && ui.toggle(
                Rect::new(area.x, y, ui.px(210), h),
                &mut enabled,
                "Wi-Fi software radio",
            )
        {
            command = Some(NetworkCommand::SetWireless(enabled));
        }
        if snapshot.ui_owned
            && ui.button(Rect::new(area.x + ui.px(220), y, ui.px(100), h), "Cancel")
        {
            self.secret.clear_sensitive();
            command = Some(NetworkCommand::Cancel);
        }
        y += h + gap;
        if !snapshot.ui_owned {
            if ui.button(
                Rect::new(area.x, y, ui.px(240), h),
                "Use native controls / prompts",
            ) {
                command = Some(NetworkCommand::Observe(true));
            }
            y += h + gap;
        }
        if let Some(prompt) = snapshot.prompts.first() {
            ui.label(
                Rect::new(area.x, y, area.w, h),
                &self.prompt_label,
                ui.theme.text,
                Align::Left,
            );
            y += h + gap;
            if matches!(
                prompt.kind,
                PromptKind::Password | PromptKind::Pin | PromptKind::Passkey
            ) {
                ui.text_field(
                    &mut self.secret,
                    Rect::new(area.x, y, (area.w - ui.px(190)).max(0), h),
                    "Network password (masked)",
                    false,
                    true,
                );
                self.secret_drawn = true;
                if ui.button(
                    Rect::new(area.right() - ui.px(180), y, ui.px(90), h),
                    "Submit",
                ) {
                    command = Some(NetworkCommand::Secret {
                        token: prompt.token.clone(),
                        value: Zeroizing::new(self.secret.take_text()),
                    });
                }
            }
            if ui.button(
                Rect::new(area.right() - ui.px(80), y, ui.px(80), h),
                "Reject",
            ) {
                self.secret.clear_sensitive();
                command = Some(NetworkCommand::Answer {
                    token: prompt.token.clone(),
                    approved: false,
                });
            }
            return (command, layout);
        }
        let old_hidden = self.hidden;
        if ui.toggle(
            Rect::new(area.x, y, ui.px(220), h),
            &mut self.hidden,
            "Connect a hidden network",
        ) {
            layout = old_hidden != self.hidden;
        }
        y += h + gap;
        if self.hidden && snapshot.ui_owned {
            ui.text_field(
                &mut self.hidden_ssid,
                Rect::new(area.x, y, (area.w - ui.px(150)).max(0), h),
                "SSID text or hex bytes",
                false,
                false,
            );
            self.hidden_drawn = true;
            ui.toggle(
                Rect::new(area.right() - ui.px(140), y, ui.px(140), h),
                &mut self.hidden_hex,
                "Hex bytes",
            );
            y += h + gap;
            for (index, (label, selected_label, value)) in [
                ("Open", "Selected: Open", "open"),
                ("WPA/WPA2", "Selected: WPA/WPA2", "wpa-psk"),
                ("WPA3 SAE", "Selected: WPA3", "sae"),
            ]
            .into_iter()
            .enumerate()
            {
                let selected =
                    self.security == value || (self.security.is_empty() && value == "wpa-psk");
                if ui.button(
                    Rect::new(area.x + ui.px(index as i32 * 120), y, ui.px(112), h),
                    if selected { selected_label } else { label },
                ) {
                    self.security = value.into();
                }
            }
            y += h + gap;
            ui.toggle(
                Rect::new(area.x, y, ui.px(280), h),
                &mut self.session_only,
                "Session-only credential (do not save)",
            );
            if ui.button(
                Rect::new(area.right() - ui.px(100), y, ui.px(100), h),
                "Connect",
            ) {
                match hidden_ssid(self.hidden_ssid.text(), self.hidden_hex) {
                    Ok(ssid) => {
                        if let Some(device) = &self.selected_device {
                            command = Some(NetworkCommand::Connect {
                                device: device.clone(),
                                point: "/".into(),
                                ssid,
                                security: if self.security.is_empty() {
                                    "wpa-psk".into()
                                } else {
                                    self.security.clone()
                                },
                                hidden: true,
                                session_only: self.session_only,
                            });
                        }
                    }
                    Err(error) => {
                        self.state.error = Some(OperationError {
                            code: "ssid-invalid".into(),
                            message: error.to_string(),
                        })
                    }
                }
            }
            y += h + gap;
        }
        self.heights.clear();
        self.heights
            .extend(snapshot.devices.iter().map(|_| ui.px(88)));
        self.heights
            .extend(snapshot.access_points.iter().map(|point| {
                ui.px(
                    if self.selected_point.as_deref() == Some(point.path.as_str()) {
                        126
                    } else {
                        70
                    },
                )
            }));
        self.heights
            .extend(snapshot.profiles.iter().map(|_| ui.px(96)));
        let device_count = snapshot.devices.len();
        let point_count = snapshot.access_points.len();
        ui.scroll_list(
            &mut self.scroll,
            Rect::new(area.x, y, area.w, (area.bottom() - y).max(0)),
            &self.heights,
            |ui, index, rect| {
                if index < device_count {
                    let device = &snapshot.devices[index];
                    let (label, reason) = &self.device_labels[index];
                    if ui.button(Rect::new(rect.x, rect.y, rect.w, h), label) {
                        self.selected_device = Some(device.path.clone());
                        self.selected_label.clone_from(&device.interface);
                    }
                    ui.label(
                        Rect::new(rect.x, rect.y + h, (rect.w - ui.px(190)).max(0), h),
                        reason,
                        ui.theme.text_dim,
                        Align::Left,
                    );
                    if snapshot.ui_owned
                        && device.kind == 2
                        && ui.button(
                            Rect::new(rect.right() - ui.px(180), rect.y + h, ui.px(80), h),
                            "Scan",
                        )
                    {
                        command = Some(NetworkCommand::Scan(device.path.clone()));
                    }
                    if snapshot.ui_owned
                        && device.state >= 40
                        && device.state <= 100
                        && ui.button(
                            Rect::new(rect.right() - ui.px(95), rect.y + h, ui.px(95), h),
                            "Disconnect",
                        )
                    {
                        command = Some(NetworkCommand::Disconnect(device.path.clone()));
                    }
                } else if index < device_count + point_count {
                    let point = &snapshot.access_points[index - device_count];
                    if ui.button(
                        Rect::new(rect.x, rect.y, rect.w, h),
                        &self.point_labels[index - device_count],
                    ) {
                        layout |= self.selected_point.as_deref() != Some(point.path.as_str());
                        self.selected_point = Some(point.path.clone());
                        self.selected_device = Some(point.device.clone());
                        if let Some(device) = snapshot
                            .devices
                            .iter()
                            .find(|device| device.path == point.device)
                        {
                            self.selected_label.clone_from(&device.interface);
                        }
                    }
                    if self.selected_point.as_deref() == Some(point.path.as_str())
                        && snapshot.ui_owned
                    {
                        if !point.personal_supported {
                            ui.label(
                                Rect::new(rect.x, rect.y + h + gap, rect.w, h),
                                "Enterprise/legacy security: use Existing network provider",
                                ui.theme.text_dim,
                                Align::Left,
                            );
                            return;
                        }
                        ui.toggle(
                            Rect::new(rect.x, rect.y + h + gap, ui.px(300), h),
                            &mut self.session_only,
                            "Session-only credential (do not save)",
                        );
                        if ui.button(
                            Rect::new(rect.right() - ui.px(100), rect.y + h + gap, ui.px(100), h),
                            "Connect",
                        ) {
                            command = Some(NetworkCommand::Connect {
                                device: point.device.clone(),
                                point: point.path.clone(),
                                ssid: point.ssid.clone(),
                                security: if !point.privacy {
                                    "open"
                                } else if point.rsn_flags & 0x400 != 0 {
                                    "sae"
                                } else {
                                    "wpa-psk"
                                }
                                .into(),
                                hidden: false,
                                session_only: self.session_only,
                            });
                        }
                    }
                } else {
                    let profile = &snapshot.profiles[index - device_count - point_count];
                    ui.label(
                        Rect::new(rect.x, rect.y, rect.w, h),
                        &self.profile_labels[index - device_count - point_count],
                        ui.theme.text,
                        Align::Left,
                    );
                    if snapshot.ui_owned {
                        if ui.button(
                            Rect::new(rect.x, rect.y + h + gap, ui.px(90), h),
                            "Activate",
                        ) && let Some(device) = &self.selected_device
                        {
                            command = Some(NetworkCommand::Activate {
                                profile: profile.path.clone(),
                                device: device.clone(),
                            });
                        }
                        if ui.button(
                            Rect::new(rect.x + ui.px(100), rect.y + h + gap, ui.px(80), h),
                            "Forget",
                        ) {
                            command = Some(NetworkCommand::Forget(profile.path.clone()));
                        }
                        let mut autoconnect = profile.autoconnect;
                        if ui.toggle(
                            Rect::new(rect.x + ui.px(190), rect.y + h + gap, ui.px(200), h),
                            &mut autoconnect,
                            "Autoconnect",
                        ) {
                            command = Some(NetworkCommand::Autoconnect {
                                profile: profile.path.clone(),
                                enabled: autoconnect,
                            });
                        }
                    }
                }
            },
        );
        (command, layout)
    }
}
fn hidden_ssid(value: &str, hexadecimal: bool) -> Result<Vec<u8>> {
    let bytes = if hexadecimal {
        anyhow::ensure!(
            value.len() % 2 == 0 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "hex SSID requires complete hexadecimal byte pairs"
        );
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
            .collect::<Result<Vec<_>>>()?
    } else {
        value.as_bytes().to_vec()
    };
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= 32,
        "SSID must contain 1..32 bytes"
    );
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_identity_supports_lossless_bytes_and_refuses_truncation() {
        assert_eq!(hidden_ssid("ff0061", true).unwrap(), [255, 0, 97]);
        assert!(hidden_ssid("f", true).is_err());
        assert!(hidden_ssid(&"x".repeat(33), false).is_err());
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;
    fn snapshot() -> NetworkSnapshot {
        NetworkSnapshot {
            revision: templar_types::settings::Revision::new_owner(),
            capability: templar_types::settings::Capability::Available {
                requires_restart: false,
            },
            ui_owned: true,
            state: 70,
            connectivity: 4,
            wireless_enabled: true,
            wireless_hardware_enabled: true,
            devices: vec![templar_client::devices::NetworkDevice {
                path: "/device".into(),
                interface: "wlan0".into(),
                kind: 2,
                state: 100,
                reason: 0,
                firmware_missing: false,
                managed: true,
                active_connection: "/active".into(),
            }],
            access_points: vec![templar_client::devices::AccessPoint {
                path: "/ap".into(),
                device: "/device".into(),
                ssid: vec![255, 0, 97],
                label: "SSID display only".into(),
                strength: 40,
                privacy: true,
                wpa_flags: 0,
                rsn_flags: 0x100,
                personal_supported: true,
            }],
            profiles: Vec::new(),
            prompts: vec![templar_client::devices::DevicePrompt {
                token: "fixture-token".into(),
                subject: "/connection:802-11-wireless-security".into(),
                label: "Fixture network".into(),
                kind: PromptKind::Password,
                deadline_unix_ms: 0,
                entered: 0,
            }],
            operation_error: None,
        }
    }
    #[test]
    fn signal_churn_preserves_focus_and_secret_but_owner_replacement_clears_it() {
        let mut view = NetworkView::default();
        let mut observed = snapshot();
        assert!(view.replace(NetworkState {
            snapshot: Some(observed.clone()),
            error: None
        }));
        view.secret.set_text("fixture-password");
        view.selected_point = Some("/ap".into());
        observed.access_points[0].strength = 60;
        observed.revision.generation += 1;
        assert!(!view.replace(NetworkState {
            snapshot: Some(observed.clone()),
            error: None
        }));
        assert_eq!(view.secret.text(), "fixture-password");
        assert_eq!(view.selected_point.as_deref(), Some("/ap"));
        observed.revision = templar_types::settings::Revision::new_owner();
        assert!(view.replace(NetworkState {
            snapshot: Some(observed),
            error: None
        }));
        assert!(view.secret.text().is_empty());
        assert!(view.selected_point.is_none());
        view.secret.set_text("second-fixture");
        view.leave();
        assert!(view.secret.text().is_empty());
    }
}
