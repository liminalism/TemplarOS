//! State shared between the sampling loop (a plain OS thread — ADR-0002: a stalled bus must never
//! delay containment) and the D-Bus interface (`crate::dbus`, on the tokio runtime).
//!
//! Split deliberately narrow: the loop owns everything it needs to *act* privately
//! (`crate::executor::ManagedInstance`, the `Governor`) and only publishes what the bus needs to
//! *read* ([`Report`], the trace ring) or *write* ([`Enrichment`], the stage, forget-requests)
//! through [`SharedState`]'s locks. Every lock here is held only for a plain, synchronous read,
//! clone or `fs::write` of a few bytes — never across an `.await` — so a slow D-Bus peer cannot
//! stall the loop, and the loop cannot stall a bus call.

use crate::policy::{PressureLevel, ScoreBreakdown, Stage};
use crate::psi::Psi;
use crate::trace::TraceRing;
use lege_types::{AppId, InstanceId};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

/// Enrichment received over D-Bus: who is on screen and what must not be a victim. Defaults are
/// chosen to be *protective*: an instance the compositor and sessiond have not yet described
/// stays out of the "obviously fine to trim" bucket until they say otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Enrichment {
    pub foreground: bool,
    pub visible: bool,
    pub protected: bool,
    pub disposable: bool,
    pub supports_checkpoint: bool,
    pub has_unsaved_work: bool,
    /// When the unsaved-work claim was made, in milliseconds since the epoch.
    ///
    /// `has_unsaved_work` on its own is a claim with no expiry: an application that says "I have
    /// unsaved work" once is protected by that claim for the rest of the session, whether or not
    /// it is still true — and there is nothing in the protocol that requires it ever to say
    /// otherwise. A timestamp makes the claim something that can go out of date, which is what a
    /// claim of that shape has to be.
    pub unsaved_since_ms: Option<i64>,
    /// Whether the instance has a usable checkpoint to be restored from.
    ///
    /// Distinct from `supports_checkpoint`, which says only that it *could* be asked. An
    /// application that can be restored from a checkpoint taken a minute ago is a very different
    /// proposition to one that can only be killed, and the difference is not a capability but a
    /// fact about right now.
    pub checkpoint_ok: bool,
    /// When that checkpoint was taken, in milliseconds since the epoch.
    pub checkpoint_at_ms: Option<i64>,
    /// When the user last interacted with this instance, in milliseconds since the epoch.
    ///
    /// `None` means nobody has said — which is not the same as "a long time ago" and is not
    /// scored as if it were.
    pub last_interaction_ms: Option<i64>,
    /// The user asked for this instance to be kept.
    pub user_pinned: bool,
    /// The instance belongs to a project the user marked important.
    pub project_important: bool,
}

impl Default for Enrichment {
    fn default() -> Self {
        Self {
            foreground: false,
            // Unknown is treated as "could be visible": the protective default, until the
            // compositor's first `SetInstanceState` call says otherwise.
            visible: true,
            protected: false,
            disposable: false,
            supports_checkpoint: false,
            has_unsaved_work: false,
            unsaved_since_ms: None,
            checkpoint_ok: false,
            checkpoint_at_ms: None,
            last_interaction_ms: None,
            user_pinned: false,
            project_important: false,
        }
    }
}

impl Enrichment {
    /// Record an unsaved-work claim, stamping it the first time it becomes true.
    ///
    /// Stamped on the transition rather than on every call, so a producer that re-asserts the
    /// same claim every second does not keep it permanently fresh — which would be the same
    /// unexpiring claim with extra steps.
    pub fn set_unsaved_work(&mut self, has_unsaved_work: bool, now_ms: i64) {
        if has_unsaved_work {
            if !self.has_unsaved_work {
                self.unsaved_since_ms = Some(now_ms);
            }
        } else {
            self.unsaved_since_ms = None;
        }
        self.has_unsaved_work = has_unsaved_work;
    }

    /// Record an unsaved-work claim that carries its own timestamps.
    ///
    /// The producer said when the work became unsaved, so that time is kept rather than the
    /// moment the message happened to arrive — a claim relayed a minute late is still a claim
    /// about a minute ago.
    pub fn set_work(
        &mut self,
        has_unsaved_work: bool,
        as_of_ms: Option<i64>,
        checkpoint_ok: bool,
        checkpoint_at_ms: Option<i64>,
        now_ms: i64,
    ) {
        self.has_unsaved_work = has_unsaved_work;
        self.unsaved_since_ms = has_unsaved_work.then(|| as_of_ms.unwrap_or(now_ms));
        self.checkpoint_ok = checkpoint_ok;
        self.checkpoint_at_ms = checkpoint_ok.then(|| checkpoint_at_ms.unwrap_or(now_ms));
    }
}

