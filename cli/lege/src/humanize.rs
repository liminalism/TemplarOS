//! Human-readable byte sizes and durations, shared by every subcommand that prints memory or
//! storage figures (`res`, `files`) so a size reads the same everywhere in `lege`'s output.

use std::fmt::Write as _;

/// `13.4 GiB`, `512 MiB`, `900 B` — binary (1024-based) units, one decimal place above `KiB`.
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// A signed byte rate, e.g. for memory growth: `+30.0 MiB/s`, `-1.0 MiB/s`, `0 B/s`.
pub fn bytes_per_second(value: i64) -> String {
    if value == 0 {
        return "0 B/s".to_owned();
    }
    let sign = if value < 0 { "-" } else { "+" };
    format!("{sign}{}/s", bytes(value.unsigned_abs()))
}

/// A duration in seconds as `1d 2h 3m 4s`, dropping leading zero components (`45s`, `3m 4s`) —
/// good enough for an uptime or an idle timer without pulling in a duration-formatting crate.
pub fn duration_secs(mut seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_owned();
    }
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3600;
    seconds %= 3600;
    let minutes = seconds / 60;
    seconds %= 60;

    let mut out = String::new();
    for (value, unit) in [(days, 'd'), (hours, 'h'), (minutes, 'm'), (seconds, 's')] {
        if value > 0 || (!out.is_empty() && unit == 's') {
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "{value}{unit}");
        }
    }
    out
}

/// Fraction (already `done`/`total`) as a percentage, `0%` when the total is zero rather than
/// dividing by it.
pub fn percent(done: u64, total: u64) -> String {
    if total == 0 {
        "0%".to_owned()
    } else {
        format!("{:.0}%", (done as f64 / total as f64) * 100.0)
    }
}

/// A Unix millisecond timestamp as a fixed local-independent `YYYY-MM-DD HH:MM:SS UTC` string —
/// no timezone-database dependency, just enough for a human to place an event.
pub fn timestamp_ms(at_ms: i64) -> String {
    let secs = at_ms.div_euclid(1000);
    let millis_of_day = secs.rem_euclid(86_400);
    let days_since_epoch = secs.div_euclid(86_400);

    let (year, month, day) = civil_from_days(days_since_epoch);
    let hour = millis_of_day / 3600;
    let minute = (millis_of_day % 3600) / 60;
    let second = millis_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a proleptic Gregorian
/// `(year, month, day)`. Dependency-free so this CLI does not need a chrono/time crate just to
/// print an event timestamp.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
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
    fn byte_sizes_pick_the_right_unit() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(900), "900 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(512 * 1024 * 1024), "512.0 MiB");
        assert_eq!(bytes(14_388_183_654), "13.4 GiB");
    }

    #[test]
    fn byte_rates_carry_a_sign() {
        assert_eq!(bytes_per_second(0), "0 B/s");
        assert_eq!(bytes_per_second(31_457_280), "+30.0 MiB/s");
        assert_eq!(bytes_per_second(-1_048_576), "-1.0 MiB/s");
    }

    #[test]
    fn durations_drop_leading_zero_components() {
        assert_eq!(duration_secs(0), "0s");
        assert_eq!(duration_secs(45), "45s");
        assert_eq!(duration_secs(184), "3m 4s");
        assert_eq!(duration_secs(3725), "1h 2m 5s");
        assert_eq!(duration_secs(90_061), "1d 1h 1m 1s");
    }

    #[test]
    fn percent_handles_zero_total() {
        assert_eq!(percent(0, 0), "0%");
        assert_eq!(percent(1, 2), "50%");
        assert_eq!(percent(3, 4), "75%");
    }

    #[test]
    fn timestamps_render_a_known_instant() {
        // 2026-08-21 14:32:00 UTC, the planning doc's worked example time-of-day.
        assert_eq!(timestamp_ms(1_787_322_720_000), "2026-08-21 14:32:00 UTC");
        assert_eq!(timestamp_ms(0), "1970-01-01 00:00:00 UTC");
    }
}
