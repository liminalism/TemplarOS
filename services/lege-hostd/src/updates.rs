//! Staged system and driver updates as one transaction (planning doc 01 §4).
//!
//! 1. download candidate kernel, NVIDIA modules, userspace, firmware;
//! 2. install into a non-default boot deployment;
//! 3. verify module signatures, version matching, initramfs contents;
//! 4. preserve the complete previous deployment;
//! 5. boot the candidate with a watchdog (systemd boot-assessment counter);
//! 6. `lege-boot-health` verifies module loaded, DRM modeset, compositor started, Vulkan/GL probes;
//! 7. promote only after validation (`systemd-bless-boot`);
//! 8. roll back automatically otherwise.
//!
//! The transaction journal lives on disk (`/var/lib/legeos/hostd/transactions/`) so a restart
//! mid-apt resumes rather than leaving a half-applied system. apt runs as a child transient unit.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// Transaction phases. Transitions are validated by [`Transaction::advance`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Planned,
    Downloading,
    Staging,
    Verifying,
    AwaitingReboot,
    Trial,
    Promoted,
    RolledBack,
    Failed,
}

impl Phase {
    /// The legal next phases from `self`.
    pub fn successors(self) -> &'static [Phase] {
        match self {
            Self::Planned => &[Self::Downloading, Self::Failed],
            Self::Downloading => &[Self::Staging, Self::Failed],
            Self::Staging => &[Self::Verifying, Self::Failed],
            Self::Verifying => &[Self::AwaitingReboot, Self::Failed],
            Self::AwaitingReboot => &[Self::Trial, Self::Failed],
            Self::Trial => &[Self::Promoted, Self::RolledBack],
            Self::Promoted | Self::RolledBack | Self::Failed => &[],
        }
    }

    pub fn is_terminal(self) -> bool {
        self.successors().is_empty()
    }
}

/// What is being changed together (kernel + modules + userspace as one tested set).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub kernel: Option<String>,
    pub nvidia_modules: Option<String>,
    pub nvidia_userspace: Option<String>,
    pub firmware: Vec<String>,
    pub other_packages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub changes: ChangeSet,
    pub phase: Phase,
    /// Deployment identifier of the candidate (snapshot / boot entry).
    pub candidate_deployment: Option<String>,
    pub previous_deployment: Option<String>,
    pub log: Vec<String>,
}

impl Transaction {
    pub fn new(id: impl Into<String>, changes: ChangeSet) -> Self {
        Self {
            id: id.into(),
            changes,
            phase: Phase::Planned,
            candidate_deployment: None,
            previous_deployment: None,
            log: Vec::new(),
        }
    }

    /// Move to `next` if legal, recording why.
    pub fn advance(&mut self, next: Phase, note: impl Into<String>) -> Result<(), String> {
        if !self.phase.successors().contains(&next) {
            return Err(format!("illegal transition {:?} → {:?}", self.phase, next));
        }
        self.log
            .push(format!("{:?} → {:?}: {}", self.phase, next, note.into()));
        self.phase = next;
        Ok(())
    }
}

/// `org.legeos.Host.Updates1`
#[derive(Debug, Default)]
pub struct UpdatesApi {
    current: Mutex<Option<Transaction>>,
}

#[zbus::interface(name = "org.legeos.Host.Updates1")]
impl UpdatesApi {
    /// The current transaction as JSON, or an empty string.
    async fn current(&self) -> String {
        self.current
            .lock()
            .ok()
            .and_then(|current| {
                current
                    .as_ref()
                    .map(|t| serde_json::to_string(t).unwrap_or_default())
            })
            .unwrap_or_default()
    }

    /// Plan a transaction for `changes` (JSON `ChangeSet`). Returns its id. Fails if one is
    /// already in flight and not terminal.
    async fn plan(&self, changes_json: &str) -> zbus::fdo::Result<String> {
        let changes: ChangeSet = serde_json::from_str(changes_json)
            .map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;
        let mut current = self
            .current
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("poisoned".into()))?;
        if let Some(existing) = current.as_ref()
            && !existing.phase.is_terminal()
        {
            return Err(zbus::fdo::Error::Failed(format!(
                "transaction {} is in phase {:?}",
                existing.id, existing.phase
            )));
        }
        let id = format!("txn-{}", std::process::id());
        let mut transaction = Transaction::new(id.clone(), changes);
        transaction
            .advance(Phase::Downloading, "planned via D-Bus")
            .map_err(zbus::fdo::Error::Failed)?;
        *current = Some(transaction);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_and_rollback_are_legal_and_shortcuts_are_not() {
        let mut t = Transaction::new(
            "t1",
            ChangeSet {
                kernel: Some("7.0.0-14-generic".into()),
                ..Default::default()
            },
        );
        for (phase, note) in [
            (Phase::Downloading, "apt-get download"),
            (Phase::Staging, "into snapshot"),
            (Phase::Verifying, "signatures ok"),
            (Phase::AwaitingReboot, "entry +3"),
            (Phase::Trial, "booted"),
        ] {
            t.advance(phase, note).unwrap();
        }
        assert!(t.advance(Phase::Promoted, "bless-boot").is_ok());
        assert!(t.phase.is_terminal());
        assert_eq!(t.log.len(), 6);

        let mut u = Transaction::new("t2", ChangeSet::default());
        assert!(u.advance(Phase::Promoted, "skip everything").is_err());
        u.advance(Phase::Downloading, "").unwrap();
        u.advance(Phase::Failed, "mirror down").unwrap();
        assert!(u.advance(Phase::Staging, "").is_err());
    }
}
