//! First-run / migration: a one-time import of legacy state (`mimeapps.list`, XDG autostart,
//! dconf/LXQt/COSMIC settings) into the typed stores, with conflicts surfaced in Settings
//! rather than silently resolved.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegacySource {
    MimeappsList,
    XdgAutostart,
    CosmicConfig,
    LxqtConfig,
    Dconf,
}

/// One value found in a legacy store that maps onto a LegeOS setting/association/provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedItem {
    pub source: LegacySource,
    pub path: PathBuf,
    /// Target key (`assoc:application/pdf`, `provider:network-status-ui`, `setting:shell.dock.grouping`).
    pub target: String,
    pub value: String,
}

/// Two legacy sources disagree about one target; the user decides in Settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub target: String,
    pub candidates: Vec<ImportedItem>,
}

/// The plan produced by the importer and shown once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub apply: Vec<ImportedItem>,
    pub conflicts: Vec<Conflict>,
}

impl MigrationPlan {
    /// Group imported items by target; unanimous targets are applied, the rest become conflicts.
    pub fn from_items(items: Vec<ImportedItem>) -> Self {
        let mut by_target: std::collections::BTreeMap<String, Vec<ImportedItem>> =
            std::collections::BTreeMap::default();
        for item in items {
            by_target.entry(item.target.clone()).or_default().push(item);
        }
        let mut plan = Self::default();
        for (target, candidates) in by_target {
            let mut values: Vec<&str> = candidates.iter().map(|c| c.value.as_str()).collect();
            values.sort_unstable();
            values.dedup();
            if values.len() == 1 {
                plan.apply
                    .push(candidates.into_iter().next().expect("non-empty"));
            } else {
                plan.conflicts.push(Conflict { target, candidates });
            }
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disagreeing_sources_become_conflicts() {
        let item = |source, value: &str| ImportedItem {
            source,
            path: PathBuf::from("/x"),
            target: "assoc:application/pdf".into(),
            value: value.into(),
        };
        let plan = MigrationPlan::from_items(vec![
            item(LegacySource::MimeappsList, "org.legeos.Viewer.desktop"),
            item(LegacySource::CosmicConfig, "firefox.desktop"),
            ImportedItem {
                source: LegacySource::Dconf,
                path: "/y".into(),
                target: "setting:a".into(),
                value: "1".into(),
            },
        ]);
        assert_eq!(plan.apply.len(), 1);
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].candidates.len(), 2);
    }
}
