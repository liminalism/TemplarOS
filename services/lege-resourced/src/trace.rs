//! Bounded, append-only trace recording (planning doc 02 §8: "only after recorded playthroughs
//! show sensible choices should `memory.high` and proactive reclaim be enabled").
//!
//! This is the evidence a human reviews before raising the [`crate::policy::Stage`], so it must
//! be complete (every input the decision used) and readable (one JSON object per line). It is
//! **not** state the governor depends on: [`TraceRing`] never blocks the sampling loop on disk —
//! every write is best-effort, and failures are logged and dropped, never propagated (ADR-0002:
//! no fsync on the loop). The in-memory ring (bounded by count) always answers `RecentTraces`
//! even if the disk write failed or the directory is not writable at all; the on-disk file
//! (bounded by size, with one rotation) is what survives a restart.

use crate::policy::{PressureLevel, ScoreBreakdown, Stage};
use crate::psi::Psi;
use lege_types::InstanceId;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// One instance's contribution to a [`TraceRecord`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceTrace {
    pub instance_id: InstanceId,
    pub app_id: String,
    pub memory_current_bytes: u64,
    pub memory_growth_bytes_per_second: i64,
    pub full_psi_avg10: f64,
    pub foreground: bool,
    pub visible: bool,
    pub protected: bool,
    pub disposable: bool,
    pub has_unsaved_work: bool,
    /// The signals that reached scoring after the first four. Every one is `#[serde(default)]`,
    /// which is the additive-fields rule doing its job: a trace recorded before these existed
    /// replays as "nobody said", which is exactly what was true when it was recorded.
    #[serde(default)]
    pub last_interaction_age_ms: Option<u64>,
    #[serde(default)]
    pub project_important: bool,
    #[serde(default)]
    pub supports_checkpoint: bool,
    #[serde(default)]
    pub checkpoint_age_ms: Option<u64>,
    #[serde(default)]
    pub user_pinned: bool,
    pub victim_score: f64,
    pub score_breakdown: ScoreBreakdown,
}

/// One decided action, whether or not the current stage allowed it to be performed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionTrace {
    pub kind: String,
    pub instance_id: InstanceId,
    pub applied: bool,
    pub reason: String,
}

/// One tick's worth of decision-relevant state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceRecord {
    /// Format version, so a trace recorded by an older build can say what it is. The rule for
    /// changing this format is additive only — new fields default, existing ones never change
    /// meaning — which is what keeps last month's recordings replayable by this month's binary
    /// (`crate::replay`). A record written before versioning existed reads as version 1.
    #[serde(default = "default_trace_version")]
    pub trace_version: u32,
    pub timestamp_ms: i64,
    pub level: PressureLevel,
    pub stage: Stage,
    pub mem_available_bytes: u64,
    pub global_memory_psi: Psi,
    pub global_io_psi: Psi,
    pub instances: Vec<InstanceTrace>,
    pub victim: Option<InstanceId>,
    pub actions: Vec<ActionTrace>,
    /// Why the loop was awake for this tick: the kernel's pressure trigger fired, the idle
    /// heartbeat came round, or there is no trigger on this kernel and the loop is sampling on
    /// the clock. Serde-defaulted, so every trace recorded before the loop became event-driven
    /// still replays — it reads as `timeout`, which is what an interval sample was.
    #[serde(default)]
    pub wakeup: crate::psi::Wakeup,
}

