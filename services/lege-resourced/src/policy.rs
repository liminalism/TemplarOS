//! Pure policy engine: no I/O, fully unit-testable.
//!
//! The constants are starting values for trace collection on real systems, not final policy.
//! A browser with 12 GiB of useful tabs may be less disposable than a background image
//! conversion using 5 GiB; memory size alone must never select the victim. [`GovernorConfig`]
//! (including [`VictimWeights`]) is loadable from a file (`crate::config`) precisely so these
//! numbers can be replaced once recorded traces (`crate::trace`) show what they should be.

use lege_types::InstanceId;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fmt,
    time::{Duration, Instant},
};

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// Global pressure state (planning doc 01 §5 state machine, collapsed to four levels for v1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PressureLevel {
    Normal,
    Elevated,
    Severe,
    Critical,
}

impl PressureLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Elevated => "elevated",
            Self::Severe => "severe",
            Self::Critical => "critical",
        }
    }
}

impl fmt::Display for PressureLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The containment ladder (planning doc 02 §8, ADR-0002): `observe-only` records and explains
/// but performs nothing; each later stage additionally performs everything the stages below it
/// decide. Persisted across restarts (`crate::daemon`) and defaults to `ObserveOnly`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    #[default]
    ObserveOnly,
    Trim,
    Throttle,
    Checkpoint,
    Terminate,
}

impl Stage {
    pub const ALL: [Self; 5] = [
        Self::ObserveOnly,
        Self::Trim,
        Self::Throttle,
        Self::Checkpoint,
        Self::Terminate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ObserveOnly => "observe-only",
            Self::Trim => "trim",
            Self::Throttle => "throttle",
            Self::Checkpoint => "checkpoint",
            Self::Terminate => "terminate",
        }
    }

    /// Parse the D-Bus/config-file spelling (`SetStage`, `resourced.toml`, `stage.toml`).
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.as_str() == text)
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One application instance as seen by the governor on this tick.
#[derive(Clone, Debug, PartialEq)]
pub struct AppSample {
    pub instance_id: InstanceId,
    pub memory_current: u64,
    pub memory_growth_per_second: i64,
    pub full_psi_avg10: f64,

    pub foreground: bool,
    pub visible: bool,
    /// Session-critical or user-protected: never a victim.
    pub protected: bool,
    pub reports_unsaved_state: bool,
    pub disposable: bool,
}

/// What the governor wants done. The executor applies it (or only logs it, at a stage that does
/// not yet permit it).
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    TrimCaches {
        instance_id: InstanceId,
    },
    SetMemoryHigh {
        instance_id: InstanceId,
        bytes: u64,
    },
    Reclaim {
        instance_id: InstanceId,
        bytes: u64,
    },
    RequestCheckpoint {
        instance_id: InstanceId,
        deadline: Duration,
    },
    KillCgroup {
        instance_id: InstanceId,
        reason: String,
    },
    RestoreDynamicLimits {
        instance_id: InstanceId,
    },
}

impl Action {
    /// The lowest [`Stage`] at which this action is actually performed rather than only decided
    /// and signalled with `applied=false`.
    ///
    /// `RestoreDynamicLimits` is allowed at every stage (including `observe-only`): undoing a
    /// temporary override is never itself a risk, and it must still run when the stage is
    /// lowered while an override is outstanding.
    pub fn minimum_stage(&self) -> Stage {
        match self {
            Self::RestoreDynamicLimits { .. } => Stage::ObserveOnly,
            Self::TrimCaches { .. } => Stage::Trim,
            Self::SetMemoryHigh { .. } | Self::Reclaim { .. } => Stage::Throttle,
            Self::RequestCheckpoint { .. } => Stage::Checkpoint,
            Self::KillCgroup { .. } => Stage::Terminate,
        }
    }

