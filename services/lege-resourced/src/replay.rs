//! Replay: run recorded pressure back through the current policy and diff the outcome.
//!
//! The governor's weights and thresholds are, by the module's own admission, starting values to
//! be calibrated from real traces. Recording those traces has worked since the beginning
//! ([`crate::trace`]); reading them back has not existed at all, which means every change to the
//! policy so far has been argued from reasoning rather than from evidence. This closes that: a
//! trace holds every input a decision used, so the same inputs can be pushed through today's code
//! and the difference read off.
//!
//! **What a green replay proves, and what it does not.** It proves the policy still makes the
//! same decisions on the same inputs — that a refactor was a refactor, and that a deliberate
//! change moved exactly what it meant to move. It proves nothing at all about whether those
//! decisions are *right*, and the fixtures under `tests/fixtures/resources/replay/` are
//! hand-authored scenarios rather than recordings of anyone's actual desktop. Calibration needs
//! real traces from real sessions; this is the harness that will make them worth collecting.
//!
//! **Old recordings stay replayable.** Records carry a `trace_version`, and the rule for
//! changing the format is additive only: new fields get `#[serde(default)]`, existing fields
//! never change meaning. A trace recorded last month has to survive being read by this month's
//! binary, or the evidence base resets every time the code moves — which is the same failure the
//! recordings were meant to prevent.

use crate::{
    policy::{Action, AppSample, Governor, GovernorConfig, PressureLevel},
    trace::{InstanceTrace, TraceRecord},
};
use lege_types::InstanceId;
use std::{
    fmt::Write as _,
    path::Path,
    time::{Duration, Instant},
};

/// The trace format this build writes.
///
/// Bumped only for a change that is *not* additive — which the rule above says should not happen.
/// It exists so that if one ever does, the reader can say so instead of silently misreading.
pub const TRACE_VERSION: u32 = 1;

/// The inputs a decision used, recovered from one trace record.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayedTick {
    pub timestamp_ms: i64,
    pub mem_available_bytes: u64,
    pub some_avg10: f64,
    pub full_avg10: f64,
    pub applications: Vec<AppSample>,
}

/// Everything an [`InstanceTrace`] knew about an application, as the governor sees it.
///
/// The record is deliberately a superset — it also carries the score the run computed, so a human
/// reading the JSONL can see *why* — but only these fields are inputs. Reconstructing from the
/// inputs alone is what makes the replay a test of the policy rather than a re-run of its
/// conclusions.
#[must_use]
pub fn sample_from(trace: &InstanceTrace) -> AppSample {
    AppSample {
        instance_id: trace.instance_id,
        memory_current: trace.memory_current_bytes,
        memory_growth_per_second: trace.memory_growth_bytes_per_second,
        full_psi_avg10: trace.full_psi_avg10,
        foreground: trace.foreground,
        visible: trace.visible,
        protected: trace.protected,
        reports_unsaved_state: trace.has_unsaved_work,
        disposable: trace.disposable,
        last_interaction_age: trace.last_interaction_age_ms.map(Duration::from_millis),
        project_important: trace.project_important,
        supports_checkpoint: trace.supports_checkpoint,
        checkpoint_age: trace.checkpoint_age_ms.map(Duration::from_millis),
        user_pinned: trace.user_pinned,
    }
}

/// The decision inputs of one record.
#[must_use]
pub fn tick_from(record: &TraceRecord) -> ReplayedTick {
    ReplayedTick {
        timestamp_ms: record.timestamp_ms,
        mem_available_bytes: record.mem_available_bytes,
        some_avg10: record.global_memory_psi.some.avg10,
        full_avg10: record.global_memory_psi.full.avg10,
        applications: record.instances.iter().map(sample_from).collect(),
    }
}

/// One record where the current policy disagrees with what was recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct Divergence {
    pub index: usize,
    pub timestamp_ms: i64,
    pub recorded_level: PressureLevel,
    pub replayed_level: PressureLevel,
    /// `kind` of each recorded action, in order.
    pub recorded_actions: Vec<String>,
    pub replayed_actions: Vec<String>,
    pub recorded_victim: Option<InstanceId>,
    pub replayed_victim: Option<InstanceId>,
}

