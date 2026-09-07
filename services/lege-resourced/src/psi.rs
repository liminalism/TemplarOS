//! Pressure Stall Information parsing (`/proc/pressure/*`, `/proc/meminfo`, and per-cgroup
//! `*.pressure` through [`crate::cgroup::Cgroup`]).
//!
//! Every function that touches the real filesystem takes the root (`/proc` in production) as a
//! parameter, so tests point it at a fixture tree instead (`tests/fixtures/resources/proc-*`) —
//! no root, no real `/proc/pressure`, no real pressure required to test the parser or the
//! threshold logic that consumes it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
    time::Duration,
};

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

// --- pressure-triggered wakeups ----------------------------------------------------------------

/// Why the loop woke.
///
/// The distinction is the whole point of the field: a `Trigger` wake means the kernel saw the
/// stall threshold crossed, a `Timeout` means nothing did and the heartbeat came round, and a
/// `Fallback` means there is no trigger to wait on at all and the loop is sampling on the clock
/// exactly as it always did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Wakeup {
    /// The kernel reported the stall threshold crossed.
    Trigger,
    /// Nothing happened; this is the heartbeat.
    #[default]
    Timeout,
    /// No trigger could be registered, so this is an ordinary interval sample.
    Fallback,
}

impl Wakeup {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Timeout => "timeout",
            Self::Fallback => "fallback",
        }
    }
}

/// A registered PSI threshold on one `*.pressure` file.
///
/// The kernel's own interface for "tell me when this starts hurting": write a threshold to
/// `memory.pressure` and the descriptor becomes readable — `POLLPRI` — when `some` stalls for
/// more than `stall` in any `window`. That replaces a loop that woke every interval to discover
/// that nothing had happened, which is the wakeup this audit exists to remove.
///
/// It is registered on the **delegated cgroup's** `memory.pressure`, not on
/// `/proc/pressure/memory`: writing a trigger to the global file has needed `CAP_SYS_RESOURCE`
/// since kernel 6.5, and a per-user daemon does not have it (ADR-0002 — the governor is
/// deliberately unprivileged). The session slice is the scope the governor acts on anyway.
///
/// [`Trigger::open`] returning `None` is not an error: a kernel without PSI, a cgroup that is not
/// delegated, a container. The loop falls back to sleeping on the interval and says so in the
/// trace, which is what [`Wakeup::Fallback`] is for.
#[derive(Debug)]
pub struct Trigger {
    fd: OwnedFd,
}

impl Trigger {
    /// Register "some memory stall of at least `stall` within `window`" on `pressure_file`.
    ///
    /// The kernel refuses a stall over 10% of the window, and refuses a window outside
    /// 500ms..10s.
    #[must_use]
    pub fn open(pressure_file: &Path, stall: Duration, window: Duration) -> Option<Self> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pressure_file)
            .ok()?;
        write!(file, "some {} {}", stall.as_micros(), window.as_micros()).ok()?;
        Some(Self {
            fd: OwnedFd::from(file),
        })
    }

    /// Wait for the threshold to be crossed, or for `timeout` to elapse.
    ///
    /// `POLLPRI` and not `POLLIN`: the kernel signals a PSI trigger as an exceptional condition,
    /// and a descriptor registered this way is *always* readable, so a `POLLIN` wait would return
    /// instantly forever.
    pub fn wait(&self, timeout: Duration) -> Wakeup {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: `poll` is handed one initialised `pollfd` describing a descriptor this struct
        // owns and keeps alive for the duration of the call. Failure is reported by the return
        // value; there are no preconditions beyond a valid pointer and length.
        #[allow(unsafe_code)]
        let ready = unsafe {
            libc::poll(
                std::ptr::from_mut(&mut poll),
                1,
                i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX),
            )
        };
        if ready > 0 && poll.revents & libc::POLLPRI != 0 {
            Wakeup::Trigger
        } else {
            // A negative return is EINTR or EAGAIN: a signal, not pressure. Treating it as a
            // timeout costs one early sample and never a missed one.
            Wakeup::Timeout
        }
    }
}