    /// The `kind` string used on the wire (`org.legeos.Resources1.PressureAction`, trace records).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TrimCaches { .. } => "trim-caches",
            Self::SetMemoryHigh { .. } => "set-memory-high",
            Self::Reclaim { .. } => "reclaim",
            Self::RequestCheckpoint { .. } => "request-checkpoint",
            Self::KillCgroup { .. } => "kill-cgroup",
            Self::RestoreDynamicLimits { .. } => "restore-limits",
        }
    }

    pub fn instance_id(&self) -> InstanceId {
        match self {
            Self::TrimCaches { instance_id }
            | Self::SetMemoryHigh { instance_id, .. }
            | Self::Reclaim { instance_id, .. }
            | Self::RequestCheckpoint { instance_id, .. }
            | Self::KillCgroup { instance_id, .. }
            | Self::RestoreDynamicLimits { instance_id } => *instance_id,
        }
    }
}

/// Weights for [`victim_score_breakdown`]. Starting values only — see the module doc comment.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VictimWeights {
    /// Per GiB of `memory_current`.
    pub memory_gib: f64,
    /// Per GiB/s of positive `memory_growth_per_second`.
    pub growth_gib_per_second: f64,
    /// Per point of `full_psi_avg10`.
    pub full_psi_avg10: f64,
    /// Flat bonus when `disposable`.
    pub disposable_bonus: f64,
    /// Flat term (expected negative — protective) when `visible`.
    pub visible_protection: f64,
    /// Flat term (expected negative — protective) when `foreground`.
    pub foreground_protection: f64,
    /// Flat term (expected negative — protective) when `reports_unsaved_state`.
    pub unsaved_protection: f64,
}

