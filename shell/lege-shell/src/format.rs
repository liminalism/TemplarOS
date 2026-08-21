//! Human-readable formatting shared by every drawer renderer, so a size reads the same in the
//! Drives drawer as it does in the Operations drawer.
//!
//! Deliberately dependency-free (no chrono/time crate) and deliberately duplicated rather than
//! shared: `cli/lege/src/humanize.rs` already has the same `bytes`/`timestamp_ms` pair, but that
//! crate belongs to a different ownership track, and four lines of binary-unit arithmetic is not
//! worth a new shared crate to avoid repeating.

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

/// A `done`/`total` pair as a whole-number percentage, `0` when `total` is zero rather than
/// dividing by it.
pub fn percent(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    let ratio = (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
    ratio as u8
}

/// A Unix millisecond timestamp as a fixed, timezone-database-free `YYYY-MM-DD HH:MM UTC`
/// string — enough for a human to place an event in the Recent drawer without pulling in a
/// timezone database this restartable, decorative process has no business owning.
pub fn timestamp_ms(at_ms: i64) -> String {
    let secs = at_ms.div_euclid(1000);
    let minute_of_day = secs.rem_euclid(86_400) / 60;
    let days_since_epoch = secs.div_euclid(86_400);

    let (year, month, day) = civil_from_days(days_since_epoch);
    let hour = minute_of_day / 60;
    let minute = minute_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a proleptic Gregorian
/// `(year, month, day)`.
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
        assert_eq!(bytes(14_388_183_654), "13.4 GiB");
    }

    #[test]
    fn percent_is_bounded_and_handles_zero_total() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(5, 10), 50);
        assert_eq!(percent(10, 10), 100);
        assert_eq!(percent(11, 10), 100);
    }

    #[test]
    fn timestamps_render_a_known_instant() {
        // 2026-08-21 14:32:00 UTC, the planning doc's worked example time-of-day.
        assert_eq!(timestamp_ms(1_787_322_720_000), "2026-08-21 14:32 UTC");
        assert_eq!(timestamp_ms(0), "1970-01-01 00:00 UTC");
    }
}
