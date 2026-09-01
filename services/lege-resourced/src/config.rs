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
    /// Reserve as a fraction of `MemTotal`, for a machine whose shape the defaults get wrong.
    /// Ignored when the absolute value is also set — see [`load_for_memory`].
    pub reserve_fraction: Option<f64>,
    pub critical_reserve_fraction: Option<f64>,
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
///
/// # Errors
/// If the file exists and cannot be read or parsed.
pub fn load(path: &Path) -> Result<GovernorConfig> {
    load_onto(path, GovernorConfig::default())
}

/// [`load`] against defaults scaled to this machine's `MemTotal`.
///
/// Precedence, highest first: an absolute value in the file, a fraction in the file, the scaled
/// default. Absolute wins because it is the more specific statement — someone who wrote
/// `reserve_bytes = 2147483648` means two gigabytes, not "two gigabytes unless you disagree" —
/// and a fraction is still an improvement on a literal for anyone moving a config between
/// machines.
///
/// # Errors
/// As [`load`].
pub fn load_for_memory(path: &Path, mem_total: u64) -> Result<GovernorConfig> {
    let scaled = GovernorConfig::for_total_memory(mem_total);
    let mut config = load_onto(path, scaled)?;
    let file = read_file(path)?;
    if file.reserve_bytes.is_none()
        && let Some(fraction) = file.reserve_fraction
    {
        config.reserve_bytes = fraction_of(mem_total, fraction);
    }
    if file.critical_reserve_bytes.is_none()
        && let Some(fraction) = file.critical_reserve_fraction
    {
        config.critical_reserve_bytes = fraction_of(mem_total, fraction);
    }
    Ok(config)
}

#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
fn fraction_of(mem_total: u64, fraction: f64) -> u64 {
    (mem_total as f64 * fraction.clamp(0.0, 1.0)) as u64
}

fn load_onto(path: &Path, base: GovernorConfig) -> Result<GovernorConfig> {
    Ok(read_file(path)?.apply_to(base))
}

fn read_file(path: &Path) -> Result<ConfigFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
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

    fn write(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lege-resourced-cfg-{name}-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn an_absolute_value_beats_a_fraction_beats_the_scaled_default() {
        // Someone who wrote `reserve_bytes = 2 GiB` means two gigabytes, not "two gigabytes
        // unless you disagree"; a fraction is still better than a literal for a config that
        // travels between machines; and with neither, the machine's own size decides.
        let mem_total = 8 * crate::policy::GIB;

        let both = write(
            "both",
            "reserve_bytes = 2147483648
reserve_fraction = 0.5
",
        );
        assert_eq!(
            load_for_memory(&both, mem_total).unwrap().reserve_bytes,
            2 * crate::policy::GIB
        );

        let fraction = write(
            "fraction",
            "reserve_fraction = 0.5
",
        );
        assert_eq!(
            load_for_memory(&fraction, mem_total).unwrap().reserve_bytes,
            4 * crate::policy::GIB
        );

        let empty = write("empty", "");
        assert_eq!(
            load_for_memory(&empty, mem_total).unwrap().reserve_bytes,
            GovernorConfig::for_total_memory(mem_total).reserve_bytes
        );

        for path in [both, fraction, empty] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn a_nonsense_fraction_cannot_produce_a_nonsense_reserve() {
        let path = write(
            "silly",
            "reserve_fraction = 4.0
critical_reserve_fraction = -1.0
",
        );
        let config = load_for_memory(&path, 8 * crate::policy::GIB).unwrap();
        assert_eq!(config.reserve_bytes, 8 * crate::policy::GIB);
        assert_eq!(config.critical_reserve_bytes, 0);
        let _ = std::fs::remove_file(path);
    }

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
