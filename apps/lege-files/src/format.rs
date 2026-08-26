use std::time::{Duration, UNIX_EPOCH};

pub fn bytes(value: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit + 1 < UNITS.len() {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} {}", UNITS[unit])
    } else if scaled >= 10.0 {
        format!("{scaled:.0} {}", UNITS[unit])
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}

pub fn timestamp_ms(value: i64) -> String {
    if value <= 0 {
        return "Unknown".to_owned();
    }
    let seconds = u64::try_from(value / 1_000).unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(seconds);
    match age {
        0..=59 => "Just now".to_owned(),
        60..=3_599 => format!("{} min ago", age / 60),
        3_600..=86_399 => format!("{} hr ago", age / 3_600),
        86_400..=604_799 => format!("{} days ago", age / 86_400),
        _ => format!("{} days ago", Duration::from_secs(age).as_secs() / 86_400),
    }
}

pub fn mode(mode: u32) -> String {
    let mut out = String::with_capacity(9);
    for (bit, glyph) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        out.push(if mode & bit == bit { glyph } else { '-' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_measurements_are_compact_and_permissions_are_legible() {
        assert_eq!(bytes(1_024), "1.0 KiB");
        assert_eq!(bytes(12 * 1_024), "12 KiB");
        assert_eq!(mode(0o754), "rwxr-xr--");
    }
}
