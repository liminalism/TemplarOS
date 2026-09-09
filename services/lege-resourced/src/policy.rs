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

    /// How long since the user last interacted with this instance. `None` means nobody has said
    /// — which is not the same as "a long time", and must not be scored as if it were.
    pub last_interaction_age: Option<Duration>,
    /// The instance belongs to a project the user marked important.
    pub project_important: bool,
    /// The instance serves `org.legeos.Checkpoint1` and can be asked to save and be restarted.
    ///
    /// This has existed as enrichment since the beginning and never reached the score. It is the
    /// difference between an application that can be *contained* and one that can only be
    /// killed, which is exactly the kind of thing choosing a victim should weigh.
    pub supports_checkpoint: bool,
    /// How long ago its last successful checkpoint was taken.
    pub checkpoint_age: Option<Duration>,
    /// The user asked for this one to be kept.
    pub user_pinned: bool,
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

    /// Per minute since the user last touched the instance, capped by `idle_bonus_cap`.
    ///
    /// Capped rather than unbounded because an application left open for a week is not a
    /// thousand times better a victim than one left for an hour — after a while, "idle" is just
    /// idle, and an uncapped term would eventually outweigh every other consideration including
    /// unsaved work.
    pub idle_bonus_per_minute: f64,
    pub idle_bonus_cap: f64,
    /// Flat term (expected negative) for an instance belonging to an important project.
    pub project_protection: f64,
    /// Flat bonus for an instance that can be asked to checkpoint at all.
    pub checkpoint_capable_bonus: f64,
    /// Additional bonus when its last checkpoint is recent enough to restore from.
    pub fresh_checkpoint_bonus: f64,
    /// Flat term (expected strongly negative) when the user pinned the instance.
    ///
    /// **A weight, not an exclusion.** `protected` stays the only hard exclusion, so a pinned
    /// application that starts consuming the machine can still be contained. Making a user pin
    /// unbreakable would mean the one way to make a desktop unrecoverable is to ask for
    /// something reasonable.
    pub pinned_protection: f64,
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
            // Declared guesses, like every number above them, and to be recalibrated from real
            // traces. Their *relative* placement is the part that is reasoned: idle saturates
            // below a disposable application's bonus, because being unused is weaker evidence
            // than being declared throwaway; a pinned instance outranks the foreground one,
            // because the user asking for something is stronger than the user looking at it; and
            // a fresh checkpoint is worth less than disposability, because being restorable is
            // not the same as being expendable.
            idle_bonus_per_minute: 2.0,
            idle_bonus_cap: 40.0,
            project_protection: -40.0,
            checkpoint_capable_bonus: 8.0,
            fresh_checkpoint_bonus: 25.0,
            pinned_protection: -120.0,
        }
    }
}

/// How recent a checkpoint has to be to count as one the governor could restore from.
///
/// Five minutes: long enough that an application checkpointed during the last pressure episode
/// still counts, short enough that a checkpoint from before the user's current work does not.
pub const FRESH_CHECKPOINT_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GovernorConfig {
    pub reserve_bytes: u64,
    pub critical_reserve_bytes: u64,
    /// This machine's `MemTotal`. Read once at startup; used to size the thresholds and to judge
    /// what counts as a runaway.
    pub total_memory_bytes: u64,

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
            total_memory_bytes: mem_total,
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
            total_memory_bytes: REFERENCE_MEMORY,
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

/// One instance's share of a reclaim plan: what to do to it, what that is expected to free, and
/// why it was chosen.
#[derive(Clone, Debug, PartialEq)]
pub struct ReclaimStep {
    pub instance_id: InstanceId,
    pub actions: Vec<Action>,
    /// A rough estimate, in bytes. Rough is honest: `memory.high` and `memory.reclaim` are
    /// requests to the kernel, not transactions, and what actually comes back depends on what is
    /// reclaimable. It exists so a plan can stop when it has probably done enough rather than
    /// always doing the maximum.
    pub expected_relief_bytes: u64,
    pub reason: String,
}

/// What the governor intends to do this tick, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct ReclaimPlan {
    pub level: PressureLevel,
    /// How far below the reserve the machine is. Zero above the reserve.
    pub deficit_bytes: u64,
    pub steps: Vec<ReclaimStep>,
}

