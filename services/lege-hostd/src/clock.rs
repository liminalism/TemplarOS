//! Time, abstracted (rule 4: "a clock" is one of the traits every system-touching path sits
//! behind). Transaction and deployment ids are timestamp-derived so they sort and read
//! chronologically (`distro/deploy/README.md`'s `@deployments/2026-08-17T10-00` example) without
//! pulling in a date/time crate — civil-calendar conversion from `SystemTime` is ~20 lines
//! (Howard Hinnant's `days_from_civil`, UTC only, which is all a system daemon needs).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> SystemTime;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A fixed instant, for deterministic tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub SystemTime);

#[cfg(test)]
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// `YYYYMMDDTHHMMSSZ`, sortable and stable across timezones (always UTC).
pub fn stamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86400) as i64);
    let time_of_day = secs % 86400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days-since-epoch to a proleptic-Gregorian (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_stamps_as_expected() {
        assert_eq!(stamp(UNIX_EPOCH), "19700101T000000Z");
    }

    #[test]
    fn known_dates_format_correctly() {
        // 2026-08-17T10:00:00Z, the timestamp distro/deploy/README.md uses as an example.
        assert_eq!(
            stamp(UNIX_EPOCH + Duration::from_secs(1_786_960_800)),
            "20260817T100000Z"
        );
        // A leap-day well after a century boundary, to exercise civil_from_days' era math.
        assert_eq!(
            stamp(UNIX_EPOCH + Duration::from_secs(1_709_210_096)),
            "20240229T123456Z"
        );
    }

    #[test]
    fn fixed_clock_is_deterministic() {
        let time = UNIX_EPOCH + Duration::from_secs(1_786_960_800);
        let clock = FixedClock(time);
        assert_eq!(clock.now(), time);
        assert_eq!(clock.now(), clock.now());
    }
}