/// One instance's data and score, as of the last tick — the payload of `Status`/`Report`/
/// `ExplainInstance`, and what a trace record's `instances` entries mirror.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InstanceReport {
    pub instance_id: InstanceId,
    pub app_id: AppId,
    pub memory_current_bytes: u64,
    pub memory_growth_bytes_per_second: i64,
    pub full_psi_avg10: f64,
    pub foreground: bool,
    pub visible: bool,
    pub protected: bool,
    pub disposable: bool,
    /// Whether the instance's unsaved-work claim was still current at this tick.
    pub has_unsaved_work: bool,
    /// How old that claim is, in milliseconds — `None` when there is no claim. Reported so
    /// `ExplainInstance` can say "unsaved work reported 4 minutes ago" rather than making a
    /// six-hour-old assertion sound like a fresh one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsaved_work_age_ms: Option<i64>,
    /// The signals that reached scoring alongside the original four. Absent when nobody has
    /// reported them, which is different from zero and is scored as such.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_interaction_age_ms: Option<u64>,
    pub project_important: bool,
    pub supports_checkpoint: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_age_ms: Option<u64>,
    pub user_pinned: bool,
    pub victim_score: f64,
    pub score_breakdown: ScoreBreakdown,
}

/// The latest tick's full snapshot.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub level: PressureLevel,
    pub stage: Stage,
    pub uptime_seconds: u64,
    pub mem_available_bytes: u64,
    pub global_memory_psi: Psi,
    pub global_io_psi: Psi,
    pub instances: Vec<InstanceReport>,
}

/// The answer to `ExplainInstance`: why this instance would (or would not) be picked next, in
/// words and in numbers (planning doc 01 §5's worked example).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Explanation {
    pub instance_id: InstanceId,
    pub known: bool,
    pub level: PressureLevel,
    pub stage: Stage,
    pub protected: Option<bool>,
    pub victim_score: Option<f64>,
    pub score_breakdown: Option<ScoreBreakdown>,
    /// 1-based rank among unprotected candidates, best (most likely victim) first.
    pub rank: Option<usize>,
    pub candidate_count: usize,
    pub would_be_selected: bool,
    pub top_candidate_app_id: Option<AppId>,
    pub summary: String,
}

/// "4 minutes ago" — how old a claim is, in the words a person would use.
///
/// The point of printing it at all is that "this application has unsaved work" and "this
/// application said it had unsaved work six hours ago" are very different statements, and the
/// governor was making the second one while sounding like the first.
fn ago(age_ms: i64) -> String {
    let seconds = age_ms.max(0) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} minute(s) ago", seconds / 60),
        3600..=86_399 => format!("{} hour(s) ago", seconds / 3600),
        _ => format!("{} day(s) ago", seconds / 86_400),
    }
}

impl Report {
    pub fn instance(&self, instance_id: InstanceId) -> Option<&InstanceReport> {
        self.instances
            .iter()
            .find(|instance| instance.instance_id == instance_id)
    }

    /// Unprotected instances ranked best-victim-first.
    fn ranked_candidates(&self) -> Vec<&InstanceReport> {
        let mut candidates: Vec<&InstanceReport> =
            self.instances.iter().filter(|i| !i.protected).collect();
        candidates.sort_by(|left, right| {
            right
                .victim_score
                .partial_cmp(&left.victim_score)
                .unwrap_or(Ordering::Equal)
        });
        candidates
    }

    /// The candidate `decide()` would currently pick: the highest-scoring *unprotected*
    /// instance, if any.
    pub fn top_candidate(&self) -> Option<&InstanceReport> {
        self.ranked_candidates().into_iter().next()
    }

