//! `lege session` — end, suspend or lock the session (`org.legeos.Session.Lifecycle1`).
//!
//! Every capability the shell offers has to be reachable here too
//! (`docs/architecture/accessibility.md`: "everything the shell shows is available through
//! `lege` too"), and this one doubly so: the CLI is how you end a session whose shell has died.
//!
//! The bus call goes through [`LifecycleClient`] so the rendering below is testable with a
//! fixture rather than a live logind — the same shape `crate::power`'s `PowerClient` uses.

use anyhow::{Context, Result};
use async_trait::async_trait;

/// What `lege session` needs from `org.legeos.Session.Lifecycle1`.
#[async_trait]
pub trait LifecycleClient {
    async fn logout(&self) -> Result<()>;
    async fn reboot(&self) -> Result<()>;
    async fn power_off(&self) -> Result<()>;
    async fn suspend(&self) -> Result<()>;
    async fn lock(&self) -> Result<()>;
    async fn can(&self, action: Action) -> Result<String>;
}

/// The actions with a `Can*` query behind them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Reboot,
    PowerOff,
    Suspend,
}

impl Action {
    pub const ALL: [Action; 3] = [Self::Reboot, Self::PowerOff, Self::Suspend];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reboot => "reboot",
            Self::PowerOff => "poweroff",
            Self::Suspend => "suspend",
        }
    }
}

#[async_trait]
impl LifecycleClient for lege_client::proxies::LifecycleProxy<'_> {
    async fn logout(&self) -> Result<()> {
        self.logout().await.context("Lifecycle1.Logout")
    }
    async fn reboot(&self) -> Result<()> {
        self.reboot().await.context("Lifecycle1.Reboot")
    }
    async fn power_off(&self) -> Result<()> {
        self.power_off().await.context("Lifecycle1.PowerOff")
    }
    async fn suspend(&self) -> Result<()> {
        self.suspend().await.context("Lifecycle1.Suspend")
    }
    async fn lock(&self) -> Result<()> {
        self.lock().await.context("Lifecycle1.Lock")
    }
    async fn can(&self, action: Action) -> Result<String> {
        match action {
            Action::Reboot => self.can_reboot().await.context("Lifecycle1.CanReboot"),
            Action::PowerOff => self.can_power_off().await.context("Lifecycle1.CanPowerOff"),
            Action::Suspend => self.can_suspend().await.context("Lifecycle1.CanSuspend"),
        }
    }
}

/// Turn one of logind's verdicts into a sentence. The four values are genuinely different
/// answers and flattening them would lose the reason.
pub fn render_verdict(action: Action, verdict: &str) -> String {
    let meaning = match verdict {
        "yes" => "available",
        "no" => "refused by policy",
        "na" => "not supported by this machine",
        "challenge" => "available, but would need authentication",
        other => return format!("{:<9} {other} (unrecognised)", action.as_str()),
    };
    format!("{:<9} {meaning}", action.as_str())
}

/// The `lege session status` table.
pub fn render_status(verdicts: &[(Action, String)]) -> String {
    verdicts
        .iter()
        .map(|(action, verdict)| render_verdict(*action, verdict))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_of_loginds_four_verdicts_reads_as_a_different_answer() {
        assert!(render_verdict(Action::PowerOff, "yes").contains("available"));
        assert!(render_verdict(Action::PowerOff, "no").contains("refused by policy"));
        assert!(render_verdict(Action::Suspend, "na").contains("not supported"));
        let challenge = render_verdict(Action::Reboot, "challenge");
        assert!(
            challenge.contains("authentication"),
            "a challenge is not a refusal: {challenge}"
        );
        assert_ne!(
            render_verdict(Action::Reboot, "no"),
            render_verdict(Action::Reboot, "challenge")
        );
    }

    #[test]
    fn an_unrecognised_verdict_is_shown_rather_than_guessed_at() {
        let rendered = render_verdict(Action::Suspend, "maybe");
        assert!(rendered.contains("maybe"));
        assert!(rendered.contains("unrecognised"));
    }

    #[test]
    fn the_status_table_names_every_action() {
        let table = render_status(&[
            (Action::Reboot, "yes".to_owned()),
            (Action::PowerOff, "challenge".to_owned()),
            (Action::Suspend, "na".to_owned()),
        ]);
        for action in Action::ALL {
            assert!(table.contains(action.as_str()), "{table}");
        }
        assert_eq!(table.lines().count(), 3);
    }
}
