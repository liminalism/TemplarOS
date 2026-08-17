//! Typed overlay layers. Instead of arbitrary "always on top", z-order is explainable and
//! overlay conflicts are impossible by construction (planning doc 01 §3).

use serde::{Deserialize, Serialize};

/// Compositor layers from top-most to bottom-most.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Layer {
    /// Secure lock and authentication (`ext-session-lock-v1`).
    SecureLock = 0,
    /// Emergency system recovery (resource overlay, "show every window").
    Recovery = 1,
    /// System-modal requests (polkit, permission prompts).
    SystemModal = 2,
    /// Launcher, switcher and transient shell overlays.
    ShellOverlay = 3,
    /// Notifications.
    Notification = 4,
    /// Application always-on-top surfaces.
    AppOnTop = 5,
    /// Normal windows.
    Normal = 6,
    /// The desktopless Home Surface — visible wherever no window is.
    HomeSurface = 7,
}

impl Layer {
    /// All layers, top-most first.
    pub const ALL: [Layer; 8] = [
        Self::SecureLock,
        Self::Recovery,
        Self::SystemModal,
        Self::ShellOverlay,
        Self::Notification,
        Self::AppOnTop,
        Self::Normal,
        Self::HomeSurface,
    ];

    /// Layers only the compositor core itself may create surfaces on.
    pub fn is_core_only(self) -> bool {
        matches!(self, Self::SecureLock | Self::Recovery)
    }
}