impl Divergence {
    #[must_use]
    pub fn level_changed(&self) -> bool {
        self.recorded_level != self.replayed_level
    }

    #[must_use]
    pub fn actions_changed(&self) -> bool {
        self.recorded_actions != self.replayed_actions
    }

    #[must_use]
    pub fn victim_changed(&self) -> bool {
        self.recorded_victim != self.replayed_victim
    }
}

/// The result of replaying one trace file.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayReport {
    pub source: String,
    pub records: usize,
    /// Records the reader could not parse at all — a genuinely broken format, not an old one.
    pub unreadable: usize,
    pub divergences: Vec<Divergence>,
}

impl ReplayReport {
    #[must_use]
    pub fn agrees(&self) -> bool {
        self.divergences.is_empty() && self.unreadable == 0
    }

    /// The diff, in the form a commit message can quote.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{}: {} record(s)", self.source, self.records);
        if self.unreadable > 0 {
            let _ = write!(out, ", {} unreadable", self.unreadable);
        }
        if self.divergences.is_empty() {
            out.push_str(" — identical\n");
            return out;
        }
        let _ = writeln!(out, " — {} diverge", self.divergences.len());
        for divergence in &self.divergences {
            let _ = write!(out, "  #{:<4}", divergence.index);
            if divergence.level_changed() {
                let _ = write!(
                    out,
                    " level {:?} → {:?}",
                    divergence.recorded_level, divergence.replayed_level
                );
            }
            if divergence.victim_changed() {
                let _ = write!(
                    out,
                    " victim {} → {}",
                    short(divergence.recorded_victim),
                    short(divergence.replayed_victim)
                );
            }
            if divergence.actions_changed() {
                let _ = write!(
                    out,
                    " actions [{}] → [{}]",
                    divergence.recorded_actions.join(" "),
                    divergence.replayed_actions.join(" ")
                );
            }
            out.push('\n');
        }
        out
    }
}

fn short(id: Option<InstanceId>) -> String {
    id.map_or_else(
        || "none".to_owned(),
        |id| id.to_string().chars().take(8).collect(),
    )
}

/// Replays a JSONL trace through a governor built from `config`.
///
/// Time is reconstructed from the records' own timestamps, offset from a single `Instant` taken
/// at the start. That matters for `Critical`: the emergency kill is gated on how long the level
/// has persisted, so replaying with wall-clock time would decide differently from the run being
/// replayed for reasons that have nothing to do with the policy.
#[must_use]
pub fn replay(source: &str, jsonl: &str, config: GovernorConfig) -> ReplayReport {
    let mut governor = Governor::new(config);
    let base = Instant::now();
    let mut first_timestamp: Option<i64> = None;
    let mut records = 0;
    let mut unreadable = 0;
    let mut divergences = Vec::new();

    for (index, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<TraceRecord>(line) else {
            unreadable += 1;
            continue;
        };
        records += 1;

        let tick = tick_from(&record);
        let origin = *first_timestamp.get_or_insert(tick.timestamp_ms);
        let elapsed = u64::try_from(tick.timestamp_ms.saturating_sub(origin)).unwrap_or(0);
        let now = base + Duration::from_millis(elapsed);

        let level = governor.tick_level(tick.mem_available_bytes, tick.some_avg10, tick.full_avg10);
        let actions = governor.decide(now, level, &tick.applications);

        let recorded_actions: Vec<String> = record
            .actions
            .iter()
            .map(|action| action.kind.clone())
            .collect();
        let replayed_actions: Vec<String> = actions
            .iter()
            .map(|action| action.kind().to_owned())
            .collect();
        let replayed_victim = victim_of(&actions);

        if level != record.level
            || recorded_actions != replayed_actions
            || record.victim != replayed_victim
        {
            divergences.push(Divergence {
                index,
                timestamp_ms: record.timestamp_ms,
                recorded_level: record.level,
                replayed_level: level,
                recorded_actions,
                replayed_actions,
                recorded_victim: record.victim,
                replayed_victim,
            });
        }
    }

    ReplayReport {
        source: source.to_owned(),
        records,
        unreadable,
        divergences,
    }
}