impl ReclaimPlan {
    /// Every action, in plan order.
    #[must_use]
    pub fn actions(&self) -> Vec<Action> {
        self.steps
            .iter()
            .flat_map(|step| step.actions.iter().cloned())
            .collect()
    }

    #[must_use]
    pub fn expected_relief_bytes(&self) -> u64 {
        self.steps
            .iter()
            .map(|step| step.expected_relief_bytes)
            .sum()
    }
}

/// How many idle or disposable instances `Elevated` will trim in one tick.
///
/// Three. Trimming a cache is the cheapest useful thing the governor can do and nobody notices
/// it; doing it to one instance at a time means several ticks of mild pressure before the same
/// amount of memory comes back.
pub const ELEVATED_TRIM_LIMIT: usize = 3;

/// How many instances `Severe` will throttle in one tick.
///
/// Two. Every throttled application is one the user may be about to return to, and a plan that
/// touched five of them to cover a deficit would be indistinguishable from the machine having
/// simply become slow.
pub const SEVERE_THROTTLE_LIMIT: usize = 2;

/// Growth at or above which an instance is treated as a runaway rather than merely large.
///
/// A declared guess, deliberately far above anything an ordinary application does: the cost of a
/// false positive is throttling the window the user is working in.
pub const RUNAWAY_GROWTH_PER_SECOND: u64 = 256 * MIB;

/// How long an instance must be untouched to count as idle for `Elevated` trimming.
pub const IDLE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Whether nobody has touched this instance for a while. `None` — nobody has said — is not idle.
#[must_use]
fn is_idle(app: &AppSample) -> bool {
    app.last_interaction_age
        .is_some_and(|age| age >= IDLE_AFTER)
}

/// The pressure state machine.
#[derive(Debug)]
pub struct Governor {
    config: GovernorConfig,
    /// Which instance the emergency grace period is running for, and since when.
    ///
    /// Bound to the victim, not to the episode: the delay is a *specific application's* chance to
    /// checkpoint before it is terminated, so it belongs to whoever was chosen. Kept for the
    /// episode instead, it became a countdown the machine was running — and the next instance
    /// selected inherited an expired one and was killed on the first tick it was ever chosen on,
    /// having been asked to save its work in the same breath.
    critical_since: Option<(InstanceId, Instant)>,
    current_level: PressureLevel,
    /// A raw level below `current_level` seen for `pending.1` consecutive ticks so far.
    pending: Option<(PressureLevel, u32)>,
    /// `MemAvailable` at the last [`Governor::tick_level`].
    ///
    /// Kept here rather than passed to [`Governor::plan`] so that `decide`'s signature — which
    /// every test and the replay harness call — does not change. The two are always called in
    /// order, one tick apart at most.
    last_available_bytes: u64,
}

impl Governor {
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            last_available_bytes: config.reserve_bytes,
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
        self.last_available_bytes = available_bytes;
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

    /// Decide what to do this tick. A thin wrapper over [`Governor::plan`].
    ///
    /// Kept because it is what every test and the replay harness call, and because "the actions
    /// for this tick" is genuinely the common question; the plan is what to ask when the answer
    /// needs to be explained.
    pub fn decide(
        &mut self,
        now: Instant,
        level: PressureLevel,
        applications: &[AppSample],
    ) -> Vec<Action> {
        self.plan(now, level, applications).actions()
    }

