//! Loading [`GovernorConfig`] from a TOML file.
//!
//! `GovernorConfig`'s thresholds and [`VictimWeights`](crate::policy::VictimWeights) are
//! explicitly starting values (planning doc 02 §5) — this is the seam that lets them be replaced
//! once recorded traces (`crate::trace`) show what they should be, without a rebuild. Every field
//! is optional in the file; anything absent keeps the built-in default. Durations are plain
//! milliseconds rather than a serialized `Duration`, so the file stays hand-editable.

use crate::policy::{GovernorConfig, VictimWeights};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigFile {
    pub reserve_bytes: Option<u64>,
    pub critical_reserve_bytes: Option<u64>,
    pub elevated_some_avg10: Option<f64>,
    pub severe_full_avg10: Option<f64>,
    pub critical_full_avg10: Option<f64>,
    pub emergency_kill_delay_ms: Option<u64>,
    pub minimum_high_limit: Option<u64>,
    pub hysteresis_downgrade_ticks: Option<u32>,
    #[serde(default)]
    pub victim_weights: VictimWeightsFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VictimWeightsFile {
    pub memory_gib: Option<f64>,
    pub growth_gib_per_second: Option<f64>,
    pub full_psi_avg10: Option<f64>,
    pub disposable_bonus: Option<f64>,
    pub visible_protection: Option<f64>,
    pub foreground_protection: Option<f64>,
    pub unsaved_protection: Option<f64>,
}

impl ConfigFile {
    /// Merge onto `base` (usually [`GovernorConfig::default`]): a present field overrides,
    /// an absent one keeps `base`'s value.
    pub fn apply_to(self, base: GovernorConfig) -> GovernorConfig {
        GovernorConfig {
            reserve_bytes: self.reserve_bytes.unwrap_or(base.reserve_bytes),
            critical_reserve_bytes: self
                .critical_reserve_bytes
                .unwrap_or(base.critical_reserve_bytes),
            elevated_some_avg10: self.elevated_some_avg10.unwrap_or(base.elevated_some_avg10),
            severe_full_avg10: self.severe_full_avg10.unwrap_or(base.severe_full_avg10),
            critical_full_avg10: self.critical_full_avg10.unwrap_or(base.critical_full_avg10),
            emergency_kill_delay: self
                .emergency_kill_delay_ms
                .map_or(base.emergency_kill_delay, Duration::from_millis),
            minimum_high_limit: self.minimum_high_limit.unwrap_or(base.minimum_high_limit),
            hysteresis_downgrade_ticks: self
                .hysteresis_downgrade_ticks
                .unwrap_or(base.hysteresis_downgrade_ticks),
            victim_weights: VictimWeights {
                memory_gib: self
                    .victim_weights
                    .memory_gib
                    .unwrap_or(base.victim_weights.memory_gib),
                growth_gib_per_second: self
                    .victim_weights
                    .growth_gib_per_second
                    .unwrap_or(base.victim_weights.growth_gib_per_second),
                full_psi_avg10: self
                    .victim_weights
                    .full_psi_avg10
                    .unwrap_or(base.victim_weights.full_psi_avg10),
                disposable_bonus: self
                    .victim_weights
                    .disposable_bonus
                    .unwrap_or(base.victim_weights.disposable_bonus),
                visible_protection: self
                    .victim_weights
                    .visible_protection
                    .unwrap_or(base.victim_weights.visible_protection),
                foreground_protection: self
                    .victim_weights
                    .foreground_protection
                    .unwrap_or(base.victim_weights.foreground_protection),
                unsaved_protection: self
                    .victim_weights
                    .unsaved_protection
                    .unwrap_or(base.victim_weights.unsaved_protection),
            },
        }
    }
}

/// Load and merge a TOML override at `path` onto [`GovernorConfig::default`]. A missing file is
/// not an error — it means "use the defaults" — but a present, unparsable one is.
pub fn load(path: &Path) -> Result<GovernorConfig> {
    let base = GovernorConfig::default();
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: ConfigFile =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            Ok(file.apply_to(base))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(base),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// `$XDG_CONFIG_HOME/legeos/resourced.toml` (falling back to `~/.config`, then `/etc/legeos`).
pub fn default_path() -> PathBuf {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/etc"));
    config_home.join("legeos").join("resourced.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let path =
            std::env::temp_dir().join(format!("lege-resourced-cfg-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let config = load(&path).unwrap();
        assert_eq!(config, GovernorConfig::default());
    }

    #[test]
    fn partial_override_only_replaces_named_fields() {
        let dir = std::env::temp_dir().join(format!("lege-resourced-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resourced.toml");
        std::fs::write(
            &path,
            "critical_full_avg10 = 5.0\n\n[victim_weights]\nmemory_gib = 1.0\n",
        )
        .unwrap();

        let config = load(&path).unwrap();
        let default = GovernorConfig::default();
        assert!((config.critical_full_avg10 - 5.0).abs() < f64::EPSILON);
        assert!((config.victim_weights.memory_gib - 1.0).abs() < f64::EPSILON);
        // Everything else keeps the default.
        assert_eq!(config.reserve_bytes, default.reserve_bytes);
        assert!(
            (config.victim_weights.growth_gib_per_second
                - default.victim_weights.growth_gib_per_second)
                .abs()
                < f64::EPSILON
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let dir =
            std::env::temp_dir().join(format!("lege-resourced-cfg-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resourced.toml");
        std::fs::write(&path, "not_a_real_field = 1\n").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emergency_kill_delay_is_read_as_milliseconds() {
        let dir =
            std::env::temp_dir().join(format!("lege-resourced-cfg-ms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resourced.toml");
        std::fs::write(&path, "emergency_kill_delay_ms = 7500\n").unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.emergency_kill_delay, Duration::from_millis(7500));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