impl Default for VictimWeights {
    fn default() -> Self {
        Self {
            memory_gib: 8.0,
            growth_gib_per_second: 30.0,
            full_psi_avg10: 4.0,
            disposable_bonus: 60.0,
            visible_protection: -20.0,
            foreground_protection: -80.0,
            unsaved_protection: -60.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GovernorConfig {
    pub reserve_bytes: u64,
    pub critical_reserve_bytes: u64,

    pub elevated_some_avg10: f64,
    pub severe_full_avg10: f64,
    pub critical_full_avg10: f64,

    pub emergency_kill_delay: Duration,
    pub minimum_high_limit: u64,

    /// Consecutive ticks a *lower* raw level must persist before the effective level actually
    /// drops (hysteresis). Escalation is always immediate — only relaxation is debounced.
    pub hysteresis_downgrade_ticks: u32,

    /// How long an unsaved-work claim counts for before it stops protecting an instance.
    ///
    /// The claim is a strong protective term — a whole application excused from containment —
    /// with, until now, no expiry: whoever said it once was protected for the rest of the
    /// session whether or not it was still true, and nothing in the protocol requires anyone to
    /// take it back. Ten minutes is a safety net, not the fix: the fix is a producer that
    /// reports the transition both ways, and this is what limits the damage when one does not.
    pub unsaved_ttl: Duration,

    pub victim_weights: VictimWeights,
}

/// The machine this project's thresholds were calibrated on.
///
/// [`GovernorConfig::default`]'s literal 3 GiB and 1 GiB come from a 48 GiB workstation, and were
/// only ever right for one. [`GovernorConfig::for_total_memory`] is what production uses; this
/// constant is what ties the two together, in a test, so the scaled form cannot quietly drift
/// away from the calibration it was derived from.
pub const REFERENCE_MEMORY: u64 = 48 * GIB;

/// Fraction of `MemTotal` held back before pressure counts as severe.
///
/// 1/16. On the 48 GiB reference machine that is exactly the 3 GiB the constant always said; on a
/// 4 GiB laptop it is 256 MiB, floored up to 768 MiB, which is the point — a fixed 3 GiB reserve
/// on a 4 GiB machine declares severe pressure at the login screen and never stops.
const RESERVE_FRACTION: f64 = 0.0625;
/// Fraction of `MemTotal` below which pressure is critical.
const CRITICAL_FRACTION: f64 = 0.02;

/// Floors and ceilings for the scaled reserves.
///
/// The floors are what a desktop needs to survive at all regardless of how little memory it has:
/// below roughly 768 MiB free, opening a window is already failing. The ceilings stop a very large
/// machine from holding back an absurd amount it will never need — 6.25% of 256 GiB is 16 GiB of
/// memory nobody would ever get to use.
const RESERVE_FLOOR: u64 = 768 * MIB;
const RESERVE_CEILING: u64 = 4 * GIB;
const CRITICAL_FLOOR: u64 = 256 * MIB;
const CRITICAL_CEILING: u64 = 3 * GIB / 2;

impl GovernorConfig {
    /// Defaults scaled to this machine's `MemTotal`.
    ///
    /// The shipped thresholds were two literals — 3 GiB and 1 GiB — with `MemTotal` read nowhere
    /// in the crate. On the machine they were measured on that is right; on a 4 GiB laptop a 3 GiB
    /// reserve means the governor believes the system is under severe memory pressure from the
    /// moment it logs in, forever, and on a 256 GiB workstation it means it never notices anything
    /// at all.
    #[must_use]
    pub fn for_total_memory(mem_total: u64) -> Self {
        Self {
            reserve_bytes: scale(mem_total, RESERVE_FRACTION, RESERVE_FLOOR, RESERVE_CEILING),
            critical_reserve_bytes: scale(
                mem_total,
                CRITICAL_FRACTION,
                CRITICAL_FLOOR,
                CRITICAL_CEILING,
            ),
            ..Self::default()
        }
    }
}

/// `fraction` of `mem_total`, held between `floor` and `ceiling`.
fn scale(mem_total: u64, fraction: f64, floor: u64, ceiling: u64) -> u64 {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let scaled = (mem_total as f64 * fraction) as u64;
    scaled.clamp(floor, ceiling)
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            reserve_bytes: 3 * GIB,
            critical_reserve_bytes: GIB,
            // Starting values only. Calibrate from real traces.
            elevated_some_avg10: 3.0,
            severe_full_avg10: 0.5,
            critical_full_avg10: 2.0,
            emergency_kill_delay: Duration::from_secs(3),
            minimum_high_limit: 512 * MIB,
            hysteresis_downgrade_ticks: 3,
            unsaved_ttl: Duration::from_secs(10 * 60),
            victim_weights: VictimWeights::default(),
        }
    }
}

/// The pressure state machine.
#[derive(Debug)]
pub struct Governor {
    config: GovernorConfig,
    critical_since: Option<Instant>,
    current_level: PressureLevel,
    /// A raw level below `current_level` seen for `pending.1` consecutive ticks so far.
    pending: Option<(PressureLevel, u32)>,
}

impl Governor {
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            config,
            critical_since: None,
            current_level: PressureLevel::Normal,
            pending: None,
        }
    }

    pub fn config(&self) -> &GovernorConfig {
        &self.config
    }

    /// The hysteresis-debounced level as of the last [`Governor::tick_level`] call.
    pub fn current_level(&self) -> PressureLevel {
        self.current_level
    }

    /// Classify *raw* global state from `MemAvailable` and global memory PSI, with no hysteresis.
    /// Exposed for direct testing of the thresholds; `tick_level` is what the daemon calls.
    pub fn classify(
        &self,
        available_bytes: u64,
        global_some_avg10: f64,
        global_full_avg10: f64,
    ) -> PressureLevel {
        if available_bytes <= self.config.critical_reserve_bytes
            || global_full_avg10 >= self.config.critical_full_avg10
        {
            PressureLevel::Critical
        } else if available_bytes <= self.config.reserve_bytes
            || global_full_avg10 >= self.config.severe_full_avg10
        {
            PressureLevel::Severe
        } else if global_some_avg10 >= self.config.elevated_some_avg10 {
            PressureLevel::Elevated
        } else {
            PressureLevel::Normal
        }
    }

    /// Classify and apply hysteresis: escalation is immediate, de-escalation requires
    /// `hysteresis_downgrade_ticks` consecutive ticks at (or below) the lower level first, so a
    /// single good sample does not flap the level back down.
    pub fn tick_level(
        &mut self,
        available_bytes: u64,
        global_some_avg10: f64,
        global_full_avg10: f64,
    ) -> PressureLevel {
        let raw = self.classify(available_bytes, global_some_avg10, global_full_avg10);
        match raw.cmp(&self.current_level) {
            Ordering::Greater => {
                self.current_level = raw;
                self.pending = None;
            }
            Ordering::Less => {
                let streak = match self.pending {
                    Some((level, streak)) if level == raw => streak + 1,
                    _ => 1,
                };
                if streak >= self.config.hysteresis_downgrade_ticks.max(1) {
                    self.current_level = raw;
                    self.pending = None;
                } else {
                    self.pending = Some((raw, streak));
                }
            }
            Ordering::Equal => self.pending = None,
        }
        self.current_level
    }

    /// Decide what to do this tick.
    pub fn decide(
        &mut self,
        now: Instant,
        level: PressureLevel,
        applications: &[AppSample],
    ) -> Vec<Action> {
        if level != PressureLevel::Critical {
            self.critical_since = None;
        }

        if level == PressureLevel::Normal {
            return applications
                .iter()
                .map(|app| Action::RestoreDynamicLimits {
                    instance_id: app.instance_id,
                })
                .collect();
        }

        let weights = &self.config.victim_weights;
        let Some(target) =
            applications
                .iter()
                .filter(|app| !app.protected)
                .max_by(|left, right| {
                    victim_score(left, weights)
                        .partial_cmp(&victim_score(right, weights))
                        .unwrap_or(Ordering::Equal)
                })
        else {
            return Vec::new();
        };

        match level {
            PressureLevel::Normal => unreachable!("handled above"),

            PressureLevel::Elevated => {
                if target.foreground {
                    Vec::new()
                } else {
                    vec![Action::TrimCaches {
                        instance_id: target.instance_id,
                    }]
                }
            }

            PressureLevel::Severe => {
                let high = target
                    .memory_current
                    .saturating_mul(9)
                    .saturating_div(10)
                    .max(self.config.minimum_high_limit);
                let reclaim = (target.memory_current / 8).min(256 * MIB);
                vec![
                    Action::TrimCaches {
                        instance_id: target.instance_id,
                    },
                    Action::SetMemoryHigh {
                        instance_id: target.instance_id,
                        bytes: high,
                    },
                    Action::Reclaim {
                        instance_id: target.instance_id,
                        bytes: reclaim,
                    },
                ]
            }

            PressureLevel::Critical => {
                let started = self.critical_since.get_or_insert(now);
                let elapsed = now.saturating_duration_since(*started);
                let high = target
                    .memory_current
                    .saturating_mul(8)
                    .saturating_div(10)
                    .max(self.config.minimum_high_limit);

                let mut actions = vec![
                    Action::SetMemoryHigh {
                        instance_id: target.instance_id,
                        bytes: high,
                    },
                    Action::RequestCheckpoint {
                        instance_id: target.instance_id,
                        deadline: self.config.emergency_kill_delay,
                    },
                ];

                if elapsed >= self.config.emergency_kill_delay {
                    actions.push(Action::KillCgroup {
                        instance_id: target.instance_id,
                        reason: format!(
                            "persistent critical memory pressure; usage={} MiB, full PSI avg10={:.2}",
                            target.memory_current / MIB,
                            target.full_psi_avg10,
                        ),
                    });
                }

                actions
            }
        }
    }
}

