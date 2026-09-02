//! Wire models for the three hardware-status interfaces: `Audio1`, `Backlight1`, `Battery1`.
//!
//! The daemon remains an independent process and clients do not link its implementation crate, so
//! — exactly as [`crate::files`] does for `org.legeos.Files` — these serde models are the
//! versioned JSON contract carried by the typed proxies. The CLI and the shell parse the same
//! structs, which is what stops `lege status battery` and the battery widget from disagreeing
//! about what "72%" means.
//!
//! Every one of the three carries `present`, and it is the field that matters most: a desktop has
//! no battery and no backlight, and a machine with no WirePlumber has no volume. None of those is
//! a failure, and a surface that cannot tell them from "0%" will show a working machine as a
//! broken one.

use serde::{Deserialize, Serialize};

/// `Audio1.Get`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioState {
    pub present: bool,
    #[serde(default)]
    pub volume_percent: u32,
    #[serde(default)]
    pub muted: bool,
}

/// `Backlight1.Get`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BacklightState {
    pub present: bool,
    #[serde(default)]
    pub percent: u32,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub raw: u64,
    #[serde(default)]
    pub max_raw: u64,
}

/// `Battery1.Get`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatteryState {
    pub present: bool,
    #[serde(default)]
    pub percent: Option<u32>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub on_mains: bool,
    #[serde(default)]
    pub time_to_empty_minutes: Option<u32>,
    #[serde(default)]
    pub time_to_full_minutes: Option<u32>,
}

impl AudioState {
    /// The one line a widget or `lege status` shows at a glance.
    ///
    /// Presentation lives here and not in the service: the daemon's job is to know what the
    /// volume *is*, and two clients that both render it must not each invent their own wording.
    pub fn headline(&self) -> String {
        if !self.present {
            return "No audio".to_owned();
        }
        let level = format!("{}%{}", self.volume_percent, self.boost_note());
        if self.muted {
            // The level is still worth saying: unmuting returns to it, and "Muted" alone leaves a
            // person unable to predict how loud that will be.
            return format!("Muted · {level}");
        }
        level
    }

    /// Whether something has pushed the sink past its own full scale.
    ///
    /// WirePlumber reports a linear fraction that can exceed 1.0, and the service reports that
    /// truthfully rather than clamping a reading. A person seeing "126%" with no explanation is
    /// left to wonder whether the number or the machine is broken.
    pub fn boosted(&self) -> bool {
        self.present && self.volume_percent > 100
    }

    fn boost_note(&self) -> &'static str {
        if self.boosted() { " (boosted)" } else { "" }
    }
}

impl BacklightState {
    pub fn headline(&self) -> String {
        if self.present {
            format!("{}%", self.percent)
        } else {
            "No backlight".to_owned()
        }
    }
}

impl BatteryState {
    /// The time estimate that applies to what the battery is currently doing, with the word for
    /// it — `None` when the kernel published no estimate, which is common and not an error.
    pub fn time_remaining(&self) -> Option<(&'static str, u32)> {
        match self.status.as_str() {
            "charging" => self
                .time_to_full_minutes
                .map(|minutes| ("to full", minutes)),
            "discharging" => self
                .time_to_empty_minutes
                .map(|minutes| ("remaining", minutes)),
            // "full" and "not charging" have no meaningful estimate: nothing is happening.
            _ => None,
        }
    }

    /// The one line a widget shows at its smallest rung.
    pub fn headline(&self) -> String {
        match (self.present, self.percent) {
            (true, Some(percent)) => format!("{percent}%"),
            (true, None) => "Battery".to_owned(),
            // Neither an error nor a zero: a desktop has no battery, and "0%" would describe a
            // machine that is working perfectly as one about to die.
            (false, _) => {
                if self.on_mains {
                    "On mains".to_owned()
                } else {
                    "No battery".to_owned()
                }
            }
        }
    }
}

impl BatteryState {
    /// The one line a surface with room for a sentence shows: the charge, and what it is doing.
    ///
    /// Here rather than in each caller for the same reason [`BatteryState::headline`] is: the
    /// Home Field's Settings widget and Settings' own System page both render this, and two
    /// clients that render one fact must not each invent their own wording for it.
    ///
    /// The estimate's word carries the direction, so nothing says it twice: "2 h 23 min
    /// remaining" is already a battery going down, and adding "discharging" in front of it is the
    /// same fact said again in a line that then has to be truncated to fit a tile. The status word
    /// appears only when there is no estimate to imply it, which is when it is the whole answer.
    pub fn reading(&self) -> String {
        let mut line = self.headline();
        if !self.present {
            return line;
        }
        match self.time_remaining() {
            Some((word, minutes)) => {
                line.push_str(" · ");
                line.push_str(&duration_minutes(minutes));
                line.push(' ');
                line.push_str(word);
            }
            None if !self.status.is_empty() && self.status != "unknown" => {
                line.push(' ');
                line.push_str(&self.status);
            }
            None => {}
        }
        line
    }
}

