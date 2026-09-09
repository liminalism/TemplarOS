//! How much a resolved identity can be trusted.

use serde::{Deserialize, Serialize};

/// Confidence attached to an identity resolution. Privileged actions require
/// [`IdentityConfidence::VerifiedLaunch`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum IdentityConfidence {
    /// Nothing matched.
    Unknown = 0,

    /// A unique manifest matched only the application's own claim. Useful for legacy/manual
    /// launches, but not trusted for privileged actions.
    ClaimOnly = 1,

    /// The PID belongs to a registered launch cgroup.
    VerifiedLaunch = 2,

    /// The cgroup proves the application, but the window claim disagrees.
    VerifiedLaunchClaimMismatch = 3,
}

impl IdentityConfidence {
    /// Decode the wire representation used on D-Bus.
    pub fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::ClaimOnly,
            2 => Self::VerifiedLaunch,
            3 => Self::VerifiedLaunchClaimMismatch,
            _ => Self::Unknown,
        }
    }

    /// Whether the cgroup proved the application (regardless of claim agreement).
    pub fn is_verified(self) -> bool {
        matches!(
            self,
            Self::VerifiedLaunch | Self::VerifiedLaunchClaimMismatch
        )
    }
}
