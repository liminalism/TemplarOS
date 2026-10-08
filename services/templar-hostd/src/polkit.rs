//! Polkit authorization, checked in-process on every privileged method (ADR-0003: "a system-bus
//! daemon that does not check is the whole attack surface"). The caller's unique bus name — read
//! from the D-Bus message header, never trusted from an argument — becomes the
//! `system-bus-name` subject passed to `org.freedesktop.PolicyKit1.Authority.CheckAuthorization`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use zbus::zvariant::{Type, Value};

/// What every privileged method checks before doing anything. Kept as a trait so tests run
/// without a real polkit daemon on the bus.
#[async_trait]
pub trait PolkitAuthority: Send + Sync + std::fmt::Debug {
    /// `true` iff `sender` (a unique bus name, e.g. `:1.42`) is authorized for `action_id`.
    async fn check_authorization(&self, sender: &str, action_id: &str) -> Result<bool>;
    async fn check_authorization_interactive(&self, sender: &str, action_id: &str) -> Result<bool> {
        self.check_authorization(sender, action_id).await
    }
}

/// Calls the real `org.freedesktop.PolicyKit1.Authority` over the system bus the daemon is
/// already connected to.
#[derive(Debug, Clone)]
pub struct SystemPolkit {
    connection: zbus::Connection,
}

impl SystemPolkit {
    pub fn new(connection: zbus::Connection) -> Self {
        Self { connection }
    }
}

/// `(kind, details)` — the `(sa{sv})` `Subject` struct polkit's `CheckAuthorization` expects.
#[derive(Type, serde::Serialize)]
struct Subject<'a> {
    kind: &'a str,
    details: HashMap<&'a str, Value<'a>>,
}

/// `(is_authorized, is_challenge, details)` — the `(bba{ss})` result struct.
#[derive(Type, serde::Deserialize)]
struct AuthorizationResult {
    is_authorized: bool,
    #[allow(dead_code)]
    is_challenge: bool,
    #[allow(dead_code)]
    details: HashMap<String, String>,
}

impl SystemPolkit {
    async fn check(&self, sender: &str, action_id: &str, flags: u32) -> Result<bool> {
        let mut details = HashMap::new();
        details.insert("name", Value::from(sender));
        let subject = Subject { kind: "system-bus-name", details };
        let empty_details: HashMap<&str, &str> = HashMap::new();
        let reply = self.connection.call_method(
            Some("org.freedesktop.PolicyKit1"), "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"), "CheckAuthorization",
            &(subject, action_id, empty_details, flags, ""),
        ).await.context("calling PolicyKit1.Authority.CheckAuthorization")?;
        let result: AuthorizationResult = reply.body().deserialize()?;
        Ok(result.is_authorized)
    }
}

#[async_trait]
impl PolkitAuthority for SystemPolkit {
    async fn check_authorization(&self, sender: &str, action_id: &str) -> Result<bool> {
        // Ordinary headless operations retain their existing noninteractive contract.
        self.check(sender, action_id, 0).await
    }
    async fn check_authorization_interactive(&self, sender: &str, action_id: &str) -> Result<bool> {
        self.check(sender, action_id, 1).await
    }
}

/// Grants everything — tests that only exercise the mechanism above polkit. Deliberately not
/// available outside `#[cfg(test)]`: an "allow everything" `PolkitAuthority` must never be a
/// production build option.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[cfg(test)]
#[async_trait]
impl PolkitAuthority for AllowAll {
    async fn check_authorization(&self, _sender: &str, _action_id: &str) -> Result<bool> {
        Ok(true)
    }
}

/// Denies everything, or everything except a named allow-list — for testing that a method
/// actually calls through to authorization before doing privileged work.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ScriptedPolkit {
    pub allowed_actions: std::collections::HashSet<String>,
}

#[cfg(test)]
impl ScriptedPolkit {
    pub fn allow(action_id: impl Into<String>) -> Self {
        let mut allowed_actions = std::collections::HashSet::new();
        allowed_actions.insert(action_id.into());
        Self { allowed_actions }
    }
}

#[cfg(test)]
#[async_trait]
impl PolkitAuthority for ScriptedPolkit {
    async fn check_authorization(&self, _sender: &str, action_id: &str) -> Result<bool> {
        Ok(self.allowed_actions.contains(action_id))
    }
}

/// The one place a D-Bus method turns "not authorized" into the `fdo` error every caller expects.
/// `action` is the bare action id (`update`, `deploy`, `driver`, `storage`, `hardware`); it is
/// joined with [`templar_types::dbus::host::POLKIT_ACTION_PREFIX`].
pub async fn require_authorized(
    authority: &dyn PolkitAuthority,
    header: &zbus::message::Header<'_>,
    action: &str,
) -> zbus::fdo::Result<()> {
    let Some(sender) = header.sender() else {
        return Err(zbus::fdo::Error::AccessDenied(
            "no sender on message (peer-to-peer connection?)".into(),
        ));
    };
    let action_id = format!(
        "{}.{action}",
        templar_types::dbus::host::POLKIT_ACTION_PREFIX
    );
    let authorized = authority
        .check_authorization(sender.as_str(), &action_id)
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    if authorized {
        Ok(())
    } else {
        Err(zbus::fdo::Error::AccessDenied(format!(
            "{sender} is not authorized for {action_id}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allow_all_authorizes_everything() {
        assert!(
            AllowAll
                .check_authorization(":1.1", "org.templaros.host.update")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn scripted_polkit_only_authorizes_the_listed_action() {
        let scripted = ScriptedPolkit::allow("org.templaros.host.update");
        assert!(
            scripted
                .check_authorization(":1.1", "org.templaros.host.update")
                .await
                .unwrap()
        );
        assert!(
            !scripted
                .check_authorization(":1.1", "org.templaros.host.deploy")
                .await
                .unwrap()
        );
    }
}
