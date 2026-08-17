//! Declarative system-provider selection: one selected provider per exclusive role.

use lege_types::{AppId, ProviderCapability, ProviderRole};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A capability declared by an installed application (from its manifest or autostart entry).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub app_id: AppId,
    pub capability: ProviderCapability,
}

/// Registry of declarations plus user/distribution selections.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRegistry {
    pub declarations: Vec<Declaration>,
    /// Explicit selection per role (user choice or distribution policy).
    pub selections: BTreeMap<ProviderRole, AppId>,
}

/// Why a provider is (not) selected — reportable by `lege doctor` and the settings UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Resolution {
    /// Explicitly selected and installed.
    Selected { app_id: AppId },
    /// No explicit selection: highest-priority declaration wins.
    DefaultedByPriority {
        app_id: AppId,
        competitors: Vec<AppId>,
    },
    /// Selected app is not installed / no longer declares the role.
    SelectionStale { app_id: AppId },
    /// Nothing declares this role.
    NoProvider,
}

impl ProviderRegistry {
    pub fn declare(&mut self, app_id: AppId, capability: ProviderCapability) {
        self.declarations
            .retain(|d| !(d.app_id == app_id && d.capability.role == capability.role));
        self.declarations.push(Declaration { app_id, capability });
    }

    pub fn undeclare_app(&mut self, app_id: &AppId) {
        self.declarations.retain(|d| &d.app_id != app_id);
    }

    pub fn select(&mut self, role: ProviderRole, app_id: AppId) {
        self.selections.insert(role, app_id);
    }

    fn declaring(&self, role: ProviderRole) -> Vec<&Declaration> {
        let mut list: Vec<&Declaration> = self
            .declarations
            .iter()
            .filter(|d| d.capability.role == role)
            .collect();
        list.sort_by(|a, b| {
            b.capability
                .priority
                .cmp(&a.capability.priority)
                .then_with(|| a.app_id.cmp(&b.app_id))
        });
        list
    }

    /// Resolve the single provider for `role`.
    pub fn resolve(&self, role: ProviderRole) -> Resolution {
        let declaring = self.declaring(role);
        if let Some(selected) = self.selections.get(&role) {
            return if declaring.iter().any(|d| &d.app_id == selected) {
                Resolution::Selected {
                    app_id: selected.clone(),
                }
            } else {
                Resolution::SelectionStale {
                    app_id: selected.clone(),
                }
            };
        }
        match declaring.split_first() {
            None => Resolution::NoProvider,
            Some((winner, rest)) => Resolution::DefaultedByPriority {
                app_id: winner.app_id.clone(),
                competitors: rest.iter().map(|d| d.app_id.clone()).collect(),
            },
        }
    }

    /// The application allowed to fill `role`, if any.
    pub fn provider(&self, role: ProviderRole) -> Option<AppId> {
        match self.resolve(role) {
            Resolution::Selected { app_id } | Resolution::DefaultedByPriority { app_id, .. } => {
                Some(app_id)
            }
            Resolution::SelectionStale { .. } | Resolution::NoProvider => None,
        }
    }

    /// Whether `app_id` may autostart for `role` (exactly one may).
    pub fn may_autostart(&self, app_id: &AppId, role: ProviderRole) -> bool {
        self.provider(role).as_ref() == Some(app_id)
    }

    /// Full report for every role.
    pub fn report(&self) -> Vec<(ProviderRole, Resolution)> {
        ProviderRole::ALL
            .iter()
            .map(|role| (*role, self.resolve(*role)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(role: ProviderRole, priority: u32) -> ProviderCapability {
        ProviderCapability { role, priority }
    }

    #[test]
    fn exactly_one_network_indicator_may_autostart() {
        let mut registry = ProviderRegistry::default();
        let applet: AppId = "org.gnome.NmApplet".parse().unwrap();
        let tray: AppId = "org.legeos.NetworkTray".parse().unwrap();
        registry.declare(applet.clone(), cap(ProviderRole::NetworkStatusUi, 50));
        registry.declare(tray.clone(), cap(ProviderRole::NetworkStatusUi, 100));

        assert!(registry.may_autostart(&tray, ProviderRole::NetworkStatusUi));
        assert!(!registry.may_autostart(&applet, ProviderRole::NetworkStatusUi));
        assert!(
            matches!(registry.resolve(ProviderRole::NetworkStatusUi), Resolution::DefaultedByPriority { competitors, .. } if competitors == vec![applet.clone()])
        );

        registry.select(ProviderRole::NetworkStatusUi, applet.clone());
        assert!(registry.may_autostart(&applet, ProviderRole::NetworkStatusUi));
        assert!(!registry.may_autostart(&tray, ProviderRole::NetworkStatusUi));

        registry.undeclare_app(&applet); // uninstalled
        assert_eq!(
            registry.resolve(ProviderRole::NetworkStatusUi),
            Resolution::SelectionStale { app_id: applet }
        );
        assert_eq!(registry.provider(ProviderRole::NetworkStatusUi), None);
        assert_eq!(
            registry.resolve(ProviderRole::Greeter),
            Resolution::NoProvider
        );
    }
}
