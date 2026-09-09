//! The session→hostd health report (`org.templaros.Host.Session1`) — the only path that writes
//! `/run/templaros/{compositor-active,session-healthy}`, the two markers `templar-boot-health` reads
//! (`services/templar-boot-health/src/main.rs`).
//!
//! This is deliberately **not** a polkit-gated administrative action. `auth_admin` semantics fit
//! "should this human be allowed to change privileged system state" — they do not fit "is this
//! specifically `templar-sessiond` calling", and `templar-sessiond` runs unprivileged (ARCHITECTURE.md:
//! user scope), so gating on `auth_admin` would either block the legitimate caller outright or
//! require an interactive password prompt on every boot, which is absurd for an automated report
//! that has to happen before anyone can log in to type a password.
//!
//! Instead the caller's *identity* is verified directly, which is the property that actually
//! matters here: hostd asks the bus who is calling (`org.freedesktop.DBus.GetConnectionUnixProcessID`
//! on the message header's sender — never a self-declared argument) and confirms that PID's
//! cgroup names `templar-sessiond.service`, the one unit ADR-0001 designates as the session's single
//! policy daemon. An unprivileged process cannot manufacture a matching cgroup for itself — cgroup
//! membership is assigned by systemd/the kernel when a unit starts, not something a process can
//! self-declare — so an arbitrary peer on the bus cannot forge a healthy-session report by simply
//! calling the method. The residual trust boundary (a compromised `templar-sessiond` itself could
//! lie) is unavoidable for any "the designated reporter is believed" design and is no different
//! from, say, trusting `systemd-bless-boot` itself.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::fs;

/// What every privileged-by-identity method needs: "is `sender` running inside `unit_name`'s
/// cgroup?" A trait so tests never need a real bus connection or a real systemd unit.
#[async_trait]
pub trait CallerIdentity: Send + Sync + std::fmt::Debug {
    async fn is_unit(&self, sender: &str, unit_name: &str) -> Result<bool>;
}

/// Resolves `sender`'s PID over the bus and checks its real `/proc/<pid>/cgroup`.
#[derive(Debug, Clone)]
pub struct SystemCallerIdentity {
    connection: zbus::Connection,
}

impl SystemCallerIdentity {
    pub fn new(connection: zbus::Connection) -> Self {
        Self { connection }
    }
}

#[async_trait]
impl CallerIdentity for SystemCallerIdentity {
    async fn is_unit(&self, sender: &str, unit_name: &str) -> Result<bool> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection)
            .await
            .context("connecting to org.freedesktop.DBus")?;
        let bus_name = zbus::names::BusName::try_from(sender)
            .with_context(|| format!("{sender} is not a valid bus name"))?;
        let pid = dbus
            .get_connection_unix_process_id(bus_name)
            .await
            .context("GetConnectionUnixProcessID")?;
        let cgroup = read_cgroup(pid)?;
        Ok(cgroup_names_unit(&cgroup, unit_name))
    }
}

fn read_cgroup(pid: u32) -> Result<String> {
    let path = format!("/proc/{pid}/cgroup");
    fs::read_to_string(&path).with_context(|| format!("reading {path}"))
}

/// Pure so it is testable with literal fixture strings rather than a real `/proc`.
fn cgroup_names_unit(cgroup_content: &str, unit_name: &str) -> bool {
    cgroup_content.lines().any(|line| line.contains(unit_name))
}

/// A scripted identity for tests — no bus connection, no `/proc`.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FixedCallerIdentity {
    pub result: std::sync::Mutex<Option<bool>>,
}

#[cfg(test)]
impl FixedCallerIdentity {
    pub fn allow() -> Self {
        Self {
            result: std::sync::Mutex::new(Some(true)),
        }
    }
    pub fn deny() -> Self {
        Self {
            result: std::sync::Mutex::new(Some(false)),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl CallerIdentity for FixedCallerIdentity {
    async fn is_unit(&self, _sender: &str, _unit_name: &str) -> Result<bool> {
        Ok(self.result.lock().expect("poisoned").unwrap_or(false))
    }
}

/// The one place a `Session1` method turns "not the session" into the `fdo` error every caller
/// gets — mirrors [`crate::polkit::require_authorized`]'s shape without going through polkit.
pub async fn require_session_caller(
    identity: &dyn CallerIdentity,
    header: &zbus::message::Header<'_>,
) -> zbus::fdo::Result<()> {
    const SESSIOND_UNIT: &str = "templar-sessiond.service";
    let Some(sender) = header.sender() else {
        return Err(zbus::fdo::Error::AccessDenied(
            "no sender on message (peer-to-peer connection?)".into(),
        ));
    };
    let is_sessiond = identity
        .is_unit(sender.as_str(), SESSIOND_UNIT)
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    if is_sessiond {
        Ok(())
    } else {
        Err(zbus::fdo::Error::AccessDenied(format!(
            "{sender} is not running as {SESSIOND_UNIT}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_names_unit_matches_the_unit_line() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/session.slice/templar-sessiond.service\n";
        assert!(cgroup_names_unit(cgroup, "templar-sessiond.service"));
        assert!(!cgroup_names_unit(cgroup, "templar-comp.service"));
        assert!(!cgroup_names_unit(
            "0::/user.slice/user-1000.slice/app.slice/some-other-app.service\n",
            "templar-sessiond.service"
        ));
    }

    #[tokio::test]
    async fn fixed_identity_allows_and_denies_as_scripted() {
        assert!(
            FixedCallerIdentity::allow()
                .is_unit(":1.1", "templar-sessiond.service")
                .await
                .unwrap()
        );
        assert!(
            !FixedCallerIdentity::deny()
                .is_unit(":1.1", "templar-sessiond.service")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn require_session_caller_rejects_a_message_with_no_sender() {
        // A header built without a sender field (e.g. peer-to-peer) must be rejected, not treated
        // as "no identity to check".
        let message = zbus::Message::method_call("/org/templaros/Host", "Report")
            .unwrap()
            .destination("org.templaros.Host")
            .unwrap()
            .interface("org.templaros.Host.Session1")
            .unwrap()
            .build(&())
            .unwrap();
        let header = message.header();
        let identity = FixedCallerIdentity::allow();
        assert!(require_session_caller(&identity, &header).await.is_err());
    }
}
