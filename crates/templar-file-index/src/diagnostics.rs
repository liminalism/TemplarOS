//! Store-free diagnostic payloads shared by the owner and its clients.
use crate::{Availability, Generation, RootId, RootStatus, StorageBytes};
use std::collections::BTreeMap;

/// One column of the search arrays: what it holds and what it reserved.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArrayColumn {
    pub name: String,
    pub bytes: u64,
    pub capacity_bytes: u64,
}

/// The search arrays' size, for the layout measurement and diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ArrayStats {
    pub generation: Generation,
    pub rows: u64,
    pub bytes: u64,
    pub capacity_bytes: u64,
    pub columns: Vec<ArrayColumn>,
    #[serde(default)]
    pub roots: Vec<RootArrayStats>,
    #[serde(default)]
    pub retained_bytes: u64,
    pub hits: u64,
    pub misses: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RootArrayStats {
    pub root: RootId,
    pub generation: Generation,
    pub built_change_seq: u64,
    pub rows: u64,
    pub bytes: u64,
    pub last_build_ms: Option<u64>,
    pub refusal: Option<String>,
    #[serde(default)]
    pub overlay_rows: u64,
    #[serde(default)]
    pub overlay_bytes: u64,
    #[serde(default)]
    pub overlay_change_seq: u64,
    pub valid: bool,
    pub building: bool,
    pub dirty: bool,
    pub build_peak_bytes: u64,
    pub hits: u64,
    pub misses: BTreeMap<String, u64>,
}

impl Default for RootArrayStats {
    fn default() -> Self {
        Self {
            root: RootId(0),
            generation: Generation(0),
            built_change_seq: 0,
            rows: 0,
            bytes: 0,
            last_build_ms: None,
            refusal: None,
            overlay_rows: 0,
            overlay_bytes: 0,
            overlay_change_seq: 0,
            valid: false,
            building: false,
            dirty: false,
            build_peak_bytes: 0,
            hits: 0,
            misses: BTreeMap::new(),
        }
    }
}

/// What `templar doctor` and `templar files index status` report.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Diagnostics {
    pub path: String,
    pub schema_version: u32,
    pub format_version: u32,
    pub scanner_version: u32,
    /// Main file, write-ahead log and shared-memory index together (`storage`'s total). Kept for
    /// the readers that predate `storage`.
    pub db_bytes: u64,
    pub storage: StorageBytes,
    /// Whether the cache is open at all. A daemon serving an unavailable cache reports this with
    /// no roots rather than failing the call.
    pub availability: Availability,
    pub generation: Generation,
    pub planner: String,
    pub fts_available: bool,
    pub change_seq_floor: u64,
    pub change_seq_head: u64,
    pub cursor_count: usize,
    #[serde(default)]
    pub arrays: Option<crate::ArrayStats>,
    pub roots: Vec<RootStatus>,
    /// Absent in read-only disk inspection and older services.
    pub runtime: Option<RuntimeDiagnostics>,
}

/// The reader side of the index at one moment, for diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ReaderMetrics {
    pub open: usize,
    pub cap: usize,
    pub in_use: usize,
    /// Age of the oldest read transaction in progress. A long one keeps the WAL from being
    /// checkpointed past it.
    pub oldest_read_ms: Option<u64>,
}

/// Cursor retention after expiring idle owners, ready for runtime diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CursorMetrics {
    pub count: usize,
    pub per_owner: std::collections::BTreeMap<String, usize>,
    pub retained_rows: usize,
    pub next_expiry_ms: Option<u64>,
    pub oldest_pinned_generation: Option<Generation>,
}

/// One observation of process-local state; counters reset when the owner restarts.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RuntimeDiagnostics {
    pub lanes: Vec<LaneMetrics>,
    pub arrays: Option<ArrayStats>,
    pub snapshots: Option<SnapshotMetrics>,
    pub cursors: CursorMetrics,
    pub readers: ReaderMetrics,
    pub wal: WalMetrics,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LaneMetrics {
    pub lane: String,
    pub workers: usize,
    pub queued: usize,
    pub running: usize,
    pub wait_p50_ms: u64,
    pub wait_p95_ms: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub rejected_busy: u64,
}

/// Server-held memfds only. Client-held descriptors and mappings are not observable here.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SnapshotMetrics {
    pub scope: String,
    pub accounting_available: bool,
    pub open: usize,
    pub live_memfd_bytes: u64,
    pub oldest_age_ms: Option<u64>,
    pub completed: u64,
    pub cancelled: u64,
    pub rejected_too_large: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WalMetrics {
    pub mode: Option<String>,
    pub busy: Option<i64>,
    pub log: Option<i64>,
    pub checkpointed: Option<i64>,
    pub at_ms: Option<i64>,
    pub passive_count: u64,
    pub truncate_count: u64,
    pub queued: bool,
    pub last_error: Option<String>,
}

impl ArrayStats {
    pub fn query_count(&self) -> u64 {
        self.hits + self.misses.values().sum::<u64>()
    }
    pub fn hit_rate(&self) -> Option<f64> {
        let count = self.query_count();
        (count > 0).then(|| 100.0 * self.hits as f64 / count as f64)
    }
}

