//! Storage reserve enforcement (system side). The user-facing numbers come from `lege-filed`;
//! hostd owns the policy that keeps the reserve free (cache budgets, journald/apt cache limits,
//! and — later — quota on the root subvolume).

use serde::{Deserialize, Serialize};

/// Caches the OS is allowed to trim when the reserve is threatened, in order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrimTarget {
    AptCache,
    JournalVacuum,
    OldDeployments,
    Coredumps,
    ThumbnailCache,
}

/// Order in which caches are trimmed when the reserve is threatened.
pub const TRIM_ORDER: [TrimTarget; 5] = [
    TrimTarget::AptCache,
    TrimTarget::ThumbnailCache,
    TrimTarget::Coredumps,
    TrimTarget::JournalVacuum,
    TrimTarget::OldDeployments,
];

/// `org.legeos.Host.Storage1`
#[derive(Debug, Default)]
pub struct StorageApi;

#[zbus::interface(name = "org.legeos.Host.Storage1")]
impl StorageApi {
    /// The trim order as kebab-case names.
    async fn trim_order(&self) -> Vec<String> {
        TRIM_ORDER
            .iter()
            .map(|target| {
                serde_json::to_string(target)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_owned()
            })
            .collect()
    }
}
