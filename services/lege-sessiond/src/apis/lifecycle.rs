//! `org.legeos.Session.Lifecycle1`: ending, suspending and locking the session.
//!
//! Every method forwards to `org.freedesktop.login1`, which already authorises these for the
//! active seat. sessiond adds no privilege of its own and needs no polkit action — see the
//! interface XML for why this is here and not in `lege-hostd`.
//!
//! logind lives behind [`Logind`] so the whole interface is testable without a system bus, a
//! seat, or the ability to actually reboot the machine running the tests — the same shape
//! `cli/lege/src/power.rs`'s `PowerClient` uses on the other side of the wire.

use crate::apis::failed;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use zbus::fdo;

/// What this interface needs from logind.
#[async_trait]
pub trait Logind: Send + Sync + std::fmt::Debug {
    /// `Manager.TerminateSession(session_id)` for this session, or `"auto"` for the caller's.
    async fn terminate_session(&self) -> Result<()>;
    async fn reboot(&self) -> Result<()>;
    async fn power_off(&self) -> Result<()>;
    async fn suspend(&self) -> Result<()>;
    /// One of logind's `"yes" | "no" | "na" | "challenge"`.
    async fn can(&self, what: Action) -> Result<String>;
}

/// The three actions logind will answer a `Can*` question about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Reboot,
    PowerOff,
    Suspend,
}

impl Action {
    /// logind's method names, which are `Can` + the action.
    fn can_method(self) -> &'static str {
        match self {
            Self::Reboot => "CanReboot",
            Self::PowerOff => "CanPowerOff",
            Self::Suspend => "CanSuspend",
        }
    }
}

/// The real thing: `org.freedesktop.login1.Manager` on the system bus.
#[derive(Debug)]
pub struct SystemLogind {
    connection: zbus::Connection,
}

impl SystemLogind {
    pub fn new(connection: zbus::Connection) -> SystemLogind {
        SystemLogind { connection }
    }

    async fn call<B>(&self, method: &str, body: &B) -> Result<zbus::Message>
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        self.connection
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                method,
                body,
            )
            .await
            .with_context(|| format!("login1.Manager.{method}"))
    }
}

#[async_trait]
impl Logind for SystemLogind {
    async fn terminate_session(&self) -> Result<()> {
        // "auto" is logind's own name for "the session the caller is in", so nothing has to
        // discover or carry a session id.
        self.call("TerminateSession", &("auto",)).await?;
        Ok(())
    }

    async fn reboot(&self) -> Result<()> {
        // `false` is logind's `interactive` flag: never ask a polkit agent from here. If policy
        // says no, the call fails and the surface reports it, rather than a password dialog
        // appearing behind the Home Field where nothing can focus it.
        self.call("Reboot", &(false,)).await?;
        Ok(())
    }

    async fn power_off(&self) -> Result<()> {
        self.call("PowerOff", &(false,)).await?;
        Ok(())
    }

    async fn suspend(&self) -> Result<()> {
        self.call("Suspend", &(false,)).await?;
        Ok(())
    }

    async fn can(&self, what: Action) -> Result<String> {
        let reply = self.call(what.can_method(), &()).await?;
        reply
            .body()
            .deserialize::<String>()
            .map_err(|error| anyhow!("{}: {error}", what.can_method()))
    }
}

#[derive(Debug)]
pub struct LifecycleApi {
    logind: Arc<dyn Logind>,
}

impl LifecycleApi {
    pub fn new(logind: Arc<dyn Logind>) -> LifecycleApi {
        LifecycleApi { logind }
    }
}

#[zbus::interface(name = "org.legeos.Session.Lifecycle1")]
impl LifecycleApi {
    async fn logout(&self) -> fdo::Result<()> {
        self.logind.terminate_session().await.map_err(failed)
    }

    async fn reboot(&self) -> fdo::Result<()> {
        self.logind.reboot().await.map_err(failed)
    }

    async fn power_off(&self) -> fdo::Result<()> {
        self.logind.power_off().await.map_err(failed)
    }