/// Whether an unsaved-work claim made at `since_ms` still counts at `now_ms`.
///
/// `None` — a claim with no timestamp at all — counts. That is a producer this build has not
/// taught to stamp its claims, and treating its report as already expired would silently remove
/// the protection rather than bound it.
///
/// A claim from the *future* counts too. Clocks move backwards (an NTP step, a laptop resuming
/// with a dead RTC), and the reading of "the claim is 4 hours in the future, therefore expired"
/// would drop protection from every application at once, at exactly the moment the machine is
/// least well understood.
#[must_use]
pub fn unsaved_claim_is_current(since_ms: Option<i64>, now_ms: i64, ttl: Duration) -> bool {
    let Some(since_ms) = since_ms else {
        return true;
    };
    let age_ms = now_ms.saturating_sub(since_ms);
    if age_ms < 0 {
        return true;
    }
    u128::try_from(age_ms).unwrap_or(0) <= ttl.as_millis()
}

/// Decomposition of [`victim_score`] into its weighted terms, so `ExplainInstance` and `lege res`
/// can say *why* rather than just *how much* (planning doc 01 §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub memory_term: f64,
    pub growth_term: f64,
    pub psi_term: f64,
    pub disposable_term: f64,
    pub visible_term: f64,
    pub foreground_term: f64,
    pub unsaved_term: f64,
    pub total: f64,
}