/// `143` as `2 h 23 min` — the shape a person reads at a glance.
pub fn duration_minutes(minutes: u32) -> String {
    let (hours, rest) = (minutes / 60, minutes % 60);
    match (hours, rest) {
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_boosted_past_full_scale_says_so_rather_than_showing_a_bare_number() {
        let boosted = AudioState {
            present: true,
            volume_percent: 126,
            muted: false,
        };
        assert!(boosted.boosted());
        assert_eq!(boosted.headline(), "126% (boosted)");
    }

    #[test]
    fn a_boosted_sink_that_is_muted_says_both() {
        let state = AudioState {
            present: true,
            volume_percent: 126,
            muted: true,
        };
        assert_eq!(state.headline(), "Muted · 126% (boosted)");
    }

    #[test]
    fn full_scale_is_not_boosted() {
        let state = AudioState {
            present: true,
            volume_percent: 100,
            muted: false,
        };
        assert!(!state.boosted());
        assert_eq!(state.headline(), "100%");
    }

    /// The direction is said once. "discharging · 2 h 23 min remaining" is one fact twice, and on
    /// a tile it is one fact twice *and* truncated.
    #[test]
    fn a_battery_reading_never_states_its_direction_twice() {
        let discharging = BatteryState {
            present: true,
            percent: Some(72),
            status: "discharging".to_owned(),
            on_mains: false,
            time_to_empty_minutes: Some(143),
            time_to_full_minutes: None,
        };
        assert_eq!(discharging.reading(), "72% · 2 h 23 min remaining");

        // With no estimate the status word is the whole answer, so it does appear.
        let no_estimate = BatteryState {
            time_to_empty_minutes: None,
            ..discharging.clone()
        };
        assert_eq!(no_estimate.reading(), "72% discharging");

        // A full battery on mains gets no invented estimate and no doubled word.
        let full = BatteryState {
            present: true,
            percent: Some(100),
            status: "full".to_owned(),
            on_mains: true,
            time_to_empty_minutes: None,
            time_to_full_minutes: None,
        };
        assert_eq!(full.reading(), "100% full");

        // A machine with no battery says so and stops; nothing is appended to that.
        let none = BatteryState {
            present: false,
            on_mains: true,
            ..BatteryState::default()
        };
        assert_eq!(none.reading(), "On mains");
    }

    #[test]
    fn the_wire_shapes_match_what_the_services_publish() {
        // Exactly what `Battery1.Get` writes for a laptop on battery.
        let battery: BatteryState = serde_json::from_str(
            r#"{"present":true,"percent":72,"status":"discharging","on_mains":false,
                "time_to_empty_minutes":143,"time_to_full_minutes":null}"#,
        )
        .unwrap();
        assert_eq!(battery.percent, Some(72));
        assert_eq!(battery.time_remaining(), Some(("remaining", 143)));

        let audio: AudioState =
            serde_json::from_str(r#"{"present":true,"volume_percent":45,"muted":true}"#).unwrap();
        assert!(audio.muted);
        assert_eq!(audio.volume_percent, 45);

        let backlight: BacklightState = serde_json::from_str(
            r#"{"present":true,"percent":40,"device":"intel_backlight","raw":7680,
                "max_raw":19200}"#,
        )
        .unwrap();
        assert_eq!(backlight.percent, 40);
        assert_eq!(backlight.device, "intel_backlight");
    }

    #[test]
    fn a_machine_without_the_hardware_parses_as_absent_not_as_zero() {
        let battery: BatteryState = serde_json::from_str(
            r#"{"present":false,"percent":null,"status":"unknown","on_mains":true,
                "time_to_empty_minutes":null,"time_to_full_minutes":null}"#,
        )
        .unwrap();
        assert!(!battery.present);
        assert_eq!(battery.percent, None, "a desktop is not a flat battery");
        assert_eq!(battery.time_remaining(), None);
    }

    #[test]
    fn an_idle_battery_is_given_no_estimate_even_when_one_is_published() {
        // "full" means nothing is happening, so neither estimate applies however stale a figure
        // the kernel left in the file.
        let battery = BatteryState {
            present: true,
            percent: Some(100),
            status: "full".to_owned(),
            on_mains: true,
            time_to_empty_minutes: Some(600),
            time_to_full_minutes: Some(0),
        };
        assert_eq!(battery.time_remaining(), None);
    }

    #[test]
    fn a_headline_never_shows_absent_hardware_as_a_flat_or_silent_one() {
        assert_eq!(
            BatteryState {
                present: false,
                on_mains: true,
                ..BatteryState::default()
            }
            .headline(),
            "On mains"
        );
        assert_eq!(BatteryState::default().headline(), "No battery");
        assert_eq!(AudioState::default().headline(), "No audio");
        assert_eq!(BacklightState::default().headline(), "No backlight");
    }

    #[test]
    fn a_present_headline_is_the_level_and_says_when_it_is_muted() {
        assert_eq!(
            AudioState {
                present: true,
                volume_percent: 45,
                muted: false
            }
            .headline(),
            "45%"
        );
        assert_eq!(
            AudioState {
                present: true,
                volume_percent: 45,
                muted: true
            }
            .headline(),
            "Muted · 45%",
            "the level survives a mute, because unmuting returns to it"
        );
        assert_eq!(
            BatteryState {
                present: true,
                percent: Some(72),
                ..BatteryState::default()
            }
            .headline(),
            "72%"
        );
    }

    #[test]
    fn durations_read_the_way_a_person_says_them() {
        assert_eq!(duration_minutes(0), "0 min");
        assert_eq!(duration_minutes(45), "45 min");
        assert_eq!(duration_minutes(120), "2 h");
        assert_eq!(duration_minutes(143), "2 h 23 min");
    }
}