    async fn suspend(&self) -> fdo::Result<()> {
        self.logind.suspend().await.map_err(failed)
    }

    /// Declared, not implemented. The compositor owns the lock screen (ADR-0010) and implements
    /// `ext-session-lock-v1` fully, but nothing drives it: sessiond would have to become a
    /// Wayland lock-manager client first. Refusing plainly is better than a method that returns
    /// success and leaves the screen unlocked.
    async fn lock(&self) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "locking needs a lock-manager client for the compositor's ext-session-lock-v1; \
             see interfaces/dbus/org.legeos.Session.Lifecycle1.xml"
                .to_owned(),
        ))
    }

    async fn can_reboot(&self) -> fdo::Result<String> {
        self.logind.can(Action::Reboot).await.map_err(failed)
    }

    async fn can_power_off(&self) -> fdo::Result<String> {
        self.logind.can(Action::PowerOff).await.map_err(failed)
    }

    async fn can_suspend(&self) -> fdo::Result<String> {
        self.logind.can(Action::Suspend).await.map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct FakeLogind {
        calls: Mutex<Vec<String>>,
        answer: String,
        fail: bool,
    }

    #[async_trait]
    impl Logind for FakeLogind {
        async fn terminate_session(&self) -> Result<()> {
            self.record("terminate")
        }
        async fn reboot(&self) -> Result<()> {
            self.record("reboot")
        }
        async fn power_off(&self) -> Result<()> {
            self.record("power_off")
        }
        async fn suspend(&self) -> Result<()> {
            self.record("suspend")
        }
        async fn can(&self, what: Action) -> Result<String> {
            self.calls.lock().unwrap().push(format!("can:{what:?}"));
            Ok(self.answer.clone())
        }
    }

    impl FakeLogind {
        fn record(&self, what: &str) -> Result<()> {
            self.calls.lock().unwrap().push(what.to_owned());
            if self.fail {
                return Err(anyhow!("logind said no"));
            }
            Ok(())
        }
    }

    fn api(fake: Arc<FakeLogind>) -> LifecycleApi {
        LifecycleApi::new(fake)
    }

    #[tokio::test]
    async fn each_verb_reaches_exactly_one_logind_call() {
        let fake = Arc::new(FakeLogind::default());
        let api = api(Arc::clone(&fake));
        api.logout().await.unwrap();
        api.reboot().await.unwrap();
        api.power_off().await.unwrap();
        api.suspend().await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            ["terminate", "reboot", "power_off", "suspend"]
        );
    }

    #[tokio::test]
    async fn a_refusal_from_logind_is_reported_rather_than_swallowed() {
        let fake = Arc::new(FakeLogind {
            fail: true,
            ..FakeLogind::default()
        });
        let error = api(fake).power_off().await.unwrap_err();
        assert!(
            error.to_string().contains("logind said no"),
            "the reason must survive to the caller: {error}"
        );
    }

    #[tokio::test]
    async fn can_questions_pass_loginds_own_vocabulary_through_unflattened() {
        // "challenge" is not "no": it means authentication would be required. Flattening it to a
        // boolean would lose the difference a surface needs to explain itself.
        let fake = Arc::new(FakeLogind {
            answer: "challenge".to_owned(),
            ..FakeLogind::default()
        });
        let api = api(Arc::clone(&fake));
        assert_eq!(api.can_power_off().await.unwrap(), "challenge");
        assert_eq!(api.can_reboot().await.unwrap(), "challenge");
        assert_eq!(api.can_suspend().await.unwrap(), "challenge");
        assert_eq!(
            *fake.calls.lock().unwrap(),
            ["can:PowerOff", "can:Reboot", "can:Suspend"]
        );
    }

    #[tokio::test]
    async fn lock_refuses_plainly_instead_of_reporting_a_success_it_did_not_achieve() {
        let error = api(Arc::new(FakeLogind::default()))
            .lock()
            .await
            .unwrap_err();
        assert!(matches!(error, fdo::Error::NotSupported(_)), "{error:?}");
    }
}
