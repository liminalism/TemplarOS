//! Subscribe before reading shared appearance; notification delivery wakes the existing UI loop.
use crate::proxies::SettingsProxy;
use templar_types::appearance::{Appearance, THEME_KEY, ACCENT_KEY, REDUCED_MOTION_KEY};
use tokio::sync::{oneshot, watch};

#[derive(Debug)]
pub struct AppearanceObserver {
    updates: watch::Receiver<Option<Result<Appearance, String>>>,
    stop: Option<oneshot::Sender<()>>,
    terminal_seen: bool,
}
impl AppearanceObserver {
    pub fn new(wake: impl Fn() + Send + 'static) -> Self {
        let (sender, updates) = watch::channel(None);
        let (stop, mut stopped) = oneshot::channel();
        std::thread::spawn(move || {
            let publish = |value| { sender.send_replace(Some(value)); wake(); };
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => { publish(Err(error.to_string())); return; }
            };
            runtime.block_on(async {
                let observe = async {
                    use futures_util::StreamExt;
                    let connection = zbus::Connection::session().await.map_err(|error| error.to_string())?;
                    let proxy = SettingsProxy::new(&connection).await.map_err(|error| error.to_string())?;
                    let mut changes = proxy.receive_changed().await.map_err(|error| error.to_string())?;
                    let bus = zbus::fdo::DBusProxy::new(&connection).await.map_err(|error| error.to_string())?;
                    let mut owners = bus.receive_name_owner_changed().await.map_err(|error| error.to_string())?;
                    publish(read(&proxy).await);
                    loop {
                        tokio::select! {
                            signal = changes.next() => {
                                let Some(signal) = signal else { break; };
                                let args = signal.args().map_err(|error| error.to_string())?;
                                if ![THEME_KEY, ACCENT_KEY, REDUCED_MOTION_KEY].contains(&args.key().as_str()) { continue; }
                            }
                            owner = owners.next() => {
                                let Some(owner) = owner else { break; };
                                let args = owner.args().map_err(|error| error.to_string())?;
                                if args.name().as_str() != "org.templaros.Session" { continue; }
                            }
                        }
                        publish(read(&proxy).await);
                    }
                    Err::<(), String>("Appearance observation ended".into())
                };
                tokio::select! {
                    result = observe => { if let Err(error) = result { publish(Err(error)); } },
                    _ = &mut stopped => {}
                }
            });
        });
        Self { updates, stop: Some(stop), terminal_seen: false }
    }
    pub fn take_update(&mut self) -> Option<Result<Appearance, String>> {
        let changed = match self.updates.has_changed() {
            Ok(changed) => changed,
            Err(_) if !self.terminal_seen => { self.terminal_seen = true; true }
            Err(_) => false,
        };
        if changed { self.updates.borrow_and_update().clone() } else { None }
    }
}
impl Drop for AppearanceObserver {
    fn drop(&mut self) { if let Some(stop) = self.stop.take() { let _ = stop.send(()); } }
}
async fn read(proxy: &SettingsProxy<'_>) -> Result<Appearance, String> {
    use templar_types::settings::{Observation, SettingObservation};
    let snapshot: Observation<Vec<SettingObservation<serde_json::Value>>> =
        serde_json::from_str(&proxy.inspect().await.map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let get = |key| snapshot.value.iter().find(|entry| entry.key == key)
        .map(|entry| &entry.value).ok_or_else(|| format!("Appearance snapshot omitted {key}"));
    Ok(Appearance::from_settings(choice(get(THEME_KEY)?)?, choice(get(ACCENT_KEY)?)?,
        boolean(get(REDUCED_MOTION_KEY)?)?))
}
fn choice(value: &serde_json::Value) -> Result<&str, String> {
    (value.get("type").and_then(serde_json::Value::as_str) == Some("choice"))
        .then(|| value.get("value").and_then(serde_json::Value::as_str)).flatten()
        .ok_or_else(|| "Appearance value is not a choice".to_owned())
}
fn boolean(value: &serde_json::Value) -> Result<bool, String> {
    (value.get("type").and_then(serde_json::Value::as_str) == Some("bool"))
        .then(|| value.get("value").and_then(serde_json::Value::as_bool)).flatten()
        .ok_or_else(|| "Reduced-motion value is not a boolean".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_tagged_values_are_read_without_coercing_unknown_shapes() {
        assert_eq!(choice(&serde_json::json!({"type":"choice","value":"dark"})).unwrap(), "dark");
        assert!(boolean(&serde_json::json!({"type":"bool","value":true})).unwrap());
        assert!(choice(&serde_json::json!({"Choice":"dark"})).is_err());
        assert!(boolean(&serde_json::json!({"type":"bool","value":"true"})).is_err());
    }

    #[test]
    fn observer_keeps_only_latest_state_and_reports_terminal_error_once() {
        let (sender, updates) = watch::channel(None);
        let mut observer = AppearanceObserver { updates, stop: None, terminal_seen: false };
        sender.send_replace(Some(Ok(Appearance::default())));
        let dark = Appearance::from_settings("dark", "blue", true);
        sender.send_replace(Some(Ok(dark)));
        assert_eq!(observer.take_update(), Some(Ok(dark)));
        assert_eq!(observer.take_update(), None);
        sender.send_replace(Some(Err("subscription closed".to_owned())));
        drop(sender);
        assert_eq!(observer.take_update(), Some(Err("subscription closed".to_owned())));
        assert_eq!(observer.take_update(), None);
    }
}
