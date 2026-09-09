//! Correlating a PID with the process that currently holds it, to defend against PID reuse
//! (planning doc 02 §3): "correlate the live Wayland peer credential with a PID start time or
//! pidfd so a stale PID cannot be confused with a later process after reuse". This crate uses
//! the start-time form — `/proc/<pid>/stat` field 22 — because it needs no `unsafe` FFI to a
//! pidfd syscall and is exact for the question asked here ("is this still the same process I
//! saw a moment ago?").
//!
//! Every function that touches the real filesystem takes the `/proc` root as a parameter so
//! tests can point it at a fixture tree, following the convention in
//! `services/templar-resourced/src/psi.rs`.

use std::path::Path;

/// Read `pid`'s start time (in clock ticks since boot) from `{proc_root}/<pid>/stat`. `None`
/// when the process is gone or the file is malformed — both mean "cannot vouch for this PID
/// right now", which callers treat as a cache miss rather than an error.
pub fn read_start_time(proc_root: &Path, pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    parse_start_time(&text)
}

/// Parse field 22 (`starttime`) out of the raw contents of `/proc/<pid>/stat`.
///
/// The format is `pid (comm) state ppid ...`; `comm` is parenthesized and may itself contain
/// spaces or parentheses, so the fields cannot be split naively. The kernel guarantees the
/// *last* `)` in the line closes `comm` (see `proc(5)`); everything after it is
/// whitespace-separated fields starting at field 3 (`state`), making field 22 the 20th token
/// after that closing paren.
fn parse_start_time(stat_line: &str) -> Option<u64> {
    let close_paren = stat_line.rfind(')')?;
    let rest = &stat_line[close_paren + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_starttime_from_a_realistic_stat_line() {
        // A comm field containing spaces and parentheses, as a hostile/unusual process name
        // would produce, to prove the "last `)`" rule is load-bearing.
        let line = "4242 (weird (name) here) S 1 4242 4242 0 -1 4194560 100 0 0 0 12 3 0 0 20 0 4 0 987654321 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_start_time(line), Some(987_654_321));
    }

    #[test]
    fn missing_or_malformed_stat_yields_none() {
        assert_eq!(parse_start_time(""), None);
        assert_eq!(parse_start_time("no closing paren here"), None);
        assert_eq!(parse_start_time("1 (short) S 1"), None);
    }

    #[test]
    fn reads_from_a_fixture_proc_root() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("4242");
        fs::create_dir_all(&pid_dir).unwrap();
        fs::write(
            pid_dir.join("stat"),
            "4242 (app) S 1 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
        )
        .unwrap();

        assert_eq!(read_start_time(dir.path(), 4242), Some(555));
        assert_eq!(read_start_time(dir.path(), 9999), None); // no such PID
    }
}
