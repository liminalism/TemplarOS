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
use lege_types::unit;
use std::sync::Arc;
use zbus::{fdo, object_server::SignalEmitter};

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
///
/// The connection is held as an `Option` and retried lazily, exactly as `apis::location` does,
/// and for a reason learned the hard way. The first version took a live connection, so
/// `Lifecycle1` was registered only `if let Some(system) = system_connection` — one attempt, made
/// while the session was still coming up. Lose that race and the interface was never served *for
/// the whole session*: every power control became a permanent no-op, the only trace was a startup
/// `warn!`, and from the user's chair the shutdown button simply did nothing. An interface that
/// exists and reports "the system bus is unreachable" is strictly better than one that is absent,
/// because the absent one cannot explain itself.
#[derive(Debug)]
pub struct SystemLogind {
    /// `Some` once a system-bus connection has succeeded at least once; zbus connections are
    /// cheap to clone and reconnect on demand.
    connection: parking_lot::Mutex<Option<zbus::Connection>>,
}

impl SystemLogind {
    pub fn new(connection: Option<zbus::Connection>) -> SystemLogind {
        SystemLogind {
            connection: parking_lot::Mutex::new(connection),
        }
    }

    /// The system-bus connection, connecting (or reconnecting) lazily on demand.
    async fn connection(&self) -> Result<zbus::Connection> {
        if let Some(connection) = self.connection.lock().clone() {
            return Ok(connection);
        }
        let connection = zbus::connection::Builder::system()
            .context("preparing a system bus connection")?
            .build()
            .await
            .context("connecting to the system bus to reach logind")?;
        *self.connection.lock() = Some(connection.clone());
        Ok(connection)
    }

    async fn call<B>(&self, method: &str, body: &B) -> Result<zbus::Message>
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        self.connection()
            .await?
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

/// Ending *this* session, as distinct from acting on the machine.
#[async_trait]
pub trait SessionEnd: Send + Sync + std::fmt::Debug {
    /// Stop [`unit::SESSION_TARGET`] on the user manager.
    async fn stop_session_target(&self) -> Result<()>;
}

/// The real thing: `org.freedesktop.systemd1.Manager.StopUnit` on the **session** bus.
#[derive(Debug)]
pub struct UserManagerSession {
    connection: zbus::Connection,
}

impl UserManagerSession {
    pub fn new(connection: zbus::Connection) -> UserManagerSession {
        UserManagerSession { connection }
    }
}

#[async_trait]
impl SessionEnd for UserManagerSession {
    async fn stop_session_target(&self) -> Result<()> {
        // "replace" rather than "fail": a logout must not be refused because some other job is
        // already queued against the target.
        self.connection
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "StopUnit",
                &(unit::SESSION_TARGET, "replace"),
            )
            .await
            .with_context(|| format!("StopUnit({})", unit::SESSION_TARGET))?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct LifecycleApi {
    logind: Arc<dyn Logind>,
    session: Arc<dyn SessionEnd>,
}

impl LifecycleApi {
    pub fn new(logind: Arc<dyn Logind>, session: Arc<dyn SessionEnd>) -> LifecycleApi {
        LifecycleApi { logind, session }
    }
}

#[zbus::interface(name = "org.legeos.Session.Lifecycle1")]
impl LifecycleApi {
    /// End this session by stopping its target, not by terminating its logind session.
    ///
    /// The difference is what the display manager sees. `lege-session` is blocked on
    /// `systemctl --user start --wait legeos-session.target` for the session's whole life, and the
    /// display manager is in turn waiting on `lege-session`. Stop the target and that chain unwinds
    /// in order: the target goes down, `lege-session` returns, runs its cleanup and exits, and the
    /// display manager — seeing its child exit normally — shows its greeter again.
    ///
    /// Asking logind to terminate the session instead sends SIGTERM to everything in the scope *at
    /// once*, including the display manager's own session helper. SDDM reported that as
    /// `sddm-helper ... crashed (exit code 1)` and never started a new greeter, which left a blank
    /// VT and a blinking cursor with no way back except the power button. That is what this method
    /// used to do.
    ///
    /// logind stays as the fallback: if the manager will not stop the target, ending the session
    /// abruptly is still better than a Log out button that does nothing.
    async fn logout(&self) -> fdo::Result<()> {
        match self.session.stop_session_target().await {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "stopping {} failed; falling back to logind",
                    unit::SESSION_TARGET
                );
                self.logind.terminate_session().await.map_err(failed)
            }
        }
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

