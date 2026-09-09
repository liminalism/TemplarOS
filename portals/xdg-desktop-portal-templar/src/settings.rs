//! `org.freedesktop.impl.portal.Settings`.
//!
//! Fully real: every value comes straight from `Settings1`. TemplarOS has no distinct
//! per-toolkit namespaces yet (no `org.freedesktop.appearance` equivalents in the schema), so
//! every key is exposed under one namespace, `"org.templaros.settings"`, keyed exactly as
//! `Settings1.List` names it (`shell.dock.grouping`, `resources.mode`, ...). `SettingChanged` is
//! forwarded from `Settings1`'s own `Changed` signal by a background task started alongside the
//! interface (`spawn_change_forwarder`) — a real subscription, not a stub.

use crate::{clients::SessionSettingsClient, request::owned_value};
use std::{collections::HashMap, sync::Arc};
use templar_session_core::settings::SettingValue;
use zbus::zvariant::OwnedValue;

/// The one namespace every TemplarOS setting is exposed under.
pub const NAMESPACE: &str = "org.templaros.settings";

/// D-Bus interface name, needed by the change-forwarder task to emit on the right interface.
pub const INTERFACE: &str = "org.freedesktop.impl.portal.Settings";
/// Object path this backend serves every interface at.
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// Convert a stored setting value into the `v` this portal interface returns.
pub fn to_variant(value: &SettingValue) -> OwnedValue {
    match value {
        SettingValue::Bool(v) => owned_value(*v),
        SettingValue::Int(v) => owned_value(*v),
        SettingValue::Float(v) => owned_value(*v),
        SettingValue::Str(v) | SettingValue::Choice(v) => owned_value(v.clone()),
        SettingValue::StrList(v) => owned_value(v.clone()),
    }
}

/// Hand-rolled `*`-glob match (no dependency pulled in for something this small): `*` matches
/// any run of characters, everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some(b'*') => {
                inner(&pattern[1..], text) || (!text.is_empty() && inner(pattern, &text[1..]))
            }
            Some(&byte) => text.first() == Some(&byte) && inner(&pattern[1..], &text[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// Whether `namespace` should be included for a `ReadAll(namespaces)` call: an empty list means
/// "everything", otherwise at least one requested pattern must glob-match.
pub fn namespace_requested(namespaces: &[String], namespace: &str) -> bool {
    namespaces.is_empty()
        || namespaces
            .iter()
            .any(|pattern| glob_match(pattern, namespace))
}

/// `org.freedesktop.impl.portal.Settings`.
#[derive(Debug)]
pub struct SettingsApi {
    pub settings: Arc<dyn SessionSettingsClient>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl SettingsApi {
    #[zbus(name = "ReadAll")]
    async fn read_all(
        &self,
        namespaces: Vec<String>,
    ) -> zbus::fdo::Result<HashMap<String, HashMap<String, OwnedValue>>> {
        let mut out = HashMap::new();
        if namespace_requested(&namespaces, NAMESPACE) {
            out.insert(NAMESPACE.to_owned(), self.snapshot().await?);
        }
        Ok(out)
    }

    #[zbus(name = "Read")]
    async fn read(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if namespace != NAMESPACE {
            return Err(zbus::fdo::Error::Failed(format!(
                "unknown namespace {namespace}"
            )));
        }
        self.snapshot()
            .await?
            .remove(key)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("unknown key {key}")))
    }

    #[zbus(signal, name = "SettingChanged")]
    pub async fn setting_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        namespace: String,
        key: String,
        value: OwnedValue,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    #[allow(clippy::unused_self)]
    fn version(&self) -> u32 {
        1
    }
}

impl SettingsApi {
    async fn snapshot(&self) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let values = self
            .settings
            .list()
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok(values
            .into_iter()
            .filter_map(|(key, value_json)| {
                serde_json::from_str::<SettingValue>(&value_json)
                    .ok()
                    .map(|value| (key, to_variant(&value)))
            })
            .collect())
    }
}

/// Subscribe to `Settings1`'s own `Changed` signal and re-emit `SettingChanged` on `connection`
/// for as long as the underlying stream keeps producing signals. Spawned once at startup;
/// exits quietly if `Settings1` is unreachable (no TemplarOS session, or sessiond not running yet).
pub async fn spawn_change_forwarder(connection: zbus::Connection) {
    use futures_util::StreamExt as _;
    tokio::spawn(async move {
        let session_settings = match templar_client::proxies::SettingsProxy::new(&connection).await
        {
            Ok(proxy) => proxy,
            Err(error) => {
                tracing::debug!(%error, "Settings1 not reachable; SettingChanged forwarding disabled");
                return;
            }
        };
        let Ok(mut changes) = session_settings.receive_changed().await else {
            tracing::debug!("Settings1.Changed not reachable; SettingChanged forwarding disabled");
            return;
        };
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            let key = args.key().clone();
            let Ok(value_json) = session_settings.get(&key).await else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<SettingValue>(&value_json) else {
                continue;
            };
            let body = (NAMESPACE.to_owned(), key, to_variant(&value));
            if let Err(error) = connection
                .emit_signal(None::<&str>, PATH, INTERFACE, "SettingChanged", &body)
                .await
            {
                tracing::debug!(%error, "failed to forward SettingChanged");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    #[test]
    fn glob_matching() {
        assert!(glob_match(
            "org.templaros.settings",
            "org.templaros.settings"
        ));
        assert!(glob_match("org.templaros.*", "org.templaros.settings"));
        assert!(glob_match("*", "org.templaros.settings"));
        assert!(!glob_match("org.gnome.*", "org.templaros.settings"));
        assert!(!glob_match(
            "org.templaros.settings.x",
            "org.templaros.settings"
        ));
    }

    #[test]
    fn empty_namespace_list_means_everything() {
        assert!(namespace_requested(&[], NAMESPACE));
        assert!(namespace_requested(&["org.templaros.*".into()], NAMESPACE));
        assert!(!namespace_requested(&["org.gnome.*".into()], NAMESPACE));
    }

    #[test]
    fn every_setting_value_kind_converts() {
        for value in [
            SettingValue::Bool(true),
            SettingValue::Int(3),
            SettingValue::Float(1.5),
            SettingValue::Str("x".into()),
            SettingValue::Choice("by-application".into()),
            SettingValue::StrList(vec!["a".into(), "b".into()]),
        ] {
            let variant = to_variant(&value);
            assert!(!variant.value_signature().to_string().is_empty());
        }
    }

    #[derive(Debug, Default)]
    struct FakeSettings(Vec<(String, String)>);

    #[async_trait]
    impl SessionSettingsClient for FakeSettings {
        async fn list(&self) -> anyhow::Result<Vec<(String, String)>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn read_all_reflects_the_underlying_settings() {
        let api = SettingsApi {
            settings: Arc::new(FakeSettings(vec![(
                "shell.dock.grouping".into(),
                serde_json::to_string(&SettingValue::Choice("by-application".into())).unwrap(),
            )])),
        };
        let all = api.read_all(Vec::new()).await.unwrap();
        let namespace = &all[NAMESPACE];
        assert_eq!(
            String::try_from(namespace["shell.dock.grouping"].clone()).unwrap(),
            "by-application"
        );
    }

    #[tokio::test]
    async fn read_unknown_key_fails() {
        let api = SettingsApi {
            settings: Arc::new(FakeSettings::default()),
        };
        assert!(api.read(NAMESPACE, "nope").await.is_err());
        assert!(api.read("org.gnome.desktop", "x").await.is_err());
    }
}
