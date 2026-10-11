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
    /// Legacy external-search role retained for stored manifest compatibility. The first-party
    /// launcher owns file discovery; graphical Settings no longer offers this provider choice.
    FileSearch,
    /// The editor a project opens in. Like [`Self::Terminal`] and [`Self::FileManager`], this is
    /// a role rather than a path copied into every project's manifest: "open this project in the
    /// editor" has to keep meaning something after the person changes which editor they use.
    CodeEditor,
}

impl ProviderRole {
    /// Every role, in a stable order (used by `templar doctor` and the settings UI).
    pub const ALL: [ProviderRole; 15] = [
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
        Self::CodeEditor,
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
            Self::CodeEditor => "code-editor",
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
    #[serde(default = "default_capability_priority")]
    pub priority: u32,
}

/// The capability schema's default: a claim that names no priority is a third-party
/// nomination at 50, not a zero that loses to everything including nothing.
fn default_capability_priority() -> u32 {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `interfaces/` is the source of truth, and a role declared in Rust but missing from the
    /// capability schema is a manifest that fails validation for a role the code accepts. That
    /// had already happened to `terminal` and `file-search` before this test existed, so the
    /// two lists are compared rather than trusted to be edited together.
    #[test]
    fn every_role_is_in_the_capability_schema() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../interfaces/schemas/provider-capability.schema.json"
        ))
        .expect("the capability schema is valid JSON");
        let declared: Vec<&str> = schema["properties"]["role"]["enum"]
            .as_array()
            .expect("the role property declares an enum")
            .iter()
            .map(|value| value.as_str().expect("a role is a string"))
            .collect();
        let ours: Vec<&str> = ProviderRole::ALL.iter().map(|role| role.as_str()).collect();
        for role in &ours {
            assert!(declared.contains(role), "{role} is missing from the schema");
        }
        for role in &declared {
            assert!(
                ours.contains(role),
                "{role} is in the schema but not in ALL"
            );
        }
    }

    #[test]
    fn a_capability_round_trips_through_its_kebab_case_name() {
        let json = r#"{"role":"code-editor","priority":100}"#;
        let capability: ProviderCapability = serde_json::from_str(json).expect("parses");
        assert_eq!(capability.role, ProviderRole::CodeEditor);
        assert_eq!(
            serde_json::to_string(&capability).expect("serialises"),
            json
        );
    }

    #[test]
    fn a_capability_without_a_priority_is_a_third_party_nomination() {
        let capability: ProviderCapability =
            serde_json::from_str(r#"{"role":"terminal"}"#).expect("parses");
        assert_eq!(capability.priority, 50);
    }
}
