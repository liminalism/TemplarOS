//! `org.legeos.Battery1`: what the battery is doing, read from sysfs.
//!
//! Not upower. Every figure below is a file the kernel already exposes and this service can read
//! unprivileged, so depending on another daemon would buy nothing and add a way for the battery
//! to be unreadable. Nothing here writes, so there is no privilege question either.
//!
//! The sysfs root is a field rather than a constant, which is the whole of what makes this
//! testable: a test points it at a fixture tree of the same shape and never reads
//! `/sys/class/power_supply`. Being able to run the real code over a fake `/sys` is also the only
//! way to have tests for a two-battery laptop, a desktop, and a battery whose kernel reports no
//! current — none of which is the machine anyone is developing on.

use crate::apis::failed;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zbus::fdo;

/// Where the kernel publishes power supplies on a real machine.
pub const SYSFS_POWER_SUPPLY: &str = "/sys/class/power_supply";

/// What `Battery1.Get` answers.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatteryState {
    /// Whether this machine has a battery at all. A desktop is `false`, and that is a fact about
    /// the machine rather than a failure to read it.
    pub present: bool,
    /// Charge across every battery, 0..=100. `None` when no battery reports one.
    pub percent: Option<u32>,
    /// The kernel's own word: `charging`, `discharging`, `full`, `not charging`, `unknown`.
    /// Passed through rather than flattened, because "full" and "not charging" are different
    /// things to say to a person.
    pub status: String,
    /// Whether a mains supply is online. Answered even with no battery: "plugged in" is still
    /// true of a desktop.
    pub on_mains: bool,
    /// `None` unless the kernel has the figures to compute it. A battery reporting no current
    /// draw cannot be given a time remaining, and inventing one is worse than an empty field.
    pub time_to_empty_minutes: Option<u32>,
    pub time_to_full_minutes: Option<u32>,
}

/// Reads batteries out of a sysfs-shaped directory.
#[derive(Clone, Debug)]
pub struct SysfsBatteries {
    root: PathBuf,
}

impl SysfsBatteries {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The real `/sys/class/power_supply`.
    pub fn system() -> Self {
        Self::new(SYSFS_POWER_SUPPLY)
    }

    /// Every supply directory, in name order so two batteries are summed the same way twice.
    fn supplies(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        paths
    }

    pub fn read(&self) -> BatteryState {
        let mut state = BatteryState {
            status: "unknown".to_owned(),
            ..BatteryState::default()
        };
        let mut charge_now = 0u64;
        let mut charge_full = 0u64;
        let mut statuses: Vec<String> = Vec::new();
        let mut to_empty = 0u32;
        let mut to_full = 0u32;
        let mut had_to_empty = false;
        let mut had_to_full = false;
        let mut percents: Vec<u32> = Vec::new();

        for supply in self.supplies() {
            match field(&supply, "type").as_deref() {
                Some("Battery") => {}
                // "Mains" is the usual name; a USB-PD supply calls itself "USB" and is still what
                // a person means by plugged in.
                Some("Mains" | "USB") => {
                    if field(&supply, "online").as_deref() == Some("1") {
                        state.on_mains = true;
                    }
                    continue;
                }
                _ => continue,
            }
            // A battery slot with no battery in it reports present=0 and must not count.
            if field(&supply, "present").as_deref() == Some("0") {
                continue;
            }
            state.present = true;
            if let Some(status) = field(&supply, "status") {
                statuses.push(status.to_lowercase());
            }
            if let Some(percent) = number(&supply, "capacity") {
                percents.push(percent.min(100) as u32);
            }
            // energy_* (Wh) and charge_* (Ah) are the same shape in different units; whichever a
            // driver publishes, the ratio is the same and that is all this needs.
            let now = number(&supply, "energy_now").or_else(|| number(&supply, "charge_now"));
            let full = number(&supply, "energy_full").or_else(|| number(&supply, "charge_full"));
            if let (Some(now), Some(full)) = (now, full) {
                charge_now += now;
                charge_full += full;
            }
            if let Some(minutes) = number(&supply, "time_to_empty_now") {
                had_to_empty = true;
                to_empty += minutes_from_sysfs(minutes);
            }
            if let Some(minutes) = number(&supply, "time_to_full_now") {
                had_to_full = true;
                to_full += minutes_from_sysfs(minutes);
            }
        }

        state.status = combine_statuses(&statuses);
        state.percent = combined_percent(&percents, charge_now, charge_full);
        // A kernel that does not publish a time estimate does not get one invented here. Deriving
        // one from power_now would mean claiming an instantaneous draw is a prediction, and the
        // number it produces swings by hours between two reads a second apart.
        state.time_to_empty_minutes = had_to_empty.then_some(to_empty);
        state.time_to_full_minutes = had_to_full.then_some(to_full);
        state
    }
}

/// Several batteries, one figure. Prefer the charge ratio, because summing two `capacity`
/// percentages over batteries of different sizes is arithmetic on the wrong quantity — a 90%
/// 20 Wh cell and a 10% 80 Wh cell average to 50% and hold 26 Wh of 100.
fn combined_percent(percents: &[u32], charge_now: u64, charge_full: u64) -> Option<u32> {
    if charge_full > 0 {
        // u128 so the multiply cannot overflow on a µWh-scale figure.
        let ratio = u128::from(charge_now) * 100 / u128::from(charge_full);
        return Some(ratio.min(100) as u32);
    }
    match percents {
        [] => None,
        many => Some(
            (many.iter().map(|percent| u64::from(*percent)).sum::<u64>() / many.len() as u64)
                as u32,
        ),
    }
}

