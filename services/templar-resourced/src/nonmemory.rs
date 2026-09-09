//! What the memory governor does not own (planning doc 01 §5): storage stalls, GPU reset or
//! driver failure, kernel-level lockup, hardware failure. Each needs a different recovery path
//! and a different message than "an application used too much memory" — so this module's job is
//! narrow: emit [`Stall`] when there is real, specific evidence, and stay silent otherwise.
//!
//! What is (and is not) implemented, honestly:
//!
//! * **Storage stalls**: solid. I/O PSI elevated while memory PSI is not is a clean, unprivileged
//!   signal (`/proc/pressure/io` vs `/proc/pressure/memory`), fully testable from parsed [`Psi`]
//!   values.
//! * **GPU reset**: best-effort. The kernel exposes reset/recovery counters under
//!   driver-specific sysfs paths that vary and are not part of a stable cross-vendor ABI. Rather
//!   than hard-coding one driver's path and silently missing every other, [`GpuResetWatcher`]
//!   scans `/sys/class/drm/**` for any file whose name looks like a reset counter and treats an
//!   increase as one reset event. On a system/driver that exposes nothing of the kind, this
//!   detector is simply always silent — that is the honest outcome, not a bug.
//! * **Kernel-level lockup (hung-task, RCU stalls)**: best-effort and often silent by
//!   construction. The only unprivileged-ish source is `/dev/kmsg`, gated by
//!   `kernel.dmesg_restrict` (default 1) unless the process holds `CAP_SYSLOG`. `templar-resourced`
//!   runs as an ordinary user unit with no extra capabilities (ADR-0002), so on a stock system
//!   [`KmsgWatcher::open`] degrades to a permanently-silent no-op. That degradation is treated as
//!   "no signal available", not an error — we do not pretend to detect what we cannot.
//! * **Hardware failure**: not attempted. There is no general unprivileged userspace signal for
//!   this (EDAC/RAS reporting needs root); claiming to detect it would be exactly the
//!   overclaiming this module's doc comment warns against.

use crate::psi::Psi;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// A stall the memory governor does not own — mirrors `org.templaros.Resources1.NonMemoryStall`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Stall {
    Storage { io_full_psi_avg10: f64 },
    GpuReset { device: String, delta: u64 },
    KernelLockup { evidence: String },
}

impl Stall {
    /// The `kind` string for the D-Bus signal.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Storage { .. } => "storage",
            Self::GpuReset { .. } => "gpu-reset",
            Self::KernelLockup { .. } => "kernel-lockup",
        }
    }
}

/// I/O PSI is elevated while memory PSI is not: the desktop is stalling on storage, not on
/// reclaim. `threshold_avg10` is the same "elevated" bar used for memory (see
/// `GovernorConfig::elevated_some_avg10`) applied to `io.full`; memory is considered "not the
/// cause" when its `full.avg10` is under half that bar.
pub fn detect_storage_stall(io_psi: Psi, memory_psi: Psi, threshold_avg10: f64) -> Option<Stall> {
    if io_psi.full.avg10 >= threshold_avg10 && memory_psi.full.avg10 < threshold_avg10 / 2.0 {
        Some(Stall::Storage {
            io_full_psi_avg10: io_psi.full.avg10,
        })
    } else {
        None
    }
}

/// Best-effort GPU reset-counter watcher. See the module doc comment for what this can and
/// cannot promise.
#[derive(Debug)]
pub struct GpuResetWatcher {
    root: PathBuf,
    last_counts: HashMap<PathBuf, u64>,
}

impl GpuResetWatcher {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            last_counts: HashMap::new(),
        }
    }

    /// Scan for reset-counter files and report any that increased since the last poll. The first
    /// poll after construction only establishes a baseline (never reports an event) so a
    /// pre-existing nonzero counter at startup is not misread as a fresh reset.
    pub fn poll(&mut self) -> Vec<Stall> {
        let mut events = Vec::new();
        for path in find_reset_counters(&self.root) {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(count) = text.trim().parse::<u64>() else {
                continue;
            };
            if let Some(&previous) = self.last_counts.get(&path)
                && count > previous
            {
                events.push(Stall::GpuReset {
                    device: path.display().to_string(),
                    delta: count - previous,
                });
            }
            self.last_counts.insert(path, count);
        }
        events
    }
}

/// Files under `root` (bounded to a few levels, matching `/sys/class/drm/card*/**`) whose name
/// contains both "reset" and "count" — the shape shared by every reset-counter file name we know
/// of across drivers, without hard-coding one specific path.
fn find_reset_counters(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 4, &mut found);
    found
}

fn walk(dir: &Path, depth_remaining: u32, found: &mut Vec<PathBuf>) {
    if depth_remaining == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(&path, depth_remaining - 1, found);
        } else if file_type.is_file() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if name.contains("reset") && name.contains("count") {
                found.push(path);
            }
        }
    }
}

/// Best-effort `/dev/kmsg` tail, for hung-task/RCU-stall evidence. See the module doc comment:
/// on a stock desktop this degrades to a permanently-silent no-op, which is the honest outcome.
#[derive(Debug)]
pub struct KmsgWatcher {
    file: Option<File>,
}