    /// What to reclaim, from whom, and why.
    ///
    /// The governor used to pick exactly one victim at every level. That is right at `Critical`,
    /// where narrowness is the safety property — one instance contained, deliberately, rather
    /// than a broad action taken in a hurry — and wrong everywhere below it. At `Elevated` the
    /// cheapest useful response is trimming several idle caches, none of which anyone notices; at
    /// `Severe` one instance's ten percent frequently does not cover the deficit, so the machine
    /// stays severe and the same instance is squeezed again next tick while three others sit
    /// untouched.
    ///
    /// Every step says what it expects to free and why, because a plan nobody can read is a plan
    /// nobody can calibrate — and these numbers need calibrating from real traces.
    pub fn plan(
        &mut self,
        now: Instant,
        level: PressureLevel,
        applications: &[AppSample],
    ) -> ReclaimPlan {
        if level != PressureLevel::Critical {
            self.critical_since = None;
        }
        let deficit_bytes = self
            .config
            .reserve_bytes
            .saturating_sub(self.last_available_bytes);

        if level == PressureLevel::Normal {
            return ReclaimPlan {
                level,
                deficit_bytes: 0,
                steps: applications
                    .iter()
                    .map(|app| ReclaimStep {
                        instance_id: app.instance_id,
                        actions: vec![Action::RestoreDynamicLimits {
                            instance_id: app.instance_id,
                        }],
                        expected_relief_bytes: 0,
                        reason: "pressure is normal; any temporary limit is lifted".to_owned(),
                    })
                    .collect(),
            };
        }

        let ranked = self.ranked_candidates(level, applications);
        let steps = match level {
            PressureLevel::Normal => unreachable!("handled above"),
            PressureLevel::Elevated => Self::elevated_steps(&ranked),
            PressureLevel::Severe => self.severe_steps(&ranked, deficit_bytes),
            PressureLevel::Critical => self.critical_steps(now, &ranked),
        };
        ReclaimPlan {
            level,
            deficit_bytes,
            steps,
        }
    }

