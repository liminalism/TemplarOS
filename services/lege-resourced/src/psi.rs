//! Pressure Stall Information parsing (`/proc/pressure/*`, `/proc/meminfo`, and per-cgroup
//! `*.pressure` through [`crate::cgroup::Cgroup`]).
//!
//! Every function that touches the real filesystem takes the root (`/proc` in production) as a
//! parameter, so tests point it at a fixture tree instead (`tests/fixtures/resources/proc-*`) —
//! no root, no real `/proc/pressure`, no real pressure required to test the parser or the
//! threshold logic that consumes it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PsiSample {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_microseconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Psi {
    pub some: PsiSample,
    pub full: PsiSample,
}

/// Parse the two-line PSI format.
pub fn parse_psi(text: &str) -> Result<Psi> {
    let mut result = Psi::default();

    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next().context("PSI line has no kind")?;
        let mut sample = PsiSample::default();

        for field in fields {
            let (key, value) = field
                .split_once('=')
                .with_context(|| format!("invalid PSI field: {field:?}"))?;
            match key {
                "avg10" => sample.avg10 = value.parse()?,
                "avg60" => sample.avg60 = value.parse()?,
                "avg300" => sample.avg300 = value.parse()?,
                "total" => sample.total_microseconds = value.parse()?,
                _ => {}
            }
        }

        match kind {
            "some" => result.some = sample,
            "full" => result.full = sample,
            unknown => bail!("unknown PSI class {unknown:?}"),
        }
    }

    Ok(result)
}

/// `{proc_root}/pressure/memory`.
pub fn read_global_memory_psi(proc_root: &Path) -> Result<Psi> {
    parse_psi(&std::fs::read_to_string(proc_root.join("pressure/memory"))?)
}

/// `{proc_root}/pressure/io`.
pub fn read_global_io_psi(proc_root: &Path) -> Result<Psi> {
    parse_psi(&std::fs::read_to_string(proc_root.join("pressure/io"))?)
}

/// `{proc_root}/pressure/cpu`.
pub fn read_global_cpu_psi(proc_root: &Path) -> Result<Psi> {
    parse_psi(&std::fs::read_to_string(proc_root.join("pressure/cpu"))?)
}

/// `MemAvailable` in bytes, from `{proc_root}/meminfo`.
pub fn read_mem_available(proc_root: &Path) -> Result<u64> {
    parse_mem_available(&std::fs::read_to_string(proc_root.join("meminfo"))?)
}

pub(crate) fn parse_mem_available(meminfo: &str) -> Result<u64> {
    parse_meminfo_field(meminfo, "MemAvailable:")
}

/// `MemTotal` in bytes, from `{proc_root}/meminfo`.
///
/// Read once, at startup: how much memory the machine has does not change while it runs, and
/// re-reading it every tick would put a file read on the sampling loop for an answer that is
/// always the same.
///
/// # Errors
/// If `/proc/meminfo` cannot be read or has no `MemTotal` line.
pub fn read_mem_total(proc_root: &Path) -> Result<u64> {
    parse_mem_total(&std::fs::read_to_string(proc_root.join("meminfo"))?)
}

pub(crate) fn parse_mem_total(meminfo: &str) -> Result<u64> {
    parse_meminfo_field(meminfo, "MemTotal:")
}

/// One `Name: <kib> kB` line of `/proc/meminfo`, in bytes.
fn parse_meminfo_field(meminfo: &str, field: &str) -> Result<u64> {
    let kib = meminfo
        .lines()
        .find_map(|line| {
            let remainder = line.strip_prefix(field)?;
            remainder.split_whitespace().next()?.parse::<u64>().ok()
        })
        .with_context(|| format!("{field} is absent from /proc/meminfo"))?;
    Ok(kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_kernel_format() {
        let psi = parse_psi(
            "some avg10=0.12 avg60=0.34 avg300=0.56 total=123456\nfull avg10=0.01 avg60=0.02 avg300=0.03 total=7890\n",
        )
        .unwrap();
        assert!((psi.some.avg10 - 0.12).abs() < f64::EPSILON);
        assert_eq!(psi.some.total_microseconds, 123_456);
        assert!((psi.full.avg300 - 0.03).abs() < f64::EPSILON);
        assert_eq!(psi.full.total_microseconds, 7890);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_psi("weird avg10=1").is_err());
        assert!(parse_psi("some avg10").is_err());
    }

    #[test]
    fn mem_available() {
        let meminfo =
            "MemTotal:       32000000 kB\nMemFree:         1000 kB\nMemAvailable:    2048 kB\n";
        assert_eq!(parse_mem_available(meminfo).unwrap(), 2048 * 1024);
        assert!(parse_mem_available("MemTotal: 1 kB\n").is_err());
    }

    #[test]
    fn reads_from_an_explicit_proc_root_fixture() {
        let root = std::env::temp_dir().join(format!("lege-resourced-psi-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("pressure")).unwrap();
        fs::write(
            root.join("pressure/memory"),
            "some avg10=1.00 avg60=1.00 avg300=1.00 total=1\nfull avg10=0.50 avg60=0.50 avg300=0.50 total=1\n",
        )
        .unwrap();
        fs::write(
            root.join("pressure/io"),
            "some avg10=9.00 avg60=9.00 avg300=9.00 total=1\nfull avg10=8.00 avg60=8.00 avg300=8.00 total=1\n",
        )
        .unwrap();
        fs::write(root.join("meminfo"), "MemAvailable:    4096 kB\n").unwrap();

        let memory = read_global_memory_psi(&root).unwrap();
        assert!((memory.full.avg10 - 0.50).abs() < f64::EPSILON);
        let io = read_global_io_psi(&root).unwrap();
        assert!((io.full.avg10 - 8.00).abs() < f64::EPSILON);
        assert_eq!(read_mem_available(&root).unwrap(), 4096 * 1024);

        assert!(read_global_cpu_psi(&root).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
