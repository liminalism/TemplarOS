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
    /// The terminal emulator the desktop opens: the drop-down chord's target, "Open terminal
    /// here" in the Files object, and whatever else needs *a* terminal rather than one it named.
    Terminal,
    /// The application that searches every file on the machine. `Files.Search1` answers inside
    /// the folder you are looking at; this is where "search everywhere" goes. It is launched
    /// and nothing more: LegeOS neither reads nor feeds whatever index it keeps.
    FileSearch,
}

impl ProviderRole {
    /// Every role, in a stable order (used by `lege doctor` and the settings UI).
    pub const ALL: [ProviderRole; 14] = [
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
        Self::Terminal,
        Self::FileSearch,
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
            Self::Terminal => "terminal",
            Self::FileSearch => "file-search",
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