impl KmsgWatcher {
    /// Open `path` best-effort, non-blocking. Any failure (most commonly `EPERM` from
    /// `kernel.dmesg_restrict`) leaves this watcher permanently silent rather than retrying —
    /// the condition does not change while the process is running.
    pub fn open(path: &Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|error| {
                tracing::debug!(path = %path.display(), %error, "kernel-lockup evidence unavailable (no signal, not an error)");
            })
            .ok();
        Self { file }
    }

    /// Drain whatever is currently available and return records that look like hung-task or
    /// RCU-stall evidence. Returns an empty vec whenever nothing is available, including on every
    /// call for a watcher that failed to open.
    ///
    /// Each `read(2)` on `/dev/kmsg` returns exactly one complete record with no trailing
    /// newline (a multi-line message embeds its own continuation newlines) — so this drains by
    /// looping `read` to exhaustion, treating each successful read as one record, rather than
    /// buffering bytes and splitting on `\n` (which would either fuse adjacent records together
    /// or split a single multi-line one).
    pub fn poll(&mut self) -> Vec<Stall> {
        let Some(file) = self.file.as_mut() else {
            return Vec::new();
        };

        let mut buffer = [0_u8; 4096];
        let mut events = Vec::new();
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let record = String::from_utf8_lossy(&buffer[..count]);
                    if let Some(message) = kmsg_message(record.trim_end_matches('\n'))
                        && looks_like_kernel_lockup(message)
                    {
                        events.push(Stall::KernelLockup {
                            evidence: message.to_owned(),
                        });
                    }
                }
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        || error.kind() == ErrorKind::Interrupted =>
                {
                    break;
                }
                Err(_) => {
                    // /dev/kmsg surfaces a genuine record loss as EPIPE; either way, stop trying
                    // to interpret this call and pick back up on the next poll.
                    break;
                }
            }
        }
        events
    }
}

/// `/dev/kmsg` record format: `level,seq,timestamp,flags[,extra];message`.
fn kmsg_message(record: &str) -> Option<&str> {
    record.split_once(';').map(|(_, message)| message)
}

fn looks_like_kernel_lockup(message: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "hung_task",
        "rcu_sched detected stalls",
        "rcu_preempt detected stalls",
        "soft lockup",
        "NMI watchdog: BUG: soft lockup",
    ];
    MARKERS.iter().any(|marker| message.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psi::PsiSample;
    use std::io::Write;

    fn psi_with_full_avg10(avg10: f64) -> Psi {
        Psi {
            full: PsiSample {
                avg10,
                ..PsiSample::default()
            },
            ..Psi::default()
        }
    }

    #[test]
    fn storage_stall_needs_io_pressure_without_memory_pressure() {
        let io = psi_with_full_avg10(10.0);
        let memory_quiet = psi_with_full_avg10(0.1);
        let memory_busy = psi_with_full_avg10(5.0);

        assert!(detect_storage_stall(io, memory_quiet, 3.0).is_some());
        assert!(detect_storage_stall(io, memory_busy, 3.0).is_none());
        assert!(detect_storage_stall(psi_with_full_avg10(0.5), memory_quiet, 3.0).is_none());
    }

    #[test]
    fn gpu_reset_watcher_ignores_the_baseline_and_reports_increases() {
        let dir =
            std::env::temp_dir().join(format!("templar-resourced-drm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let device_dir = dir.join("card0/device");
        fs::create_dir_all(&device_dir).unwrap();
        let counter = device_dir.join("reset_count");
        fs::write(&counter, "3\n").unwrap();

        let mut watcher = GpuResetWatcher::new(dir.clone());
        assert!(watcher.poll().is_empty(), "first poll only baselines");

        fs::write(&counter, "3\n").unwrap();
        assert!(watcher.poll().is_empty(), "unchanged count is not a reset");

        fs::write(&counter, "5\n").unwrap();
        let events = watcher.poll();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Stall::GpuReset { delta, .. } if *delta == 2));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpu_reset_watcher_is_silent_when_nothing_matches() {
        let dir = std::env::temp_dir().join(format!(
            "templar-resourced-drm-empty-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("card0/device")).unwrap();
        fs::write(dir.join("card0/device/vendor"), "0x8086\n").unwrap();

        let mut watcher = GpuResetWatcher::new(dir.clone());
        assert!(watcher.poll().is_empty());
        assert!(watcher.poll().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kmsg_watcher_degrades_silently_when_the_path_is_unreadable() {
        let mut watcher = KmsgWatcher::open(Path::new("/does/not/exist"));
        assert!(watcher.poll().is_empty());
    }

    #[test]
    fn kmsg_watcher_reads_records_appended_between_polls() {
        // A plain file cannot replicate `/dev/kmsg`'s one-record-per-read(2) framing exactly,
        // but appending and polling in separate steps (rather than writing everything up front)
        // does exercise the same "drain what's newly available since last time" behaviour a
        // tail-like reader depends on, since a `File`'s read offset persists across calls.
        let path =
            std::env::temp_dir().join(format!("templar-resourced-kmsg-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(&path, b"").unwrap();

        let mut watcher = KmsgWatcher::open(&path);
        assert!(watcher.poll().is_empty());

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"6,100,0,-;ordinary boot message").unwrap();
        drop(file);
        assert!(watcher.poll().is_empty());

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"3,101,0,-;hung_task: blocked for more than 120 seconds.")
            .unwrap();
        drop(file);
        let events = watcher.poll();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Stall::KernelLockup { evidence } if evidence.contains("hung_task"))
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn kmsg_message_parsing_and_lockup_markers() {
        let record = "3,1234,98765,-;hung_task: blocked for more than 120 seconds.";
        assert_eq!(
            kmsg_message(record),
            Some("hung_task: blocked for more than 120 seconds.")
        );
        assert!(looks_like_kernel_lockup(kmsg_message(record).unwrap()));
        assert!(!looks_like_kernel_lockup("ordinary boot message"));
    }
}