/// Higher = better victim. Never memory size alone: growth, PSI contribution, foreground/visible
/// state, unsaved work and disposability all move the score: see [`VictimWeights`].
///
/// This computes the score as if `app` *were* eligible, even when `app.protected` is set: a
/// protected instance's exclusion is enforced once, structurally, by [`Governor::decide`]
/// filtering it out of the candidate pool before scoring — never by an out-of-band sentinel value
/// here (a `NEG_INFINITY` "protected" score cannot round-trip through JSON, and this value is
/// what trace records and `ExplainInstance` report). Callers that need "is this excluded"
/// read `app.protected` directly, alongside the real score.
pub fn victim_score_breakdown(app: &AppSample, weights: &VictimWeights) -> ScoreBreakdown {
    let memory_gib = app.memory_current as f64 / GIB as f64;
    let growth_gib = app.memory_growth_per_second.max(0) as f64 / GIB as f64;

    let memory_term = memory_gib * weights.memory_gib;
    let growth_term = growth_gib * weights.growth_gib_per_second;
    let psi_term = app.full_psi_avg10 * weights.full_psi_avg10;
    let disposable_term = if app.disposable {
        weights.disposable_bonus
    } else {
        0.0
    };
    let visible_term = if app.visible {
        weights.visible_protection
    } else {
        0.0
    };
    let foreground_term = if app.foreground {
        weights.foreground_protection
    } else {
        0.0
    };
    let unsaved_term = if app.reports_unsaved_state {
        weights.unsaved_protection
    } else {
        0.0
    };

    let total = memory_term
        + growth_term
        + psi_term
        + disposable_term
        + visible_term
        + foreground_term
        + unsaved_term;

    ScoreBreakdown {
        memory_term,
        growth_term,
        psi_term,
        disposable_term,
        visible_term,
        foreground_term,
        unsaved_term,
        total,
    }
}