/// Complete text for an explicitly requested diagnostic view; absent runtime is never zero activity.
pub fn runtime_lines(runtime: Option<&RuntimeDiagnostics>) -> Vec<String> {
    let Some(runtime) = runtime else {
        return vec!["runtime: unavailable (read-only disk inspection or older service)".into()];
    };
    let mut lines = vec!["runtime: owner process; counters reset on restart".into()];
    for lane in &runtime.lanes {
        lines.push(format!("lane {}: workers={} queued={} running={} wait_p50_ms={} wait_p95_ms={} completed={} cancelled={} rejected_busy={}",
            lane.lane, lane.workers, lane.queued, lane.running, lane.wait_p50_ms, lane.wait_p95_ms,
            lane.completed, lane.cancelled, lane.rejected_busy));
    }
    if let Some(arrays) = &runtime.arrays {
        lines.push(format!("arrays: bytes={} retained_bytes={} capacity_bytes={} rows={} hits={} misses={:?} hit_rate={} queries={}",
            arrays.bytes, arrays.retained_bytes, arrays.capacity_bytes, arrays.rows, arrays.hits, arrays.misses,
            arrays.hit_rate().map_or_else(|| "unmeasured".into(), |rate| format!("{rate:.2}%")), arrays.query_count()));
        for root in &arrays.roots {
            lines.push(format!("shard {}: generation={} valid={} rows={} bytes={} building={} dirty={} last_build_ms={:?} build_peak_bytes={} hits={} misses={:?} overlay_rows={} overlay_bytes={} overlay_seq={} refusal={:?}",
                root.root.0, root.generation.0, root.valid, root.rows, root.bytes, root.building, root.dirty,
                root.last_build_ms, root.build_peak_bytes, root.hits, root.misses, root.overlay_rows,
                root.overlay_bytes, root.overlay_change_seq, root.refusal));
        }
    } else {
        lines.push("arrays: disabled".into());
    }
    if let Some(snapshots) = &runtime.snapshots {
        lines.push(format!("snapshots: scope={} accounting_available={} open={} live_memfd_bytes={} oldest_age_ms={:?} completed={} cancelled={} rejected_too_large={}",
            snapshots.scope, snapshots.accounting_available, snapshots.open, snapshots.live_memfd_bytes,
            snapshots.oldest_age_ms, snapshots.completed, snapshots.cancelled, snapshots.rejected_too_large));
    } else {
        lines.push("snapshots: unavailable in this probe".into());
    }
    let cursors = &runtime.cursors;
    lines.push(format!("cursors: count={} per_owner={:?} retained_rows={} next_expiry_ms={:?} oldest_pinned_generation={:?}",
        cursors.count, cursors.per_owner, cursors.retained_rows, cursors.next_expiry_ms, cursors.oldest_pinned_generation.map(|g| g.0)));
    let readers = &runtime.readers;
    lines.push(format!(
        "readers: open={} cap={} in_use={} oldest_read_ms={:?}",
        readers.open, readers.cap, readers.in_use, readers.oldest_read_ms
    ));
    let wal = &runtime.wal;
    lines.push(format!("wal: mode={:?} busy={:?} log={:?} checkpointed={:?} at_ms={:?} passive_count={} truncate_count={} queued={} last_error={:?}",
        wal.mode, wal.busy, wal.log, wal.checkpointed, wal.at_ms, wal.passive_count, wal.truncate_count, wal.queued, wal.last_error));
    lines
}

pub fn health_warnings(storage: StorageBytes, runtime: Option<&RuntimeDiagnostics>) -> Vec<String> {
    let mut warnings = Vec::new();
    if storage.wal_bytes > 256 * 1024 * 1024 {
        warnings.push("WAL exceeds 256 MiB".into());
    }
    if let Some(runtime) = runtime {
        if runtime
            .readers
            .oldest_read_ms
            .is_some_and(|age| age > 60_000)
        {
            warnings.push("oldest reader exceeds 60 seconds".into());
        }
        if let Some(arrays) = &runtime.arrays
            && arrays.query_count() > 100
            && arrays.hit_rate().is_some_and(|rate| rate < 50.0)
        {
            warnings.push("arrays hit rate below 50% after more than 100 queries".into());
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_warnings_have_explicit_thresholds_and_unobserved_is_not_idle() {
        let mut runtime = RuntimeDiagnostics::default();
        runtime.readers.oldest_read_ms = Some(60_001);
        runtime.arrays = Some(ArrayStats {
            hits: 40,
            misses: BTreeMap::from([("stale".into(), 61)]),
            ..Default::default()
        });
        assert_eq!(
            health_warnings(
                StorageBytes {
                    wal_bytes: 256 * 1024 * 1024 + 1,
                    ..Default::default()
                },
                Some(&runtime)
            )
            .len(),
            3
        );
        runtime.readers.oldest_read_ms = Some(60_000);
        runtime
            .arrays
            .as_mut()
            .unwrap()
            .misses
            .insert("stale".into(), 60);
        assert!(
            health_warnings(
                StorageBytes {
                    wal_bytes: 256 * 1024 * 1024,
                    ..Default::default()
                },
                Some(&runtime)
            )
            .is_empty()
        );
        assert!(runtime_lines(None)[0].contains("unavailable"));
        assert_eq!(
            serde_json::from_str::<RuntimeDiagnostics>("{}").unwrap(),
            RuntimeDiagnostics::default()
        );
    }
}
