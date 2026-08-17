//! Pure policy engine: no I/O, fully unit-testable.
//!
//! The constants are starting values for trace collection on real systems, not final policy.
//! A browser with 12 GiB of useful tabs may be less disposable than a background image
//! conversion using 5 GiB; memory size alone must never select the victim.

use lege_types::InstanceId;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
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

/// What the governor wants done. The executor applies it (or only logs it in observe-only mode).
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

#[derive(Clone, Debug, PartialEq)]
pub struct GovernorConfig {
    pub reserve_bytes: u64,
    pub critical_reserve_bytes: u64,

    pub elevated_some_avg10: f64,
    pub severe_full_avg10: f64,
    pub critical_full_avg10: f64,

    pub emergency_kill_delay: Duration,
    pub minimum_high_limit: u64,
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
        }
    }
}

/// The pressure state machine.
#[derive(Debug)]
pub struct Governor {
    config: GovernorConfig,
    critical_since: Option<Instant>,
}

impl Governor {
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            config,
            critical_since: None,
        }
    }

    pub fn config(&self) -> &GovernorConfig {
        &self.config
    }

    /// Classify global state from `MemAvailable` and global memory PSI.
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

        let Some(target) =
            applications
                .iter()
                .filter(|app| !app.protected)
                .max_by(|left, right| {
                    victim_score(left)
                        .partial_cmp(&victim_score(right))
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

/// Higher = better victim. Never memory size alone.
pub fn victim_score(app: &AppSample) -> f64 {
    if app.protected {
        return f64::NEG_INFINITY;
    }

    let memory_gib = app.memory_current as f64 / GIB as f64;
    let growth_gib = app.memory_growth_per_second.max(0) as f64 / GIB as f64;

    let mut score = memory_gib * 8.0 + growth_gib * 30.0 + app.full_psi_avg10 * 4.0;
    if app.disposable {
        score += 60.0;
    }
    if app.visible {
        score -= 20.0;
    }
    if app.foreground {
        score -= 80.0;
    }
    if app.reports_unsaved_state {
        score -= 60.0;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut big_visible = sample(8);
        big_visible.visible = true;
        let mut small_disposable = sample(1);
        small_disposable.disposable = true;
        assert!(victim_score(&small_disposable) > victim_score(&big_visible));
    }
}
