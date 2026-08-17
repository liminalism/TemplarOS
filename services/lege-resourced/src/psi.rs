//! Pressure Stall Information parsing (`/proc/pressure/*` and per-cgroup `*.pressure`).

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PsiSample {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_microseconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
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

pub fn read_global_memory_psi() -> Result<Psi> {
    parse_psi(&std::fs::read_to_string("/proc/pressure/memory")?)
}

pub fn read_global_io_psi() -> Result<Psi> {
    parse_psi(&std::fs::read_to_string("/proc/pressure/io")?)
}

pub fn read_global_cpu_psi() -> Result<Psi> {
    parse_psi(&std::fs::read_to_string("/proc/pressure/cpu")?)
}

/// `MemAvailable` in bytes.
pub fn read_mem_available() -> Result<u64> {
    parse_mem_available(&std::fs::read_to_string("/proc/meminfo")?)
}

pub(crate) fn parse_mem_available(meminfo: &str) -> Result<u64> {
    let kib = meminfo
        .lines()
        .find_map(|line| {
            let remainder = line.strip_prefix("MemAvailable:")?;
            remainder.split_whitespace().next()?.parse::<u64>().ok()
        })
        .context("MemAvailable is absent from /proc/meminfo")?;
    Ok(kib.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