    pub fn explain(&self, instance_id: InstanceId) -> Explanation {
        let ranked = self.ranked_candidates();
        let top_app_id = ranked.first().map(|top| top.app_id.clone());

        let Some(found) = self.instance(instance_id) else {
            return Explanation {
                instance_id,
                known: false,
                level: self.level,
                stage: self.stage,
                protected: None,
                victim_score: None,
                score_breakdown: None,
                rank: None,
                candidate_count: ranked.len(),
                would_be_selected: false,
                top_candidate_app_id: top_app_id,
                summary: "not currently tracked by the governor".to_owned(),
            };
        };

        let rank = ranked
            .iter()
            .position(|candidate| candidate.instance_id == instance_id)
            .map(|index| index + 1);
        let would_be_selected =
            self.level != PressureLevel::Normal && !found.protected && rank == Some(1);

        let summary = if found.protected {
            format!(
                "{} is session-protected and can never be selected as a victim.",
                found.app_id
            )
        } else if self.level == PressureLevel::Normal {
            format!(
                "{} would not be selected: pressure is currently normal.",
                found.app_id
            )
        } else if would_be_selected {
            format!(
                "{} is the current top candidate at {}: score {:.1} (memory {:.1}, growth {:.1}, psi {:.1}).",
                found.app_id,
                self.level,
                found.victim_score,
                found.score_breakdown.memory_term,
                found.score_breakdown.growth_term,
                found.score_breakdown.psi_term,
            )
        } else {
            match ranked.first() {
                Some(top) => format!(
                    "{} would not be selected: {} currently scores higher ({:.1} vs {:.1}).",
                    found.app_id, top.app_id, top.victim_score, found.victim_score,
                ),
                None => format!("{} would not be selected.", found.app_id),
            }
        };

        let summary = match found.unsaved_work_age_ms {
            Some(age) if found.has_unsaved_work => {
                format!("{summary} Unsaved work was reported {}.", ago(age))
            }
            Some(age) => format!(
                "{summary} Its unsaved-work claim, made {}, has expired and no longer protects it.",
                ago(age)
            ),
            None => summary,
        };

        Explanation {
            instance_id,
            known: true,
            level: self.level,
            stage: self.stage,
            protected: Some(found.protected),
            victim_score: Some(found.victim_score),
            score_breakdown: Some(found.score_breakdown),
            rank,
            candidate_count: ranked.len(),
            would_be_selected,
            top_candidate_app_id: top_app_id,
            summary,
        }
    }
}

/// Everything the sampling loop and the D-Bus interface share.
#[derive(Debug)]
pub struct SharedState {
    stage: RwLock<Stage>,
    stage_path: Option<PathBuf>,
    pub enrichment: RwLock<HashMap<InstanceId, Enrichment>>,
    /// Instances `ForgetInstance` has been asked to drop; the loop drains this, restores any
    /// temporary limits and removes its own record on the next tick (see the module doc comment
    /// for why this is not done synchronously inside the D-Bus call).
    pub pending_forgets: Mutex<HashSet<InstanceId>>,
    pub report: RwLock<Option<Report>>,
    pub traces: Mutex<TraceRing>,
}

impl SharedState {
    /// `stage_path`: where the containment stage is persisted and reloaded (`None` disables
    /// persistence — used by tests and by `--once`, which has no business changing it anyway).
    pub fn new(stage_path: Option<PathBuf>, traces: TraceRing) -> Self {
        let stage = stage_path
            .as_deref()
            .and_then(load_stage_file)
            .unwrap_or_default();
        Self {
            stage: RwLock::new(stage),
            stage_path,
            enrichment: RwLock::new(HashMap::new()),
            pending_forgets: Mutex::new(HashSet::new()),
            report: RwLock::new(None),
            traces: Mutex::new(traces),
        }
    }

    /// No persistence, in-memory trace ring: for tests and `--once`.
    pub fn in_memory() -> Self {
        Self::new(None, TraceRing::in_memory(256, 256 * 1024))
    }

    pub fn stage(&self) -> Stage {
        *self.stage.read()
    }

    /// Change the containment stage and best-effort persist it. A failed write is logged, not
    /// propagated: the in-memory stage (what actually governs behaviour) has already changed.
    pub fn set_stage(&self, stage: Stage) {
        *self.stage.write() = stage;
        if let Some(path) = &self.stage_path
            && let Err(error) = persist_stage_file(path, stage)
        {
            tracing::warn!(path = %path.display(), %error, "could not persist containment stage");
        }
    }

    pub fn enrichment_for(&self, instance_id: InstanceId) -> Enrichment {
        self.enrichment
            .read()
            .get(&instance_id)
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Deserialize, Serialize)]
struct StageFile {
    stage: String,
}

fn load_stage_file(path: &Path) -> Option<Stage> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: StageFile = toml::from_str(&text).ok()?;
    Stage::parse(&file.stage)
}

