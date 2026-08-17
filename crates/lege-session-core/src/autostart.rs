//! Permissioned autostart. An installed `.desktop` autostart file is a *request*; the session
//! daemon launches only entries that are approved and, for exclusive roles, selected.
//! The LegeOS session masks `xdg-desktop-autostart.target` so nothing bypasses this.

use crate::providers::ProviderRegistry;
use lege_types::{AppId, ProviderRole};
use serde::{Deserialize, Serialize};

/// Background capabilities an application may request (planning doc 01 §7).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundPermission {
    LoginAutostart,
    ContinuousBackground,
    NetworkWhileHidden,
    GlobalShortcuts,
    ScreenCapture,
    InputMonitoring,
    Notifications,
    RemovableMedia,
}

/// One autostart request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartEntry {
    pub app_id: AppId,
    /// If set, the entry only starts when this app is the selected provider for the role.
    pub role: Option<ProviderRole>,
    pub requested: Vec<BackgroundPermission>,
}

/// User/distribution decision for an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    Approved,
    Denied,
    /// Not yet decided: distribution defaults apply (approved for role providers, denied otherwise).
    Undecided,
}

/// Outcome per entry, always with a reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Verdict {
    Launch,
    SkippedDenied,
    SkippedNotSelectedProvider { role: ProviderRole },
    SkippedUndecided,
}

/// Decide which autostart entries launch this login.
pub fn plan(
    entries: &[(AutostartEntry, Approval)],
    providers: &ProviderRegistry,
) -> Vec<(AppId, Verdict)> {
    entries
        .iter()
        .map(|(entry, approval)| {
            let verdict = match (approval, entry.role) {
                (Approval::Denied, _) => Verdict::SkippedDenied,
                (_, Some(role)) if !providers.may_autostart(&entry.app_id, role) => {
                    Verdict::SkippedNotSelectedProvider { role }
                }
                (Approval::Approved, _) | (Approval::Undecided, Some(_)) => Verdict::Launch,
                (Approval::Undecided, None) => Verdict::SkippedUndecided,
            };
            (entry.app_id.clone(), verdict)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::ProviderCapability;

    #[test]
    fn duplicate_role_providers_do_not_both_start() {
        let mut providers = ProviderRegistry::default();
        let a: AppId = "org.example.A".parse().unwrap();
        let b: AppId = "org.example.B".parse().unwrap();
        let c: AppId = "org.example.Sync".parse().unwrap();
        providers.declare(
            a.clone(),
            ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 100,
            },
        );
        providers.declare(
            b.clone(),
            ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 50,
            },
        );

        let entries = vec![
            (
                AutostartEntry {
                    app_id: a.clone(),
                    role: Some(ProviderRole::NetworkStatusUi),
                    requested: vec![],
                },
                Approval::Undecided,
            ),
            (
                AutostartEntry {
                    app_id: b.clone(),
                    role: Some(ProviderRole::NetworkStatusUi),
                    requested: vec![],
                },
                Approval::Approved,
            ),
            (
                AutostartEntry {
                    app_id: c.clone(),
                    role: None,
                    requested: vec![BackgroundPermission::ContinuousBackground],
                },
                Approval::Undecided,
            ),
        ];
        let plan = plan(&entries, &providers);
        assert_eq!(plan[0], (a, Verdict::Launch));
        assert_eq!(
            plan[1],
            (
                b,
                Verdict::SkippedNotSelectedProvider {
                    role: ProviderRole::NetworkStatusUi
                }
            )
        );
        assert_eq!(plan[2], (c, Verdict::SkippedUndecided));
    }
}