    /// Ask for the session to be locked.
    ///
    /// This daemon does not lock the screen itself and is not going to: the lock screen is
    /// compositor-owned (ADR-0010) and `ext-session-lock-v1` can only be driven by a Wayland
    /// client, which sessiond deliberately is not. So this emits [`LockRequested`], and
    /// `lege-shell` — which already holds the compositor's trusted-shell connection — is what
    /// calls `lock()` on the protocol.
    ///
    /// Returning `Ok` here means the request went out, not that the screen is covered. That is
    /// honest in a way the old `NotSupported` was not, and it is the only thing this process is
    /// in a position to promise.
    async fn lock(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> fdo::Result<()> {
        emitter
            .lock_requested()
            .await
            .map_err(|error| fdo::Error::Failed(format!("emitting LockRequested: {error}")))
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

    /// Somebody asked for the session to be locked. The trusted shell drives the protocol.
    #[zbus(signal)]
    async fn lock_requested(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
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

    #[derive(Debug, Default)]
    struct FakeSession {
        stops: Mutex<usize>,
        fail: bool,
    }

    #[async_trait]
    impl SessionEnd for FakeSession {
        async fn stop_session_target(&self) -> Result<()> {
            *self.stops.lock().unwrap() += 1;
            if self.fail {
                return Err(anyhow!("no user manager"));
            }
            Ok(())
        }
    }

    fn api(fake: Arc<FakeLogind>) -> LifecycleApi {
        LifecycleApi::new(fake, Arc::new(FakeSession::default()))
    }

    fn api_with(fake: Arc<FakeLogind>, session: Arc<FakeSession>) -> LifecycleApi {
        LifecycleApi::new(fake, session)
    }

    #[tokio::test]
    async fn each_verb_reaches_exactly_one_logind_call() {
        let fake = Arc::new(FakeLogind::default());
        let session = Arc::new(FakeSession::default());
        let api = api_with(Arc::clone(&fake), Arc::clone(&session));
        api.logout().await.unwrap();
        api.reboot().await.unwrap();
        api.power_off().await.unwrap();
        api.suspend().await.unwrap();
        // Logout is not a logind call at all: it stops the session target, so the display manager
        // sees its own child exit cleanly and shows the greeter.
        assert_eq!(*session.stops.lock().unwrap(), 1);
        assert_eq!(
            *fake.calls.lock().unwrap(),
            ["reboot", "power_off", "suspend"]
        );
    }

    /// A Log out button that does nothing is worse than one that ends the session abruptly, so a
    /// manager that will not stop the target still falls through to logind.
    #[tokio::test]
    async fn logout_falls_back_to_logind_when_the_target_will_not_stop() {
        let fake = Arc::new(FakeLogind::default());
        let session = Arc::new(FakeSession {
            fail: true,
            ..FakeSession::default()
        });
        api_with(Arc::clone(&fake), Arc::clone(&session))
            .logout()
            .await
            .unwrap();
        assert_eq!(*session.stops.lock().unwrap(), 1);
        assert_eq!(*fake.calls.lock().unwrap(), ["terminate"]);
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

    /// `Lock` used to refuse: nothing drove the compositor's protocol, so reporting success
    /// would have been a lie. It now emits `LockRequested` and the trusted shell does the
    /// locking — which means the interface has a signal, and this checks the interface declares
    /// it rather than calling `lock()`, whose signal emitter needs a real bus connection.
    #[test]
    fn lock_asks_for_a_lock_through_a_signal_rather_than_refusing() {
        let xml = include_str!("../../../../interfaces/dbus/org.legeos.Session.Lifecycle1.xml");
        assert!(
            xml.contains(r#"<signal name="LockRequested"/>"#),
            "the interface no longer declares the signal Lock emits"
        );
        assert!(
            !xml.contains("NotSupported"),
            "the interface still documents Lock as refusing"
        );
        // The implementation emits it; a `NotSupported` left behind would mean the two halves
        // disagree about what calling Lock does.
        let source = include_str!("lifecycle.rs");
        assert!(source.contains("emitter\n            .lock_requested()"));
    }
}