    /// Eligible candidates, best victim first.
    ///
    /// `protected` is the one hard exclusion and is applied here, once, structurally. At `Severe`
    /// the foreground application joins it — the thing the user is looking at is the last thing
    /// to throttle while there is anything else to throttle — *unless* it is demonstrably the
    /// cause, in which case protecting it would mean containing everything except the problem.
    /// At `Critical` it is eligible again but sorted last, because at that point the alternative
    /// to touching it is the kernel's OOM killer touching something at random.
    fn ranked_candidates<'a>(
        &self,
        level: PressureLevel,
        applications: &'a [AppSample],
    ) -> Vec<&'a AppSample> {
        let weights = &self.config.victim_weights;
        let mut candidates: Vec<&AppSample> = applications
            .iter()
            .filter(|app| !app.protected)
            .filter(|app| level != PressureLevel::Severe || !app.foreground || self.is_runaway(app))
            .collect();
        candidates.sort_by(|left, right| {
            // At Critical the foreground application goes last among eligible candidates.
            let foreground_last = left.foreground.cmp(&right.foreground);
            foreground_last.then_with(|| {
                victim_score(right, weights)
                    .partial_cmp(&victim_score(left, weights))
                    .unwrap_or(Ordering::Equal)
            })
        });
        candidates
    }

    /// Whether this instance is demonstrably the cause rather than merely large.
    ///
    /// Two declared guesses, to be recalibrated from real traces: growing by a quarter of a
    /// gigabyte a second, or already holding half the machine. Both are deliberately far above
    /// anything an ordinary application does, because the cost of a false positive here is
    /// throttling the window the user is working in.
    fn is_runaway(&self, app: &AppSample) -> bool {
        app.memory_growth_per_second >= i64::try_from(RUNAWAY_GROWTH_PER_SECOND).unwrap_or(i64::MAX)
            || app.memory_current >= self.config.total_memory_bytes / 2
    }

    /// Trim up to three idle or disposable caches. Nobody notices a trimmed cache.
    fn elevated_steps(ranked: &[&AppSample]) -> Vec<ReclaimStep> {
        ranked
            .iter()
            .filter(|app| !app.foreground)
            .filter(|app| app.disposable || is_idle(app))
            .take(ELEVATED_TRIM_LIMIT)
            .map(|app| ReclaimStep {
                instance_id: app.instance_id,
                actions: vec![Action::TrimCaches {
                    instance_id: app.instance_id,
                }],
                expected_relief_bytes: 0,
                reason: if app.disposable {
                    "disposable, and not on screen".to_owned()
                } else {
                    "idle, and not on screen".to_owned()
                },
            })
            .collect()
    }

    /// Throttle the top candidates until the deficit is covered, up to two.
    ///
    /// Two, not more: every throttled application is one the user may be about to return to, and
    /// a plan that touched five of them to cover a deficit would be indistinguishable from the
    /// machine having simply become slow.
    fn severe_steps(&self, ranked: &[&AppSample], deficit_bytes: u64) -> Vec<ReclaimStep> {
        let mut steps = Vec::new();
        let mut covered = 0_u64;
        for app in ranked.iter().take(SEVERE_THROTTLE_LIMIT) {
            let high = app
                .memory_current
                .saturating_mul(9)
                .saturating_div(10)
                .max(self.config.minimum_high_limit);
            let reclaim = (app.memory_current / 8).min(256 * MIB);
            let relief = app
                .memory_current
                .saturating_sub(high)
                .saturating_add(reclaim);
            steps.push(ReclaimStep {
                instance_id: app.instance_id,
                actions: vec![
                    Action::TrimCaches {
                        instance_id: app.instance_id,
                    },
                    Action::SetMemoryHigh {
                        instance_id: app.instance_id,
                        bytes: high,
                    },
                    Action::Reclaim {
                        instance_id: app.instance_id,
                        bytes: reclaim,
                    },
                ],
                expected_relief_bytes: relief,
                reason: format!(
                    "holding {} MiB under severe pressure",
                    app.memory_current / MIB
                ),
            });
            covered = covered.saturating_add(relief);
            if covered >= deficit_bytes {
                break;
            }
        }
        steps
    }

    /// One victim, contained hard, and terminated only after the delay has elapsed.
    fn critical_steps(&mut self, now: Instant, ranked: &[&AppSample]) -> Vec<ReclaimStep> {
        let Some(target) = ranked.first() else {
            // Nobody to contain: whatever grace period was running was somebody else's.
            self.critical_since = None;
            return Vec::new();
        };
        let started = match self.critical_since {
            Some((instance_id, started)) if instance_id == target.instance_id => started,
            _ => {
                // A different victim — the first one exited, or the ranking moved. Its grace
                // period does not transfer: this one is being asked to checkpoint now, and gets
                // the whole delay to do it in.
                self.critical_since = Some((target.instance_id, now));
                now
            }
        };
        let elapsed = now.saturating_duration_since(started);
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
        vec![ReclaimStep {
            instance_id: target.instance_id,
            actions,
            expected_relief_bytes: target.memory_current.saturating_sub(high),
            reason: "the single highest-scoring candidate under critical pressure".to_owned(),
        }]
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
    #[serde(default)]
    pub idle_term: f64,
    #[serde(default)]
    pub project_term: f64,
    #[serde(default)]
    pub checkpoint_term: f64,
    #[serde(default)]
    pub pinned_term: f64,
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
    // `None` is "nobody has told us", not "zero minutes" and not "forever": an instance nothing
    // reports activity for scores exactly as it did before any of this existed.
    let idle_term = app.last_interaction_age.map_or(0.0, |age| {
        let minutes = age.as_secs_f64() / 60.0;
        (minutes * weights.idle_bonus_per_minute).min(weights.idle_bonus_cap)
    });
    let project_term = if app.project_important {
        weights.project_protection
    } else {
        0.0
    };
    // Capability and freshness are one term because they are one idea: how cheaply this instance
    // can be put down and brought back. An application that could checkpoint but has not is
    // partway there; one with a checkpoint from a minute ago is most of the way.
    let checkpoint_term = if app.supports_checkpoint {
        let fresh = app
            .checkpoint_age
            .is_some_and(|age| age <= FRESH_CHECKPOINT_WINDOW);
        if fresh {
            weights.checkpoint_capable_bonus + weights.fresh_checkpoint_bonus
        } else {
            weights.checkpoint_capable_bonus
        }
    } else {
        0.0
    };
    let pinned_term = if app.user_pinned {
        weights.pinned_protection
    } else {
        0.0
    };

    let total = memory_term
        + growth_term
        + psi_term
        + disposable_term
        + visible_term
        + foreground_term
        + unsaved_term
        + idle_term
        + project_term
        + checkpoint_term
        + pinned_term;

    ScoreBreakdown {
        memory_term,
        growth_term,
        psi_term,
        disposable_term,
        visible_term,
        foreground_term,
        unsaved_term,
        idle_term,
        project_term,
        checkpoint_term,
        pinned_term,
        total,
    }
}

