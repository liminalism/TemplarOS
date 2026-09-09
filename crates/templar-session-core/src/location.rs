//! Timezone / location policy — a service, not a tray applet.
//!
//! Combines multiple sources, keeps a confidence, remembers manual corrections and **refuses to
//! silently change the timezone when providers disagree** (several IP geolocation databases
//! return `Asia/Jakarta` where `Asia/Makassar` is correct).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One provider's opinion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimezoneCandidate {
    pub provider: String,
    pub timezone: String,
    /// 0.0..=1.0
    pub confidence: f64,
}

/// What the policy decided and why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum TimezoneDecision {
    /// Apply this zone.
    Apply { timezone: String, confidence: f64 },
    /// Keep the current zone; a manual correction covers this location.
    KeepManualOverride { timezone: String },
    /// Providers disagree beyond tolerance: keep the current zone and surface a choice.
    Ambiguous { candidates: Vec<String> },
    /// Nothing usable came back.
    NoData,
}

/// Policy state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimezonePolicy {
    /// Manual corrections keyed by a coarse location key (e.g. network SSID or /24), so a bad
    /// provider never overrides what the user fixed at this place.
    pub manual_overrides: BTreeMap<String, String>,
    /// Minimum agreement (weighted share) required to auto-apply.
    pub required_agreement: f64,
}

impl TimezonePolicy {
    pub fn new() -> Self {
        Self {
            manual_overrides: BTreeMap::new(),
            required_agreement: 0.75,
        }
    }

    pub fn decide(
        &self,
        location_key: Option<&str>,
        candidates: &[TimezoneCandidate],
    ) -> TimezoneDecision {
        if let Some(zone) = location_key.and_then(|key| self.manual_overrides.get(key)) {
            return TimezoneDecision::KeepManualOverride {
                timezone: zone.clone(),
            };
        }
        let total: f64 = candidates
            .iter()
            .map(|c| c.confidence.clamp(0.0, 1.0))
            .sum();
        if total <= 0.0 {
            return TimezoneDecision::NoData;
        }
        let mut weights: BTreeMap<&str, f64> = BTreeMap::new();
        for candidate in candidates {
            *weights.entry(candidate.timezone.as_str()).or_default() +=
                candidate.confidence.clamp(0.0, 1.0);
        }
        let (best, weight) = weights
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("non-empty");
        let share = weight / total;
        if share >= self.required_agreement {
            TimezoneDecision::Apply {
                timezone: (*best).to_owned(),
                confidence: share,
            }
        } else {
            let mut names: Vec<String> = weights.keys().map(|k| (*k).to_owned()).collect();
            names.sort();
            TimezoneDecision::Ambiguous { candidates: names }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(provider: &str, tz: &str, conf: f64) -> TimezoneCandidate {
        TimezoneCandidate {
            provider: provider.into(),
            timezone: tz.into(),
            confidence: conf,
        }
    }

    #[test]
    fn disagreement_is_surfaced_not_applied() {
        let policy = TimezonePolicy::new();
        let decision = policy.decide(
            None,
            &[
                c("a", "Asia/Jakarta", 0.8),
                c("b", "Asia/Makassar", 0.8),
                c("c", "Asia/Jakarta", 0.3),
            ],
        );
        assert!(
            matches!(decision, TimezoneDecision::Ambiguous { candidates } if candidates == vec!["Asia/Jakarta", "Asia/Makassar"])
        );
    }

    #[test]
    fn agreement_applies_and_manual_override_wins() {
        let mut policy = TimezonePolicy::new();
        let decision = policy.decide(
            None,
            &[
                c("a", "Europe/Berlin", 0.9),
                c("b", "Europe/Berlin", 0.7),
                c("c", "Europe/Paris", 0.2),
            ],
        );
        assert!(
            matches!(decision, TimezoneDecision::Apply { timezone, .. } if timezone == "Europe/Berlin")
        );

        policy
            .manual_overrides
            .insert("home-wifi".into(), "Asia/Makassar".into());
        let decision = policy.decide(Some("home-wifi"), &[c("a", "Asia/Jakarta", 1.0)]);
        assert_eq!(
            decision,
            TimezoneDecision::KeepManualOverride {
                timezone: "Asia/Makassar".into()
            }
        );
        assert_eq!(policy.decide(None, &[]), TimezoneDecision::NoData);
    }
}