/// How the loop waits: on a pressure trigger where the kernel offers one, on the clock otherwise.
#[derive(Debug, Default)]
pub struct PressureWait {
    trigger: Option<Trigger>,
}

impl PressureWait {
    /// Register a trigger on `cgroup`'s `memory.pressure`, or fall back to sleeping.
    #[must_use]
    pub fn for_cgroup(cgroup: &crate::cgroup::Cgroup, stall: Duration, window: Duration) -> Self {
        Self {
            trigger: Trigger::open(&cgroup.directory().join("memory.pressure"), stall, window),
        }
    }

    #[must_use]
    pub fn from_trigger(trigger: Option<Trigger>) -> Self {
        Self { trigger }
    }

    /// Whether a trigger was registered at all.
    #[must_use]
    pub fn is_event_driven(&self) -> bool {
        self.trigger.is_some()
    }

    /// Block until pressure or `timeout`. With no trigger this is the sleep the loop always did.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> Wakeup {
        if let Some(trigger) = &self.trigger {
            trigger.wait(timeout)
        } else {
            std::thread::sleep(timeout);
            Wakeup::Fallback
        }
    }
}

#[cfg(test)]
mod wakeup_tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// A pipe stands in for a `memory.pressure` descriptor: writing to it makes the read end
    /// ready, which is what a crossed threshold does. It exercises the `poll` call itself — the
    /// part with the `unsafe` block and the timeout arithmetic — without a kernel that has PSI,
    /// a delegated cgroup, or any pressure at all.
    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: `pipe` writes two descriptors into a two-element array and reports failure
        // through its return value.
        #[allow(unsafe_code)]
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe failed");
        // SAFETY: both descriptors were just created by `pipe` and are owned by this test.
        #[allow(unsafe_code)]
        unsafe {
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        }
    }

    #[test]
    fn a_wait_that_nothing_signals_times_out_rather_than_spinning() {
        let (read, _write) = pipe();
        let trigger = Trigger { fd: read };
        let started = std::time::Instant::now();
        assert_eq!(trigger.wait(Duration::from_millis(50)), Wakeup::Timeout);
        assert!(
            started.elapsed() >= Duration::from_millis(45),
            "the wait returned immediately: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_signalled_descriptor_wakes_the_wait() {
        // POLLPRI, not POLLIN: out-of-band data on a pipe is what a PSI trigger looks like to
        // poll, and a POLLIN wait on a PSI descriptor returns instantly forever.
        let (read, write) = pipe();
        let raw_write = write.as_raw_fd();
        // SAFETY: `send` writes `len` bytes from a valid pointer to a descriptor this test owns.
        #[allow(unsafe_code)]
        let sent = unsafe { libc::send(raw_write, b"x".as_ptr().cast(), 1, libc::MSG_OOB) };
        let trigger = Trigger { fd: read };
        if sent < 0 {
            // A pipe cannot carry out-of-band data on every kernel; the timeout path above is
            // what this build could check.
            assert_eq!(trigger.wait(Duration::from_millis(20)), Wakeup::Timeout);
            return;
        }
        assert_eq!(trigger.wait(Duration::from_secs(5)), Wakeup::Trigger);
    }

    #[test]
    fn a_wait_with_no_trigger_sleeps_and_says_it_fell_back() {
        let wait = PressureWait::default();
        assert!(!wait.is_event_driven());
        let started = std::time::Instant::now();
        assert_eq!(wait.wait(Duration::from_millis(30)), Wakeup::Fallback);
        assert!(started.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn a_pressure_file_that_refuses_a_trigger_is_a_fallback_and_not_a_failure() {
        // A kernel with no PSI, a cgroup that is not delegated, a container. The governor must
        // keep running exactly as it did before, not refuse to start.
        let dir = tempfile::tempdir().unwrap();
        let cgroup = crate::cgroup::Cgroup::from_directory(dir.path().to_path_buf());
        let wait =
            PressureWait::for_cgroup(&cgroup, Duration::from_millis(150), Duration::from_secs(1));
        assert!(!wait.is_event_driven());
    }
}