pub fn victim_score(app: &AppSample, weights: &VictimWeights) -> f64 {
    victim_score_breakdown(app, weights).total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsaved_work_claim_stops_counting_after_the_ttl() {
        let ttl = Duration::from_secs(600);
        assert!(unsaved_claim_is_current(Some(0), 599_000, ttl));
        assert!(unsaved_claim_is_current(Some(0), 600_000, ttl));
        assert!(!unsaved_claim_is_current(Some(0), 600_001, ttl));
    }

    #[test]
    fn an_unstamped_claim_still_counts() {
        // A producer this build has not taught to stamp its claims. Treating its report as
        // already expired would silently remove protection rather than bound it.
        assert!(unsaved_claim_is_current(
            None,
            10_000_000,
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn a_claim_from_the_future_counts_rather_than_expiring_everything_at_once() {
        // Clocks move backwards — an NTP step, a laptop resuming with a dead RTC. Reading "this
        // claim is four hours in the future, therefore expired" would drop protection from every
        // application simultaneously, at the moment the machine is least well understood.
        assert!(unsaved_claim_is_current(
            Some(4 * 60 * 60 * 1000),
            0,
            Duration::from_secs(600)
        ));
    }

    #[test]
    fn the_default_ttl_is_ten_minutes() {
        assert_eq!(
            GovernorConfig::default().unsaved_ttl,
            Duration::from_secs(600)
        );
    }

    #[test]
    fn the_reference_machine_reproduces_the_calibrated_reserve_exactly() {
        // The literals in `default()` came off a 48 GiB workstation. If the scaled form ever
        // stops agreeing with them there, the scaling has drifted away from the only calibration
        // this project actually has.
        let scaled = GovernorConfig::for_total_memory(REFERENCE_MEMORY);
        assert_eq!(
            scaled.reserve_bytes,
            GovernorConfig::default().reserve_bytes,
            "the reserve on the reference machine is the calibrated one"
        );
        // The critical reserve is a round 2% rather than the 1/48 that would land on exactly
        // 1 GiB. 983 MiB against 1024 MiB is within 4%, and a round fraction is worth more than
        // four significant figures back-fitted to one machine.
        let critical = GovernorConfig::default().critical_reserve_bytes;
        let difference = critical.abs_diff(scaled.critical_reserve_bytes);
        assert!(
            difference * 100 / critical <= 5,
            "{} vs {critical}",
            scaled.critical_reserve_bytes
        );
    }

    #[test]
    fn a_small_laptop_is_not_told_it_is_permanently_out_of_memory() {
        // The bug the scaling fixes. A fixed 3 GiB reserve on a 4 GiB machine means MemAvailable
        // is below it from the login screen onward, so the governor declares severe pressure
        // forever and every later judgement is made from a false premise.
        let laptop = GovernorConfig::for_total_memory(4 * GIB);
        assert_eq!(laptop.reserve_bytes, 768 * MIB, "the floor applies");
        assert!(laptop.reserve_bytes < 4 * GIB / 2);

        let governor = Governor::new(laptop);
        assert_eq!(
            governor.classify(2 * GIB, 0.0, 0.0),
            PressureLevel::Normal,
            "half of a 4 GiB machine free is not severe pressure"
        );
        assert_eq!(
            Governor::new(GovernorConfig::default()).classify(2 * GIB, 0.0, 0.0),
            PressureLevel::Severe,
            "which is exactly what the unscaled thresholds used to say"
        );
    }

    #[test]
    fn a_very_large_machine_does_not_hold_back_an_absurd_amount() {
        let workstation = GovernorConfig::for_total_memory(256 * GIB);
        assert_eq!(workstation.reserve_bytes, 4 * GIB, "the ceiling applies");
        assert_eq!(workstation.critical_reserve_bytes, 3 * GIB / 2);
    }

    #[test]
    fn the_scaled_thresholds_stay_ordered_at_every_size() {
        // Critical must always be the tighter of the two, or the state machine can reach Critical
        // without ever having been Severe.
        for gib in [1_u64, 2, 4, 8, 16, 32, 48, 64, 128, 256, 1024] {
            let config = GovernorConfig::for_total_memory(gib * GIB);
            assert!(
                config.critical_reserve_bytes < config.reserve_bytes,
                "{gib} GiB: critical {} is not below reserve {}",
                config.critical_reserve_bytes,
                config.reserve_bytes
            );
        }
    }

    #[test]
    fn the_scaled_reserve_never_decreases_as_memory_grows() {
        let mut previous = 0;
        for gib in 1..=512_u64 {
            let reserve = GovernorConfig::for_total_memory(gib * GIB).reserve_bytes;
            assert!(reserve >= previous, "{gib} GiB lowers the reserve");
            previous = reserve;
        }
    }

    fn sample(memory_gib: u64) -> AppSample {
        AppSample {
            instance_id: InstanceId::new(),
            memory_current: memory_gib * GIB,
            memory_growth_per_second: 0,
            full_psi_avg10: 0.0,
            foreground: false,
            visible: false,
            protected: false,
            reports_unsaved_state: false,
            disposable: false,
        }
    }

    #[test]
    fn classification_thresholds() {
        let governor = Governor::new(GovernorConfig::default());
        assert_eq!(governor.classify(16 * GIB, 0.0, 0.0), PressureLevel::Normal);
        assert_eq!(
            governor.classify(16 * GIB, 5.0, 0.0),
            PressureLevel::Elevated
        );
        assert_eq!(governor.classify(2 * GIB, 0.0, 0.0), PressureLevel::Severe);
        assert_eq!(governor.classify(16 * GIB, 0.0, 0.7), PressureLevel::Severe);
        assert_eq!(
            governor.classify(512 * MIB, 0.0, 0.0),
            PressureLevel::Critical
        );
        assert_eq!(
            governor.classify(16 * GIB, 0.0, 3.0),
            PressureLevel::Critical
        );
    }

    #[test]
    fn protected_and_foreground_apps_are_not_victims() {
        let mut governor = Governor::new(GovernorConfig::default());
        let mut critical_daemon = sample(20);
        critical_daemon.protected = true;
        let mut foreground_editor = sample(6);
        foreground_editor.foreground = true;
        foreground_editor.visible = true;
        foreground_editor.reports_unsaved_state = true;
        let mut background_hog = sample(4);
        background_hog.memory_growth_per_second = (200 * MIB) as i64;

        let apps = [
            critical_daemon,
            foreground_editor.clone(),
            background_hog.clone(),
        ];
        let actions = governor.decide(Instant::now(), PressureLevel::Severe, &apps);
        assert!(actions.iter().all(|action| match action {
            Action::TrimCaches { instance_id }
            | Action::SetMemoryHigh { instance_id, .. }
            | Action::Reclaim { instance_id, .. } => *instance_id == background_hog.instance_id,
            other => panic!("unexpected {other:?}"),
        }));
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn critical_pressure_kills_only_after_the_delay() {
        let mut governor = Governor::new(GovernorConfig {
            emergency_kill_delay: Duration::from_secs(3),
            ..Default::default()
        });
        let apps = [sample(10)];
        let start = Instant::now();

        let first = governor.decide(start, PressureLevel::Critical, &apps);
        assert!(!first.iter().any(|a| matches!(a, Action::KillCgroup { .. })));
        assert!(
            first
                .iter()
                .any(|a| matches!(a, Action::RequestCheckpoint { .. }))
        );

        let later = governor.decide(
            start + Duration::from_secs(4),
            PressureLevel::Critical,
            &apps,
        );
        assert!(later.iter().any(|a| matches!(a, Action::KillCgroup { .. })));

        // Leaving Critical resets the timer.
        governor.decide(start + Duration::from_secs(5), PressureLevel::Normal, &apps);
        let again = governor.decide(
            start + Duration::from_secs(6),
            PressureLevel::Critical,
            &apps,
        );
        assert!(!again.iter().any(|a| matches!(a, Action::KillCgroup { .. })));
    }

    #[test]
    fn normal_restores_limits_and_no_unprotected_means_no_actions() {
        let mut governor = Governor::new(GovernorConfig::default());
        let apps = [sample(1), sample(2)];
        let actions = governor.decide(Instant::now(), PressureLevel::Normal, &apps);
        assert_eq!(actions.len(), 2);
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, Action::RestoreDynamicLimits { .. }))
        );

        let mut protected = sample(9);
        protected.protected = true;
        assert!(
            governor
                .decide(Instant::now(), PressureLevel::Critical, &[protected])
                .is_empty()
        );
    }

    #[test]
    fn disposable_beats_bigger_visible_app() {
        let weights = VictimWeights::default();
        let mut big_visible = sample(8);
        big_visible.visible = true;
        let mut small_disposable = sample(1);
        small_disposable.disposable = true;
        assert!(victim_score(&small_disposable, &weights) > victim_score(&big_visible, &weights));
    }

    #[test]
    fn score_breakdown_terms_sum_to_total() {
        let weights = VictimWeights::default();
        let mut app = sample(4);
        app.memory_growth_per_second = (100 * MIB) as i64;
        app.full_psi_avg10 = 2.0;
        app.disposable = true;
        app.visible = true;
        let breakdown = victim_score_breakdown(&app, &weights);
        let sum = breakdown.memory_term
            + breakdown.growth_term
            + breakdown.psi_term
            + breakdown.disposable_term
            + breakdown.visible_term
            + breakdown.foreground_term
            + breakdown.unsaved_term;
        assert!((sum - breakdown.total).abs() < f64::EPSILON);
        assert!(breakdown.visible_term < 0.0);
        assert!(breakdown.disposable_term > 0.0);
    }

    #[test]
    fn protected_is_excluded_by_decide_not_by_an_infinite_score() {
        // The score itself stays finite and JSON-serializable even when protected: exclusion is
        // `Governor::decide`'s job (see `protected_and_foreground_apps_are_not_victims` above),
        // not an out-of-band sentinel in the score.
        let weights = VictimWeights::default();
        let mut app = sample(20);
        app.protected = true;
        let score = victim_score_breakdown(&app, &weights).total;
        assert!(score.is_finite());
        assert!(serde_json::to_string(&victim_score_breakdown(&app, &weights)).is_ok());
    }

    #[test]
    fn action_minimum_stage_matches_the_ladder() {
        let instance_id = InstanceId::new();
        assert_eq!(
            Action::RestoreDynamicLimits { instance_id }.minimum_stage(),
            Stage::ObserveOnly
        );
        assert_eq!(
            Action::TrimCaches { instance_id }.minimum_stage(),
            Stage::Trim
        );
        assert_eq!(
            Action::SetMemoryHigh {
                instance_id,
                bytes: 0
            }
            .minimum_stage(),
            Stage::Throttle
        );
        assert_eq!(
            Action::Reclaim {
                instance_id,
                bytes: 0
            }
            .minimum_stage(),
            Stage::Throttle
        );
        assert_eq!(
            Action::RequestCheckpoint {
                instance_id,
                deadline: Duration::ZERO
            }
            .minimum_stage(),
            Stage::Checkpoint
        );
        assert_eq!(
            Action::KillCgroup {
                instance_id,
                reason: String::new()
            }
            .minimum_stage(),
            Stage::Terminate
        );
    }

    #[test]
    fn stage_ordering_and_round_trip() {
        assert!(Stage::ObserveOnly < Stage::Trim);
        assert!(Stage::Trim < Stage::Throttle);
        assert!(Stage::Throttle < Stage::Checkpoint);
        assert!(Stage::Checkpoint < Stage::Terminate);
        for stage in Stage::ALL {
            assert_eq!(Stage::parse(stage.as_str()), Some(stage));
        }
        assert_eq!(Stage::parse("bogus"), None);
    }

    #[test]
    fn hysteresis_debounces_deescalation_but_not_escalation() {
        let mut governor = Governor::new(GovernorConfig {
            hysteresis_downgrade_ticks: 3,
            ..Default::default()
        });

        // Severe (available <= reserve_bytes).
        assert_eq!(
            governor.tick_level(2 * GIB, 0.0, 0.0),
            PressureLevel::Severe
        );
        // A single good sample does not immediately relax the level...
        assert_eq!(
            governor.tick_level(16 * GIB, 0.0, 0.0),
            PressureLevel::Severe
        );
        assert_eq!(
            governor.tick_level(16 * GIB, 0.0, 0.0),
            PressureLevel::Severe
        );
        // ...but the third consecutive good sample does.
        assert_eq!(
            governor.tick_level(16 * GIB, 0.0, 0.0),
            PressureLevel::Normal
        );

        // Escalation is always immediate, even mid-debounce.
        governor.tick_level(2 * GIB, 0.0, 0.0);
        governor.tick_level(16 * GIB, 0.0, 0.0);
        assert_eq!(
            governor.tick_level(512 * MIB, 0.0, 0.0),
            PressureLevel::Critical
        );
    }
}
