//! Supported toolkit effects are separate from durable settings and native palette observation.
use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc};
use templar_session_core::settings::{SettingValue, SettingsSchema, SettingsValues};
use templar_types::appearance::Appearance;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Debug)]
pub struct AppearanceEffects {
    gate: Arc<Mutex<()>>,
    schema: Arc<RwLock<SettingsSchema>>,
    values: Arc<RwLock<SettingsValues>>,
    config_home: Option<PathBuf>,
}

impl AppearanceEffects {
    pub fn new(schema: Arc<RwLock<SettingsSchema>>, values: Arc<RwLock<SettingsValues>>,
        config_home: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self { gate: Arc::new(Mutex::new(())), schema, values, config_home })
    }

    pub async fn lock(&self) -> OwnedMutexGuard<()> { self.gate.clone().lock_owned().await }

    pub fn affects(key: &str) -> bool {
        matches!(key, "shell.appearance.theme" | "shell.appearance.accent" |
            "shell.appearance.third-party" | "shell.home-field.reduced-motion")
    }

    /// The blocking worker owns the mutation gate even if the caller disappears while awaiting it.
    /// A subsequent committed edit cannot race an older configuration replacement.
    pub async fn reconcile(self: &Arc<Self>, guard: OwnedMutexGuard<()>) -> Result<()> {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let schema = owner.schema.read();
            let values = owner.values.read();
            let get = |key| values.stored(key).or_else(|| schema.specs.get(key).map(|spec| &spec.default))
                .with_context(|| format!("appearance setting {key} is unavailable"));
            let choice = |key| match get(key)? {
                SettingValue::Choice(value) => Ok(value.as_str()),
                _ => anyhow::bail!("appearance setting {key} is not a choice"),
            };
            let boolean = |key| match get(key)? {
                SettingValue::Bool(value) => Ok(*value),
                _ => anyhow::bail!("appearance setting {key} is not a boolean"),
            };
            let enabled = boolean("shell.appearance.third-party")?;
            let appearance = Appearance::from_settings(choice("shell.appearance.theme")?,
                choice("shell.appearance.accent")?, boolean("shell.home-field.reduced-motion")?);
            drop(values);
            drop(schema);
            match (&owner.config_home, enabled) {
                (Some(home), true) => { templar_session_core::appearance::apply(home, appearance)?; }
                (Some(home), false) => { templar_session_core::appearance::restore(home)?; }
                (None, true) => anyhow::bail!("XDG configuration home is unavailable"),
                (None, false) => {}
            }
            Ok(())
        }).await.context("appearance configuration worker failed")?
    }
}