/// One word for a machine whose batteries may disagree. Charging or discharging anywhere is what
/// the machine is doing; "full" only when every battery agrees it is.
fn combine_statuses(statuses: &[String]) -> String {
    for wanted in ["charging", "discharging"] {
        if statuses.iter().any(|status| status == wanted) {
            return wanted.to_owned();
        }
    }
    match statuses {
        [] => "unknown".to_owned(),
        many => {
            let first = many[0].clone();
            if many.iter().all(|status| *status == first) {
                first
            } else {
                "unknown".to_owned()
            }
        }
    }
}

/// sysfs publishes these in seconds; a widget shows minutes.
fn minutes_from_sysfs(seconds: u64) -> u32 {
    (seconds / 60).min(u64::from(u32::MAX)) as u32
}

fn field(supply: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(supply.join(name))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn number(supply: &Path, name: &str) -> Option<u64> {
    field(supply, name)?.parse().ok()
}

/// `org.legeos.Battery1`.
#[derive(Debug)]
pub struct BatteryApi {
    batteries: SysfsBatteries,
}

impl BatteryApi {
    pub fn new(batteries: SysfsBatteries) -> Self {
        Self { batteries }
    }
}

#[zbus::interface(name = "org.legeos.Battery1")]
impl BatteryApi {
    /// The battery as `BatteryState` JSON.
    async fn get(&self) -> fdo::Result<String> {
        let batteries = self.batteries.clone();
        // sysfs reads are cheap but they are still blocking file I/O, and the session bus must
        // not be the thing that waits on a slow driver.
        let state = tokio::task::spawn_blocking(move || batteries.read())
            .await
            .map_err(|error| failed(anyhow::anyhow!("{error}")))?;
        serde_json::to_string(&state).map_err(|error| failed(anyhow::anyhow!("{error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sysfs-shaped supply directory. Every test reads one of these and never
    /// `/sys/class/power_supply`.
    fn supply(root: &Path, name: &str, fields: &[(&str, &str)]) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        for (key, value) in fields {
            std::fs::write(directory.join(key), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn a_laptop_on_battery_reports_its_charge_status_and_time_remaining() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "AC", &[("type", "Mains"), ("online", "0")]);
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("present", "1"),
                ("status", "Discharging"),
                ("capacity", "72"),
                ("energy_now", "36000000"),
                ("energy_full", "50000000"),
                ("time_to_empty_now", "8580"),
            ],
        );

        let state = SysfsBatteries::new(root.path()).read();
        assert!(state.present);
        assert_eq!(state.percent, Some(72));
        assert_eq!(state.status, "discharging");
        assert!(!state.on_mains);
        assert_eq!(state.time_to_empty_minutes, Some(143));
        assert_eq!(state.time_to_full_minutes, None);
    }

    #[test]
    fn a_desktop_says_it_has_no_battery_rather_than_zero_percent() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "AC", &[("type", "Mains"), ("online", "1")]);

        let state = SysfsBatteries::new(root.path()).read();
        assert!(!state.present);
        assert_eq!(state.percent, None);
        assert!(state.on_mains);
        assert_eq!(state.status, "unknown");
    }

    #[test]
    fn a_missing_sysfs_tree_is_answered_not_raised() {
        let state = SysfsBatteries::new("/definitely/not/a/sysfs/tree").read();
        assert_eq!(
            state,
            BatteryState {
                status: "unknown".to_owned(),
                ..BatteryState::default()
            }
        );
    }

    #[test]
    fn two_batteries_are_summed_by_charge_not_averaged_by_percentage() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        // 90% of a small cell and 10% of a large one: averaging the percentages says 50%, which
        // is wrong by a factor of two on the only quantity the user cares about.
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Charging"),
                ("capacity", "90"),
                ("energy_now", "18000000"),
                ("energy_full", "20000000"),
                ("time_to_full_now", "1800"),
            ],
        );
        supply(
            root.path(),
            "BAT1",
            &[
                ("type", "Battery"),
                ("status", "Charging"),
                ("capacity", "10"),
                ("energy_now", "8000000"),
                ("energy_full", "80000000"),
                ("time_to_full_now", "3600"),
            ],
        );

        let state = SysfsBatteries::new(root.path()).read();
        assert_eq!(state.percent, Some(26));
        assert_eq!(state.status, "charging");
        assert_eq!(state.time_to_full_minutes, Some(90));
    }

    #[test]
    fn an_empty_battery_bay_does_not_count_as_a_battery() {
        let root = tempfile::tempdir().unwrap();
        supply(
            root.path(),
            "BAT1",
            &[("type", "Battery"), ("present", "0"), ("status", "Unknown")],
        );
        assert!(!SysfsBatteries::new(root.path()).read().present);
    }

    #[test]
    fn a_kernel_with_no_time_estimate_leaves_the_field_empty() {
        let root = tempfile::tempdir().unwrap();
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging"),
                ("capacity", "55"),
                ("charge_now", "2200000"),
                ("charge_full", "4000000"),
            ],
        );
        let state = SysfsBatteries::new(root.path()).read();
        // charge_* rather than energy_*: the ratio is the same and this driver publishes Ah.
        assert_eq!(state.percent, Some(55));
        assert_eq!(state.time_to_empty_minutes, None);
    }

    #[tokio::test]
    async fn the_interface_answers_with_battery_state_json() {
        let root = tempfile::tempdir().unwrap();
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Full"),
                ("capacity", "100"),
                ("energy_now", "50000000"),
                ("energy_full", "50000000"),
            ],
        );
        let api = BatteryApi::new(SysfsBatteries::new(root.path()));
        let state: BatteryState = serde_json::from_str(&api.get().await.unwrap()).unwrap();
        assert_eq!(state.percent, Some(100));
        assert_eq!(state.status, "full");
    }
}