/// Which instance a tick's actions were aimed at.
///
/// `RestoreDynamicLimits` is emitted for every application at `Normal` and names no victim, so it
/// is excluded — otherwise every calm tick would claim whichever instance happened to sort first.
fn victim_of(actions: &[Action]) -> Option<InstanceId> {
    actions
        .iter()
        .find(|action| !matches!(action, Action::RestoreDynamicLimits { .. }))
        .map(Action::instance_id)
}

/// Replays every `.jsonl` file in a directory, or one file.
///
/// # Errors
/// If the path cannot be read.
pub fn replay_path(path: &Path, config: GovernorConfig) -> std::io::Result<Vec<ReplayReport>> {
    if path.is_dir() {
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(path)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        return files.iter().map(|file| replay_file(file, config)).collect();
    }
    Ok(vec![replay_file(path, config)?])
}

fn replay_file(path: &Path, config: GovernorConfig) -> std::io::Result<ReplayReport> {
    let text = std::fs::read_to_string(path)?;
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    Ok(replay(&name, &text, config))
}

/// Rewrites a trace's recorded level, victim and actions to what today's policy decides.
///
/// The fixtures' *inputs* are hand-authored; their recorded outputs are whatever the policy said
/// when they were last blessed. Blessing is therefore how a deliberate policy change is committed
/// — after its diff has been read and quoted — and never a way to make a surprising diff go away.
#[must_use]
pub fn bless(jsonl: &str, config: GovernorConfig) -> String {
    let mut governor = Governor::new(config);
    let base = Instant::now();
    let mut first_timestamp: Option<i64> = None;
    let mut out = String::new();

    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(mut record) = serde_json::from_str::<TraceRecord>(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let tick = tick_from(&record);
        let origin = *first_timestamp.get_or_insert(tick.timestamp_ms);
        let elapsed = u64::try_from(tick.timestamp_ms.saturating_sub(origin)).unwrap_or(0);
        let now = base + Duration::from_millis(elapsed);

        let level = governor.tick_level(tick.mem_available_bytes, tick.some_avg10, tick.full_avg10);
        let actions = governor.decide(now, level, &tick.applications);
        record.level = level;
        record.victim = victim_of(&actions);
        record.actions = actions
            .iter()
            .map(|action| crate::trace::ActionTrace {
                kind: action.kind().to_owned(),
                instance_id: action.instance_id(),
                applied: false,
                reason: format!("replayed at {:?}", record.stage),
            })
            .collect();
        record.trace_version = TRACE_VERSION;
        if let Ok(line) = serde_json::to_string(&record) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        policy::Stage,
        psi::{Psi, PsiSample},
        trace::ActionTrace,
    };

    const GIB: u64 = 1024 * 1024 * 1024;

    fn instance(memory: u64, disposable: bool) -> InstanceTrace {
        InstanceTrace {
            instance_id: InstanceId::new(),
            app_id: "org.legeos.Test".to_owned(),
            memory_current_bytes: memory,
            memory_growth_bytes_per_second: 0,
            full_psi_avg10: 0.0,
            foreground: false,
            visible: false,
            protected: false,
            disposable,
            has_unsaved_work: false,
            last_interaction_age_ms: None,
            project_important: false,
            supports_checkpoint: false,
            checkpoint_age_ms: None,
            user_pinned: false,
            victim_score: 0.0,
            score_breakdown: crate::policy::ScoreBreakdown::default(),
        }
    }

    fn record(available: u64, some: f64, full: f64, instances: Vec<InstanceTrace>) -> TraceRecord {
        TraceRecord {
            trace_version: TRACE_VERSION,
            timestamp_ms: 0,
            level: PressureLevel::Normal,
            stage: Stage::ObserveOnly,
            mem_available_bytes: available,
            global_memory_psi: Psi {
                some: PsiSample {
                    avg10: some,
                    ..PsiSample::default()
                },
                full: PsiSample {
                    avg10: full,
                    ..PsiSample::default()
                },
            },
            global_io_psi: Psi::default(),
            instances,
            victim: None,
            actions: Vec::new(),
        }
    }

    fn jsonl(records: &[TraceRecord]) -> String {
        records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_sample_round_trips_through_a_trace_record() {
        // The load-bearing property of the whole harness: what comes back out of a record is
        // exactly what went into the decision. If reconstruction quietly dropped or defaulted a
        // field, every replay would agree with every recording and prove nothing.
        let source = AppSample {
            instance_id: InstanceId::new(),
            memory_current: 3 * GIB,
            memory_growth_per_second: -17,
            full_psi_avg10: 2.5,
            foreground: true,
            visible: true,
            protected: true,
            reports_unsaved_state: true,
            disposable: true,
            last_interaction_age: Some(Duration::from_secs(90)),
            project_important: true,
            supports_checkpoint: true,
            checkpoint_age: Some(Duration::from_secs(30)),
            user_pinned: true,
        };
        let trace = InstanceTrace {
            instance_id: source.instance_id,
            app_id: "org.legeos.Test".to_owned(),
            memory_current_bytes: source.memory_current,
            memory_growth_bytes_per_second: source.memory_growth_per_second,
            full_psi_avg10: source.full_psi_avg10,
            foreground: source.foreground,
            visible: source.visible,
            protected: source.protected,
            disposable: source.disposable,
            has_unsaved_work: source.reports_unsaved_state,
            last_interaction_age_ms: Some(90_000),
            project_important: source.project_important,
            supports_checkpoint: source.supports_checkpoint,
            checkpoint_age_ms: Some(30_000),
            user_pinned: source.user_pinned,
            victim_score: 12.5,
            score_breakdown: crate::policy::ScoreBreakdown::default(),
        };
        let text = serde_json::to_string(&trace).unwrap();
        let back: InstanceTrace = serde_json::from_str(&text).unwrap();
        assert_eq!(sample_from(&back), source);
    }

    #[test]
    fn every_input_field_survives_the_round_trip_individually() {
        // Field by field, so a reconstruction that hardcoded a `false` somewhere is caught by
        // the field it hardcoded rather than by luck.
        for (name, mutate) in [
            (
                "foreground",
                Box::new(|t: &mut InstanceTrace| t.foreground = true) as Box<dyn Fn(&mut _)>,
            ),
            (
                "visible",
                Box::new(|t: &mut InstanceTrace| t.visible = true),
            ),
            (
                "protected",
                Box::new(|t: &mut InstanceTrace| t.protected = true),
            ),
            (
                "disposable",
                Box::new(|t: &mut InstanceTrace| t.disposable = true),
            ),
            (
                "has_unsaved_work",
                Box::new(|t: &mut InstanceTrace| t.has_unsaved_work = true),
            ),
        ] {
            let mut trace = instance(GIB, false);
            let before = sample_from(&trace);
            mutate(&mut trace);
            let after = sample_from(&trace);
            assert_ne!(before, after, "{name} does not reach the replayed sample");
        }
    }

    #[test]
    fn a_trace_replayed_through_the_policy_that_wrote_it_is_identical() {
        let instances = vec![instance(4 * GIB, true), instance(GIB, false)];
        let text = bless(
            &jsonl(&[
                record(8 * GIB, 0.0, 0.0, instances.clone()),
                record(2 * GIB, 5.0, 1.0, instances.clone()),
                record(512 * 1024 * 1024, 9.0, 4.0, instances),
            ]),
            GovernorConfig::default(),
        );
        let report = replay("fixture", &text, GovernorConfig::default());
        assert_eq!(report.records, 3);
        assert!(report.agrees(), "{}", report.render());
    }

    #[test]
    fn a_changed_threshold_shows_up_as_a_level_divergence() {
        // What the harness is for: a policy change is visible as a diff against the recording,
        // rather than as a test that has to be rewritten to match the new behaviour.
        let instances = vec![instance(4 * GIB, false)];
        let text = bless(
            &jsonl(&[record(2 * GIB, 0.0, 0.0, instances)]),
            GovernorConfig::default(),
        );
        let report = replay(
            "fixture",
            &text,
            GovernorConfig {
                reserve_bytes: GIB,
                ..GovernorConfig::default()
            },
        );
        assert_eq!(report.divergences.len(), 1);
        assert!(report.divergences[0].level_changed());
        assert!(report.render().contains("level"), "{}", report.render());
    }

    #[test]
    fn a_record_written_before_trace_versioning_still_replays() {
        // The additive-fields rule, asserted rather than promised. A trace from a build that had
        // no `trace_version` at all has to keep working, or the evidence base resets whenever the
        // format moves.
        let mut value =
            serde_json::to_value(record(8 * GIB, 0.0, 0.0, vec![instance(GIB, false)])).unwrap();
        value.as_object_mut().unwrap().remove("trace_version");
        let text = serde_json::to_string(&value).unwrap();
        let report = replay("old", &text, GovernorConfig::default());
        assert_eq!(report.records, 1, "{}", report.render());
        assert_eq!(report.unreadable, 0);
    }

    #[test]
    fn a_record_with_a_field_this_build_does_not_know_still_replays() {
        let mut value =
            serde_json::to_value(record(8 * GIB, 0.0, 0.0, vec![instance(GIB, false)])).unwrap();
        value.as_object_mut().unwrap().insert(
            "some_future_field".to_owned(),
            serde_json::Value::String("hello".to_owned()),
        );
        let report = replay(
            "future",
            &serde_json::to_string(&value).unwrap(),
            GovernorConfig::default(),
        );
        assert_eq!(report.records, 1, "{}", report.render());
    }

    #[test]
    fn genuinely_broken_lines_are_counted_not_silently_dropped() {
        let report = replay("broken", "{not json}\n\n", GovernorConfig::default());
        assert_eq!(report.records, 0);
        assert_eq!(report.unreadable, 1);
        assert!(!report.agrees());
    }

    #[test]
    fn a_calm_ticks_restore_actions_do_not_claim_a_victim() {
        let instances = vec![instance(GIB, false), instance(2 * GIB, false)];
        let text = bless(
            &jsonl(&[record(16 * GIB, 0.0, 0.0, instances)]),
            GovernorConfig::default(),
        );
        let record: TraceRecord = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record.level, PressureLevel::Normal);
        assert_eq!(record.victim, None, "a calm tick names no victim");
        assert!(record.actions.iter().all(|a| a.kind == "restore-limits"));
    }

    #[test]
    fn the_critical_kill_delay_is_replayed_from_the_records_own_clock() {
        // Replaying with wall-clock time would decide the emergency kill on how long the replay
        // took, which has nothing to do with the run being replayed.
        let instances = vec![instance(4 * GIB, false)];
        let mut records = Vec::new();
        for step in 0..6 {
            let mut record = record(256 * 1024 * 1024, 20.0, 8.0, instances.clone());
            record.timestamp_ms = i64::from(step) * 1000;
            records.push(record);
        }
        let text = bless(&jsonl(&records), GovernorConfig::default());
        let kinds: Vec<Vec<String>> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<TraceRecord>(line)
                    .unwrap()
                    .actions
                    .into_iter()
                    .map(|a: ActionTrace| a.kind)
                    .collect()
            })
            .collect();
        assert!(
            !kinds[0].contains(&"kill-cgroup".to_owned()),
            "the first critical tick must not kill: {kinds:?}"
        );
        assert!(
            kinds[5].contains(&"kill-cgroup".to_owned()),
            "three seconds of critical pressure must reach the kill decision: {kinds:?}"
        );
        // And the replay agrees with itself.
        assert!(
            replay("critical", &text, GovernorConfig::default()).agrees(),
            "{}",
            replay("critical", &text, GovernorConfig::default()).render()
        );
    }
}