fn persist_stage_file(path: &Path, stage: Stage) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(&StageFile {
        stage: stage.as_str().to_owned(),
    })
    .unwrap_or_default();
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsaved_work_claim_is_stamped_when_it_first_becomes_true() {
        let mut enrichment = Enrichment::default();
        enrichment.set_unsaved_work(true, 1_000);
        assert_eq!(enrichment.unsaved_since_ms, Some(1_000));

        // Re-asserting the same claim must not refresh it, or a producer that repeats itself
        // every second would keep an unexpiring claim by another route.
        enrichment.set_unsaved_work(true, 500_000);
        assert_eq!(enrichment.unsaved_since_ms, Some(1_000));

        enrichment.set_unsaved_work(false, 600_000);
        assert_eq!(enrichment.unsaved_since_ms, None);
        assert!(!enrichment.has_unsaved_work);

        enrichment.set_unsaved_work(true, 700_000);
        assert_eq!(enrichment.unsaved_since_ms, Some(700_000));
    }

    #[test]
    fn a_timestamped_claim_keeps_the_time_the_producer_gave_it() {
        // A claim relayed a minute late is still a claim about a minute ago; taking the arrival
        // time instead would quietly refresh every claim that took a while to get here.
        let mut enrichment = Enrichment::default();
        enrichment.set_work(true, Some(1_000), true, Some(900), 60_000);
        assert_eq!(enrichment.unsaved_since_ms, Some(1_000));
        assert!(enrichment.checkpoint_ok);
        assert_eq!(enrichment.checkpoint_at_ms, Some(900));
    }

    #[test]
    fn a_timestamped_claim_can_say_the_work_is_saved() {
        // The thing SetInstanceHints cannot do, and the reason the new method exists.
        let mut enrichment = Enrichment::default();
        enrichment.set_unsaved_work(true, 1_000);
        enrichment.set_work(false, None, true, None, 60_000);
        assert!(!enrichment.has_unsaved_work);
        assert_eq!(enrichment.unsaved_since_ms, None);
    }

    #[test]
    fn a_claim_with_no_time_of_its_own_is_stamped_on_arrival() {
        let mut enrichment = Enrichment::default();
        enrichment.set_work(true, None, false, None, 60_000);
        assert_eq!(enrichment.unsaved_since_ms, Some(60_000));
        assert_eq!(enrichment.checkpoint_at_ms, None, "no checkpoint, no time");
    }

    #[test]
    fn explain_says_how_old_an_unsaved_work_claim_is() {
        // "This application has unsaved work" and "it said so six hours ago" are very different
        // statements, and the governor was making the second while sounding like the first.
        let mut fresh = instance(5.0, false);
        fresh.has_unsaved_work = true;
        fresh.unsaved_work_age_ms = Some(4 * 60 * 1000);
        let snapshot = report(vec![fresh.clone()], PressureLevel::Severe);
        let explanation = snapshot.explain(fresh.instance_id);
        assert!(
            explanation.summary.contains("reported 4 minute(s) ago"),
            "{}",
            explanation.summary
        );
    }

    #[test]
    fn explain_says_when_a_claim_has_expired() {
        let mut stale = instance(5.0, false);
        stale.has_unsaved_work = false;
        stale.unsaved_work_age_ms = Some(6 * 60 * 60 * 1000);
        let snapshot = report(vec![stale.clone()], PressureLevel::Severe);
        let explanation = snapshot.explain(stale.instance_id);
        assert!(
            explanation.summary.contains("has expired"),
            "{}",
            explanation.summary
        );
        assert!(
            explanation.summary.contains("6 hour(s) ago"),
            "{}",
            explanation.summary
        );
    }

    #[test]
    fn an_age_is_printed_in_the_units_a_person_would_use() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59_000), "just now");
        assert_eq!(ago(60_000), "1 minute(s) ago");
        assert_eq!(ago(3_600_000), "1 hour(s) ago");
        assert_eq!(ago(90_000_000), "1 day(s) ago");
        assert_eq!(
            ago(-5),
            "just now",
            "a clock that went backwards is not the future"
        );
    }

    fn instance(score: f64, protected: bool) -> InstanceReport {
        InstanceReport {
            instance_id: InstanceId::new(),
            app_id: "org.legeos.Test".parse().unwrap(),
            memory_current_bytes: 0,
            memory_growth_bytes_per_second: 0,
            full_psi_avg10: 0.0,
            foreground: false,
            visible: false,
            protected,
            disposable: false,
            has_unsaved_work: false,
            unsaved_work_age_ms: None,
            last_interaction_age_ms: None,
            project_important: false,
            supports_checkpoint: false,
            checkpoint_age_ms: None,
            user_pinned: false,
            victim_score: score,
            score_breakdown: ScoreBreakdown::default(),
        }
    }

    fn report(instances: Vec<InstanceReport>, level: PressureLevel) -> Report {
        Report {
            level,
            stage: Stage::ObserveOnly,
            uptime_seconds: 0,
            mem_available_bytes: 0,
            global_memory_psi: Psi::default(),
            global_io_psi: Psi::default(),
            instances,
        }
    }

    #[test]
    fn top_candidate_ignores_protected_instances() {
        let low = instance(1.0, false);
        let high_protected = instance(99.0, true);
        let snapshot = report(vec![low.clone(), high_protected], PressureLevel::Severe);
        assert_eq!(
            snapshot.top_candidate().unwrap().instance_id,
            low.instance_id
        );
    }

    #[test]
    fn explain_unknown_instance() {
        let snapshot = report(vec![instance(5.0, false)], PressureLevel::Severe);
        let explanation = snapshot.explain(InstanceId::new());
        assert!(!explanation.known);
        assert!(!explanation.would_be_selected);
    }

    #[test]
    fn explain_top_candidate_says_it_would_be_selected() {
        let winner = instance(50.0, false);
        let loser = instance(5.0, false);
        let snapshot = report(vec![winner.clone(), loser], PressureLevel::Critical);
        let explanation = snapshot.explain(winner.instance_id);
        assert!(explanation.known);
        assert!(explanation.would_be_selected);
        assert_eq!(explanation.rank, Some(1));
        assert!(explanation.summary.contains("top candidate"));
    }

    #[test]
    fn explain_runner_up_says_who_scores_higher() {
        let winner = instance(50.0, false);
        let loser = instance(5.0, false);
        let snapshot = report(vec![winner, loser.clone()], PressureLevel::Critical);
        let explanation = snapshot.explain(loser.instance_id);
        assert!(!explanation.would_be_selected);
        assert_eq!(explanation.rank, Some(2));
        assert!(explanation.summary.contains("scores higher"));
    }

    #[test]
    fn explain_protected_instance_is_never_selectable() {
        let protected = instance(1000.0, true);
        let snapshot = report(vec![protected.clone()], PressureLevel::Critical);
        let explanation = snapshot.explain(protected.instance_id);
        assert!(!explanation.would_be_selected);
        assert!(explanation.summary.contains("session-protected"));
    }

    #[test]
    fn explain_at_normal_pressure_never_selects_anyone() {
        let winner = instance(50.0, false);
        let snapshot = report(vec![winner.clone()], PressureLevel::Normal);
        let explanation = snapshot.explain(winner.instance_id);
        assert!(!explanation.would_be_selected);
        assert!(explanation.summary.contains("pressure is currently normal"));
    }

    #[test]
    fn enrichment_default_is_protective_on_visibility() {
        let enrichment = Enrichment::default();
        assert!(enrichment.visible);
        assert!(!enrichment.foreground);
        assert!(!enrichment.protected);
    }

    #[test]
    fn shared_state_enrichment_for_falls_back_to_default() {
        let shared = SharedState::in_memory();
        let id = InstanceId::new();
        assert_eq!(shared.enrichment_for(id), Enrichment::default());
        shared.enrichment.write().insert(
            id,
            Enrichment {
                foreground: true,
                ..Enrichment::default()
            },
        );
        assert!(shared.enrichment_for(id).foreground);
    }

    #[test]
    fn stage_persists_across_a_reload() {
        let dir = std::env::temp_dir().join(format!("lege-resourced-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("stage.toml");

        let shared = SharedState::new(Some(path.clone()), TraceRing::in_memory(4, 64 * 1024));
        assert_eq!(shared.stage(), Stage::ObserveOnly);
        shared.set_stage(Stage::Throttle);
        assert_eq!(shared.stage(), Stage::Throttle);

        let reloaded = SharedState::new(Some(path), TraceRing::in_memory(4, 64 * 1024));
        assert_eq!(reloaded.stage(), Stage::Throttle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_stage_file_defaults_to_observe_only() {
        let path = std::env::temp_dir().join(format!(
            "lege-resourced-stage-missing-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let shared = SharedState::new(Some(path), TraceRing::in_memory(4, 64 * 1024));
        assert_eq!(shared.stage(), Stage::ObserveOnly);
    }
}