pub fn victim_score(app: &AppSample, weights: &VictimWeights) -> f64 {
    victim_score_breakdown(app, weights).total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A governor whose last observed `MemAvailable` is `available`.
    fn governor_at(available: u64) -> Governor {
        let mut governor = Governor::new(GovernorConfig::default());
        governor.tick_level(available, 0.0, 0.0);
        governor
    }

    fn idle(memory_gib: u64) -> AppSample {
        let mut app = sample(memory_gib);
        app.last_interaction_age = Some(IDLE_AFTER + Duration::from_secs(1));
        app
    }

    #[test]
    fn elevated_trims_several_idle_caches_rather_than_one() {
        // Trimming a cache is the cheapest useful thing the governor can do and nobody notices
        // it. Doing it one instance at a time meant several ticks of mild pressure before the
        // same memory came back.
        let mut governor = governor_at(4 * GIB);
        let apps: Vec<AppSample> = (1..=5).map(idle).collect();
        let plan = governor.plan(Instant::now(), PressureLevel::Elevated, &apps);
        assert_eq!(plan.steps.len(), ELEVATED_TRIM_LIMIT);
        assert!(
            plan.actions()
                .iter()
                .all(|action| matches!(action, Action::TrimCaches { .. }))
        );
    }

    #[test]
    fn elevated_leaves_alone_anything_the_user_might_be_using() {
        // Neither disposable nor idle: trimming it costs the user a re-read of everything it had
        // cached, for mild pressure that may pass on its own.
        let mut governor = governor_at(4 * GIB);
        let busy = sample(4);
        let plan = governor.plan(Instant::now(), PressureLevel::Elevated, &[busy]);
        assert!(plan.steps.is_empty(), "{plan:?}");
    }

    #[test]
    fn severe_stops_as_soon_as_the_deficit_is_covered() {
        // One instance's ten percent frequently does not cover a deficit, and the old single
        // victim meant the machine stayed severe while three other instances sat untouched.
        let config = GovernorConfig::default();
        // Two modest instances: each is worth roughly 460 MiB of relief, so covering a 3 GiB
        // deficit takes both of them and covering a one-byte deficit takes neither twice.
        let apps = [sample(2), sample(2)];

        let mut just_under = Governor::new(config);
        just_under.tick_level(config.reserve_bytes - 1, 0.0, 0.0);
        let plan = just_under.plan(Instant::now(), PressureLevel::Severe, &apps);
        assert_eq!(plan.steps.len(), 1, "{plan:?}");

        let mut wide = governor_at(0);
        let plan = wide.plan(Instant::now(), PressureLevel::Severe, &apps);
        assert_eq!(plan.steps.len(), 2, "{plan:?}");
    }

    #[test]
    fn severe_never_throttles_more_than_two_instances() {
        // Every throttled application is one the user may be about to return to; five of them at
        // once is indistinguishable from the machine having simply become slow.
        let mut governor = governor_at(0);
        let apps: Vec<AppSample> = (1..=6).map(sample).collect();
        let plan = governor.plan(Instant::now(), PressureLevel::Severe, &apps);
        assert_eq!(plan.steps.len(), SEVERE_THROTTLE_LIMIT);
    }

    #[test]
    fn severe_spares_the_window_the_user_is_working_in() {
        let mut governor = governor_at(0);
        // Large, but not half the machine and not growing: the foreground window, not the cause.
        let mut front = sample(4);
        front.foreground = true;
        let background = sample(2);
        let plan = governor.plan(
            Instant::now(),
            PressureLevel::Severe,
            &[front.clone(), background.clone()],
        );
        assert_eq!(
            plan.steps.iter().map(|s| s.instance_id).collect::<Vec<_>>(),
            vec![background.instance_id],
            "the foreground window is the last thing to throttle while there is anything else"
        );
    }

    #[test]
    fn severe_reaches_the_foreground_when_it_is_demonstrably_the_cause() {
        // Protecting the foreground unconditionally would mean containing everything except the
        // problem, while the problem keeps growing.
        let mut governor = governor_at(0);
        let mut runaway = sample(4);
        runaway.foreground = true;
        runaway.memory_growth_per_second = i64::try_from(RUNAWAY_GROWTH_PER_SECOND).unwrap();
        let plan = governor.plan(Instant::now(), PressureLevel::Severe, &[runaway.clone()]);
        assert_eq!(
            plan.steps.first().map(|s| s.instance_id),
            Some(runaway.instance_id)
        );

        // Merely being the foreground application and large is not enough.
        let mut large = sample(4);
        large.foreground = true;
        let plan = governor.plan(Instant::now(), PressureLevel::Severe, &[large]);
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn holding_half_the_machine_also_counts_as_a_runaway() {
        let mut governor = governor_at(0);
        let mut hog = sample(24); // half of the 48 GiB reference machine
        hog.foreground = true;
        let plan = governor.plan(Instant::now(), PressureLevel::Severe, &[hog.clone()]);
        assert_eq!(
            plan.steps.first().map(|s| s.instance_id),
            Some(hog.instance_id)
        );
    }

    #[test]
    fn critical_stays_a_single_victim() {
        // Narrowness is the safety property here: one instance contained, deliberately, rather
        // than a broad action taken in a hurry.
        let mut governor = governor_at(0);
        let apps: Vec<AppSample> = (1..=5).map(sample).collect();
        let plan = governor.plan(Instant::now(), PressureLevel::Critical, &apps);
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn critical_reaches_the_foreground_only_after_everything_else() {
        // Eligible, because the alternative at this point is the kernel's OOM killer choosing at
        // random — but last, because it is still the window the user is in.
        let mut governor = governor_at(0);
        let mut front = sample(4);
        front.foreground = true;
        let other = sample(1);
        let plan = governor.plan(
            Instant::now(),
            PressureLevel::Critical,
            &[front.clone(), other.clone()],
        );
        assert_eq!(plan.steps[0].instance_id, other.instance_id);

        let alone = governor.plan(Instant::now(), PressureLevel::Critical, &[front.clone()]);
        assert_eq!(alone.steps[0].instance_id, front.instance_id);
    }

    #[test]
    fn a_plan_says_what_it_expects_to_free_and_why() {
        // A plan nobody can read is a plan nobody can calibrate, and these numbers need
        // calibrating from real traces.
        let mut governor = governor_at(0);
        let plan = governor.plan(Instant::now(), PressureLevel::Severe, &[sample(8)]);
        let step = &plan.steps[0];
        assert!(step.expected_relief_bytes > 0);
        assert!(step.reason.contains("MiB"), "{}", step.reason);
        assert_eq!(plan.expected_relief_bytes(), step.expected_relief_bytes);
        assert!(plan.deficit_bytes > 0);
    }

    #[test]
    fn decide_is_exactly_the_plans_actions() {
        let apps: Vec<AppSample> = (1..=4).map(sample).collect();
        let mut one = governor_at(0);
        let mut two = governor_at(0);
        let now = Instant::now();
        assert_eq!(
            one.decide(now, PressureLevel::Severe, &apps),
            two.plan(now, PressureLevel::Severe, &apps).actions()
        );
    }

    #[test]
    fn a_pinned_instance_is_protected_but_not_excluded() {
        // The line this weight walks. A pin must count for a great deal — more than the user
        // merely looking at something — and must still be losable, because an unbreakable pin
        // would make asking for something reasonable the way to render a desktop unrecoverable.
        let weights = VictimWeights::default();
        let mut pinned = sample(4);
        pinned.user_pinned = true;
        let ordinary = sample(4);
        assert!(victim_score(&pinned, &weights) < victim_score(&ordinary, &weights));
        assert!(
            weights.pinned_protection < weights.foreground_protection,
            "a pin the user asked for should outweigh the window they happen to be looking at"
        );

        // A runaway pinned application is still reachable: it is a weight, not an exclusion.
        let mut runaway = sample(64);
        runaway.user_pinned = true;
        let small = sample(1);
        let mut governor = Governor::new(GovernorConfig::default());
        let actions = governor.decide(
            Instant::now(),
            PressureLevel::Severe,
            &[runaway.clone(), small],
        );
        assert_eq!(
            actions.first().map(Action::instance_id),
            Some(runaway.instance_id),
            "a pinned instance consuming 64 GiB must still be containable"
        );
    }

    #[test]
    fn only_protected_excludes_anything() {
        // `protected` is the one hard exclusion, enforced structurally by filtering the pool.
        let mut protected = sample(64);
        protected.protected = true;
        let small = sample(1);
        let mut governor = Governor::new(GovernorConfig::default());
        let actions = governor.decide(
            Instant::now(),
            PressureLevel::Severe,
            &[protected, small.clone()],
        );
        assert_eq!(
            actions.first().map(Action::instance_id),
            Some(small.instance_id)
        );
    }

    #[test]
    fn the_idle_bonus_is_capped_rather_than_growing_for_ever() {
        // An application left open for a week is not a thousand times better a victim than one
        // left for an hour; an uncapped term would eventually outweigh unsaved work itself.
        let weights = VictimWeights::default();
        let score_at = |minutes: u64| {
            let mut app = sample(1);
            app.last_interaction_age = Some(Duration::from_secs(minutes * 60));
            victim_score_breakdown(&app, &weights).idle_term
        };
        assert!((score_at(10) - 20.0).abs() < f64::EPSILON);
        assert!((score_at(20) - weights.idle_bonus_cap).abs() < f64::EPSILON);
        assert!((score_at(10_000) - weights.idle_bonus_cap).abs() < f64::EPSILON);
        assert!(
            weights.idle_bonus_cap < weights.disposable_bonus,
            "being unused is weaker evidence than being declared throwaway"
        );
    }

    #[test]
    fn nobody_having_said_is_not_the_same_as_a_long_time_ago() {
        // The default for every new signal. An instance nothing reports must score exactly as it
        // did before these signals existed, or the change is a silent policy shift on every
        // machine whose senders are not wired up yet.
        let weights = VictimWeights::default();
        let unreported = sample(4);
        let breakdown = victim_score_breakdown(&unreported, &weights);
        assert!((breakdown.idle_term - 0.0).abs() < f64::EPSILON);
        assert!((breakdown.checkpoint_term - 0.0).abs() < f64::EPSILON);
        assert!((breakdown.project_term - 0.0).abs() < f64::EPSILON);
        assert!((breakdown.pinned_term - 0.0).abs() < f64::EPSILON);
        assert!(
            (breakdown.total - 32.0).abs() < f64::EPSILON,
            "{breakdown:?}"
        );
    }

    #[test]
    fn a_restorable_instance_is_a_better_candidate_than_one_that_can_only_be_killed() {
        let weights = VictimWeights::default();
        let plain = sample(2);

        let mut capable = sample(2);
        capable.supports_checkpoint = true;
        let mut fresh = capable.clone();
        fresh.checkpoint_age = Some(Duration::from_secs(60));
        let mut stale = capable.clone();
        stale.checkpoint_age = Some(FRESH_CHECKPOINT_WINDOW + Duration::from_secs(1));

        assert!(victim_score(&capable, &weights) > victim_score(&plain, &weights));
        assert!(victim_score(&fresh, &weights) > victim_score(&capable, &weights));
        assert!(
            (victim_score(&stale, &weights) - victim_score(&capable, &weights)).abs()
                < f64::EPSILON,
            "a checkpoint older than the window buys nothing extra"
        );
        assert!(
            weights.checkpoint_capable_bonus + weights.fresh_checkpoint_bonus
                < weights.disposable_bonus,
            "being restorable is not the same as being expendable"
        );
    }

    #[test]
    fn an_important_project_protects_the_instances_it_launched() {
        let weights = VictimWeights::default();
        let mut in_project = sample(4);
        in_project.project_important = true;
        assert!(victim_score(&in_project, &weights) < victim_score(&sample(4), &weights));
    }

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
            last_interaction_age: None,
            project_important: false,
            supports_checkpoint: false,
            checkpoint_age: None,
            user_pinned: false,
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

    /// The grace period belongs to the victim, not to the pressure episode. Critical pressure
    /// that persists while the chosen instance exits used to leave the countdown running, so the
    /// next instance chosen was asked to checkpoint and killed in the same tick — a request with
    /// no time in it at all.
    #[test]
    fn a_new_victim_gets_the_whole_grace_period_of_its_own() {
        let mut governor = Governor::new(GovernorConfig {
            emergency_kill_delay: Duration::from_secs(3),
            ..Default::default()
        });
        let first = sample(10);
        let second = sample(9);
        let start = Instant::now();

        governor.decide(start, PressureLevel::Critical, std::slice::from_ref(&first));
        let killed = governor.decide(
            start + Duration::from_secs(4),
            PressureLevel::Critical,
            std::slice::from_ref(&first),
        );
        assert!(killed.iter().any(|action| matches!(
            action,
            Action::KillCgroup { instance_id, .. } if *instance_id == first.instance_id
        )));

        // The first victim is gone; pressure has not moved. The second is contained and asked to
        // checkpoint — and not killed, because its own delay has not elapsed.
        let transition = governor.decide(
            start + Duration::from_secs(5),
            PressureLevel::Critical,
            std::slice::from_ref(&second),
        );
        assert!(transition.iter().any(|action| matches!(
            action,
            Action::RequestCheckpoint { instance_id, .. } if *instance_id == second.instance_id
        )));
        assert!(transition.iter().any(|action| matches!(
            action,
            Action::SetMemoryHigh { instance_id, .. } if *instance_id == second.instance_id
        )));
        assert!(
            !transition
                .iter()
                .any(|action| matches!(action, Action::KillCgroup { .. })),
            "the second victim inherited the first one's expired countdown"
        );

        // Still not, one second before its delay is up.
        let waiting = governor.decide(
            start + Duration::from_secs(7),
            PressureLevel::Critical,
            std::slice::from_ref(&second),
        );
        assert!(
            !waiting
                .iter()
                .any(|action| matches!(action, Action::KillCgroup { .. }))
        );

        // And then it is, on its own clock.
        let expired = governor.decide(
            start + Duration::from_secs(9),
            PressureLevel::Critical,
            std::slice::from_ref(&second),
        );
        assert!(expired.iter().any(|action| matches!(
            action,
            Action::KillCgroup { instance_id, .. } if *instance_id == second.instance_id
        )));
    }

    /// The same reset, for the other way a victim can go away: a tick with nothing selectable at
    /// all (everything left is protected) must not leave a countdown running for whoever is
    /// chosen next.
    #[test]
    fn a_critical_tick_with_no_target_does_not_leave_a_countdown_running() {
        let mut governor = Governor::new(GovernorConfig {
            emergency_kill_delay: Duration::from_secs(3),
            ..Default::default()
        });
        let victim = sample(10);
        let mut shielded = sample(10);
        shielded.protected = true;
        let start = Instant::now();

        governor.decide(
            start,
            PressureLevel::Critical,
            std::slice::from_ref(&victim),
        );
        assert!(
            governor
                .decide(
                    start + Duration::from_secs(1),
                    PressureLevel::Critical,
                    &[shielded],
                )
                .is_empty()
        );

        let returning = governor.decide(
            start + Duration::from_secs(4),
            PressureLevel::Critical,
            &[victim],
        );
        assert!(
            !returning
                .iter()
                .any(|action| matches!(action, Action::KillCgroup { .. })),
            "a tick with no target left the previous victim's countdown running"
        );
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
