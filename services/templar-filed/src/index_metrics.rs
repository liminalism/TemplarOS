//! Process-local snapshot export accounting. No descriptor is retained for telemetry.
//!
//! SCM_RIGHTS transfers are not leases: once a reply leaves the server, client closes are not
//! observable. These counts cover this daemon's descriptors, including writes and queued replies.
use parking_lot::Mutex;
use std::{collections::HashMap, fs::File, os::unix::fs::MetadataExt, time::Instant};
use templar_file_index::{IndexError, SnapshotMetrics};

type Key = (u64, u64);
#[derive(Debug, Default)]
struct State {
    created: HashMap<Key, Instant>,
    completed: u64,
    cancelled: u64,
    rejected_too_large: u64,
}
#[derive(Debug, Default)]
pub struct SnapshotMeter(Mutex<State>);

/// Inspect only our own descriptor table, deduplicating writable/read-only handles to one inode.
fn live_fds() -> std::io::Result<HashMap<Key, u64>> {
    let mut live = HashMap::new();
    for entry in std::fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        // Descriptors can close between read_dir and stat; such handles are no longer held here.
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        live.insert((meta.dev(), meta.ino()), meta.len());
    }
    Ok(live)
}
impl SnapshotMeter {
    pub fn track(&self, file: &File) -> std::io::Result<()> {
        let meta = file.metadata()?;
        let live = live_fds();
        let mut state = self.0.lock();
        if let Ok(live) = live {
            state.created.retain(|key, _| live.contains_key(key));
        }
        state
            .created
            .insert((meta.dev(), meta.ino()), Instant::now());
        Ok(())
    }
    pub fn record<T>(&self, result: &Result<T, IndexError>) {
        let mut state = self.0.lock();
        match result {
            Ok(_) => state.completed += 1,
            Err(IndexError::TooLarge { .. }) => state.rejected_too_large += 1,
            Err(IndexError::Cancelled) => state.cancelled += 1,
            Err(_) => {}
        }
    }
    pub fn metrics(&self) -> SnapshotMetrics {
        let live = live_fds();
        let mut state = self.0.lock();
        let mut metrics = SnapshotMetrics {
            scope: "server-held-descriptors".into(),
            accounting_available: live.is_ok(),
            completed: state.completed,
            cancelled: state.cancelled,
            rejected_too_large: state.rejected_too_large,
            ..SnapshotMetrics::default()
        };
        if let Ok(live) = live {
            state.created.retain(|key, _| live.contains_key(key));
            metrics.open = state.created.len();
            metrics.live_memfd_bytes = state.created.keys().map(|key| live[key]).sum();
            metrics.oldest_age_ms = state
                .created
                .values()
                .min()
                .map(|at| at.elapsed().as_millis() as u64);
        }
        metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    #[test]
    fn snapshot_metrics_follow_server_fds_without_retaining_them() {
        let meter = SnapshotMeter::default();
        let fd = rustix::fs::memfd_create(
            "templar-index-snapshot-test",
            rustix::fs::MemfdFlags::CLOEXEC,
        )
        .unwrap();
        let mut file = File::from(fd);
        meter.track(&file).unwrap();
        file.write_all(&[7; 1024]).unwrap();
        let readonly = File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
        assert_eq!(meter.metrics().open, 1);
        assert_eq!(meter.metrics().live_memfd_bytes, 1024);
        drop(file);
        assert_eq!(meter.metrics().open, 1);
        drop(readonly);
        assert_eq!(meter.metrics().open, 0);
        assert_eq!(meter.metrics().live_memfd_bytes, 0);
        meter.record::<()>(&Err(IndexError::TooLarge {
            entries: 1,
            bytes: 1024,
        }));
        meter.record::<()>(&Err(IndexError::Cancelled));
        meter.record(&Ok(()));
        let metrics = meter.metrics();
        assert_eq!(
            (
                metrics.completed,
                metrics.cancelled,
                metrics.rejected_too_large
            ),
            (1, 1, 1)
        );
        assert!(metrics.accounting_available);
        assert_eq!(metrics.scope, "server-held-descriptors");
    }
}
