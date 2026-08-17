//! Exclusive system roles and the providers that may fill them.
//!
//! Installing a second provider must never silently start both (the `nm-applet` + `nm-tray`
//! problem). Exactly one selected provider per role is allowed to autostart.

use serde::{Deserialize, Serialize};
use std::fmt;

/// An exclusive system role. Exactly one selected provider per role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRole {
    NetworkStatusUi,
    NotificationServer,
    FileChooser,
    FileManager,
    LockScreen,
    AssociationBroker,
    ClipboardManager,
    PowerManager,
    ScreenshotService,
    InputMethod,
    PortalBackend,
    Greeter,
}

impl ProviderRole {
    /// Every role, in a stable order (used by `lege doctor` and the settings UI).
    pub const ALL: [ProviderRole; 12] = [
        Self::NetworkStatusUi,
        Self::NotificationServer,
        Self::FileChooser,
        Self::FileManager,
        Self::LockScreen,
        Self::AssociationBroker,
        Self::ClipboardManager,
        Self::PowerManager,
        Self::ScreenshotService,
        Self::InputMethod,
        Self::PortalBackend,
        Self::Greeter,
    ];

    /// Kebab-case name as used in manifests and on the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NetworkStatusUi => "network-status-ui",
            Self::NotificationServer => "notification-server",
            Self::FileChooser => "file-chooser",
            Self::FileManager => "file-manager",
            Self::LockScreen => "lock-screen",
            Self::AssociationBroker => "association-broker",
            Self::ClipboardManager => "clipboard-manager",
            Self::PowerManager => "power-manager",
            Self::ScreenshotService => "screenshot-service",
            Self::InputMethod => "input-method",
            Self::PortalBackend => "portal-backend",
            Self::Greeter => "greeter",
        }
    }
}

impl fmt::Display for ProviderRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A declaration, from an application manifest or autostart entry, that the application can
/// fill an exclusive role.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCapability {
    pub role: ProviderRole,
    /// Higher wins when the user has not chosen; distribution defaults use 100, third parties 50.
    #[serde(default)]
    pub priority: u32,
}