fn default_trace_version() -> u32 {
    crate::replay::TRACE_VERSION
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// A bounded ring: `max_records` in memory (what `RecentTraces` serves from), a rotated on-disk
/// JSONL file capped at `max_file_bytes` (one `.1` backup, then overwritten).
#[derive(Debug)]
pub struct TraceRing {
    memory: VecDeque<String>,
    max_records: usize,
    path: PathBuf,
    max_file_bytes: u64,
    file: Option<File>,
}

impl TraceRing {
    /// Best-effort open: `dir` is created if missing, and the file is opened for append. Any
    /// failure downgrades to memory-only recording (logged once, not fatal).
    pub fn open(dir: &Path, max_records: usize, max_file_bytes: u64) -> Self {
        let path = dir.join("traces.jsonl");
        let file = fs::create_dir_all(dir)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&path))
            .map_err(|error| {
                tracing::warn!(path = %path.display(), %error, "trace ring: disk logging disabled");
            })
            .ok();
        Self {
            memory: VecDeque::with_capacity(max_records.min(4096)),
            max_records: max_records.max(1),
            path,
            max_file_bytes: max_file_bytes.max(4096),
            file,
        }
    }

    /// In-memory only — used by `--once` and by tests that do not want a filesystem dependency.
    pub fn in_memory(max_records: usize) -> Self {
        Self {
            memory: VecDeque::with_capacity(max_records.min(4096)),
            max_records: max_records.max(1),
            path: PathBuf::new(),
            max_file_bytes: 0,
            file: None,
        }
    }

    pub fn append(&mut self, record: &TraceRecord) {
        let Ok(line) = serde_json::to_string(record) else {
            tracing::warn!("trace ring: record did not serialize; dropped");
            return;
        };

        if self.memory.len() >= self.max_records {
            self.memory.pop_front();
        }
        self.memory.push_back(line.clone());

        if let Some(file) = self.file.as_mut() {
            if let Err(error) = writeln!(file, "{line}") {
                tracing::warn!(%error, "trace ring: disk write failed; continuing in memory only");
                self.file = None;
            } else {
                self.maybe_rotate();
            }
        }
    }

    fn maybe_rotate(&mut self) {
        let Ok(metadata) = self.path.metadata() else {
            return;
        };
        if metadata.len() < self.max_file_bytes {
            return;
        }
        self.file = None;
        let backup = self.path.with_extension("jsonl.1");
        let _ = fs::rename(&self.path, &backup);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| {
                tracing::warn!(path = %self.path.display(), %error, "trace ring: could not reopen after rotation");
            })
            .ok();
    }

    /// Newest-first JSON lines, at most `limit` of them.
    pub fn recent(&self, limit: usize) -> Vec<String> {
        self.memory.iter().rev().take(limit).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.memory.len()
    }

    pub fn is_empty(&self) -> bool {
        self.memory.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psi::PsiSample;

    fn sample_record(level: PressureLevel) -> TraceRecord {
        TraceRecord {
            wakeup: crate::psi::Wakeup::default(),
            trace_version: crate::replay::TRACE_VERSION,
            timestamp_ms: now_ms(),
            level,
            stage: Stage::ObserveOnly,
            mem_available_bytes: GIB,
            global_memory_psi: Psi::default(),
            global_io_psi: Psi::default(),
            instances: Vec::new(),
            victim: None,
            actions: Vec::new(),
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn recent_returns_newest_first_and_respects_limit() {
        let mut ring = TraceRing::in_memory(10);
        ring.append(&sample_record(PressureLevel::Normal));
        ring.append(&sample_record(PressureLevel::Elevated));
        ring.append(&sample_record(PressureLevel::Severe));

        let recent = ring.recent(2);
        assert_eq!(recent.len(), 2);
        assert!(recent[0].contains("\"severe\""));
        assert!(recent[1].contains("\"elevated\""));
    }

    #[test]
    fn memory_ring_is_bounded_by_count() {
        let mut ring = TraceRing::in_memory(3);
        for _ in 0..10 {
            ring.append(&sample_record(PressureLevel::Normal));
        }
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn disk_file_persists_and_rotates() {
        let dir = std::env::temp_dir().join(format!("lege-resourced-trace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        {
            let mut ring = TraceRing::open(&dir, 1000, 2048);
            for _ in 0..64 {
                ring.append(&sample_record(PressureLevel::Severe));
            }
        }

        let path = dir.join("traces.jsonl");
        assert!(path.exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.is_empty());
        // At 2KiB rotation with 64 records of real JSON, at least one rotation must have
        // happened, leaving a `.1` backup alongside a fresh, smaller current file.
        assert!(dir.join("traces.jsonl.1").exists());
        assert!(text.len() < 2048 * 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_on_an_unwritable_directory_degrades_to_memory_only() {
        // A path with a regular file as a component cannot be created as a directory.
        let dir = std::env::temp_dir().join(format!(
            "lege-resourced-trace-blocked-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::write(&dir, b"not a directory").unwrap();

        let mut ring = TraceRing::open(&dir.join("nested"), 10, 4096);
        ring.append(&sample_record(PressureLevel::Normal));
        assert_eq!(ring.len(), 1);

        let _ = fs::remove_file(&dir);
    }

    #[test]
    fn serializes_scores_and_actions_for_review() {
        let mut record = sample_record(PressureLevel::Critical);
        record.global_memory_psi.full = PsiSample {
            avg10: 4.5,
            ..PsiSample::default()
        };
        record.actions.push(ActionTrace {
            kind: "kill-cgroup".into(),
            instance_id: InstanceId::new(),
            applied: false,
            reason: "above observe-only stage".into(),
        });
        let json = serde_json::to_string(&record).unwrap();
        let back: TraceRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }
}
