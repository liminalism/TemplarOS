//! `org.templaros.Session.Lifecycle1`: ending, suspending and locking the session.
//!
//! Every method forwards to `org.freedesktop.login1`, which already authorises these for the
//! active seat. sessiond adds no privilege of its own and needs no polkit action — see the
//! interface XML for why this is here and not in `templar-hostd`.
//!
//! logind lives behind [`Logind`] so the whole interface is testable without a system bus, a
//! seat, or the ability to actually reboot the machine running the tests — the same shape
//! `cli/templar/src/power.rs`'s `PowerClient` uses on the other side of the wire.

use crate::apis::failed;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::StreamExt as _;
use std::sync::Arc;
use templar_types::{dbus::session as names, unit};
use tokio::sync::{broadcast, watch};
use zbus::{fdo, object_server::SignalEmitter};

/// One edge of the session's own life, as distinct from the machine's.
///
/// Something has to be able to count *session* time: a laptop asleep for a week aged nothing, a
/// screen locked overnight aged nothing, and anything that grows with use (the Home garden above
/// all) is wrong if it counts wall-clock seconds instead. Nothing in this daemon knew any of these
/// four facts before — there was no logind signal match anywhere in the repository — so this is
/// where they enter the process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionEdge {
    /// templar-sessiond started, so this is a new session.
    Started,
    /// The trusted shell observed the compositor's per-output protected-frame acknowledgment.
    Locked,
    /// The shell released the compositor's lock and said so through `NotifyUnlocked`.
    Unlocked,
    /// logind's `PrepareForSleep`: `true` on the way down, `false` on the way back.
    Sleeping(bool),
    /// The compositor turned the screen off for want of input (`true`) or back on (`false`),
    /// reported through `NotifyIdle`. An unlocked session nobody is touching is not being used.
    Idle(bool),
}

impl SessionEdge {
    /// Emit this edge as its `Lifecycle1` signal. One place converts edges to signals, so no
    /// edge can reach the bus by two routes and arrive twice.
    async fn emit(self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        match self {
            Self::Started => LifecycleApi::session_started(emitter).await,
            Self::Locked => LifecycleApi::locked(emitter).await,
            Self::Unlocked => LifecycleApi::unlocked(emitter).await,
            Self::Sleeping(going_to_sleep) => LifecycleApi::sleeping(emitter, going_to_sleep).await,
            Self::Idle(idle) => LifecycleApi::idle(emitter, idle).await,
        }
    }
}

/// The in-process fan-out of [`SessionEdge`]s.
///
/// `send` fails only when nobody is subscribed, which is the ordinary state of a session with no
/// garden host running, so every send here is a deliberate `let _ =`.
pub type SessionEdges = broadcast::Sender<SessionEdge>;

/// How many edges a slow subscriber may fall behind before it is told it lagged. Session edges
/// arrive at human speed — a handful an hour — so this is a lifetime's worth of headroom.
pub const EDGE_CAPACITY: usize = 64;

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

/// The systemd unit a bus caller is running in.
///
/// `apis::providers::CallerVariant` answers a different question — which *registered launch
/// instance* owns the caller — and answers `None` for the shell, which is a session unit and was
/// never brokered through the registry. `NotifyUnlocked` needs the unit itself.
#[async_trait]
pub trait CallerUnit: Send + Sync + std::fmt::Debug {
    /// The `*.service` unit behind `sender`, or `None` when it cannot be established. `None` is a
    /// refusal, never a pass.
    async fn unit_of(&self, sender: &str) -> Option<String>;
}

/// The real one: bus name -> PID -> unified cgroup -> the last `.service` component of the path.
#[derive(Debug)]
pub struct PeerCgroupUnit {
    connection: zbus::Connection,
}

impl PeerCgroupUnit {
    pub fn new(connection: zbus::Connection) -> PeerCgroupUnit {
        PeerCgroupUnit { connection }
    }
}

#[async_trait]
impl CallerUnit for PeerCgroupUnit {
    async fn unit_of(&self, sender: &str) -> Option<String> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await.ok()?;
        let bus_name = zbus::names::BusName::try_from(sender).ok()?;
        let pid = dbus.get_connection_unix_process_id(bus_name).await.ok()?;
        let cgroup = templar_appd::identity::unified_cgroup_of(pid).ok()?;
        // The innermost `.service` is the unit the process actually runs in; everything above it
        // is slices (`user.slice`, `user@1000.service`, `app.slice`).
        cgroup
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .rfind(|name| name.ends_with(".service"))
            .map(ToOwned::to_owned)
    }
}

/// The one unit allowed to report an unlock: the trusted shell, which is the only process that
/// calls `ext_session_lock_v1.unlock_and_destroy` (ADR-0010) and so the only one that knows.
const SHELL_UNIT: &str = "templar-shell.service";
const COMPOSITOR_UNIT: &str = "templar-comp.service";

#[derive(Debug)]
pub struct LifecycleApi {
    logind: Arc<dyn Logind>,
    session: Arc<dyn SessionEnd>,
    caller: Arc<dyn CallerUnit>,
    edges: SessionEdges,
}

impl LifecycleApi {
    pub fn new(
        logind: Arc<dyn Logind>,
        session: Arc<dyn SessionEnd>,
        caller: Arc<dyn CallerUnit>,
        edges: SessionEdges,
    ) -> LifecycleApi {
        LifecycleApi {
            logind,
            session,
            caller,
            edges,
        }
    }
}

/// Own the sole logind stream and retain its actual registration/lifetime witness.
#[derive(Debug)]
pub struct SleepWatch {
    ready: watch::Receiver<bool>,
    state: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SleepWatch {
    pub fn readiness(&self) -> watch::Receiver<bool> {
        self.ready.clone()
    }
}

impl Drop for SleepWatch {
    fn drop(&mut self) {
        self.state.send_replace(false);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Watch logind for `PrepareForSleep`. One match rule, no polling, no wakeups of its own.
///
/// No system bus or a lost stream leaves sleep-delay capability unavailable.
pub fn spawn_sleep_watch(
    system_connection: Option<zbus::Connection>,
    edges: SessionEdges,
) -> SleepWatch {
    let (state, ready) = watch::channel(false);
    let mut owner = SleepWatch {
        ready,
        state,
        task: None,
    };
    let Some(connection) = system_connection else {
        tracing::warn!("no system bus; sleep will not be counted against session time");
        return owner;
    };
    let state = owner.state.clone();
    owner.task = Some(tokio::spawn(async move {
        let rule = match zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.login1")
            .and_then(|builder| builder.interface("org.freedesktop.login1.Manager"))
            .and_then(|builder| builder.member("PrepareForSleep"))
        {
            Ok(builder) => builder.build(),
            Err(error) => {
                tracing::warn!(%error, "building the PrepareForSleep match rule");
                return;
            }
        };
        let mut stream = match zbus::MessageStream::for_match_rule(rule, &connection, Some(4)).await
        {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "watching logind for PrepareForSleep");
                return;
            }
        };
        state.send_replace(true);
        while let Some(message) = stream.next().await {
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(%error, "logind sleep stream failed");
                    break;
                }
            };
            match message.body().deserialize::<bool>() {
                Ok(going_to_sleep) => {
                    let _ = edges.send(SessionEdge::Sleeping(going_to_sleep));
                }
                Err(error) => tracing::warn!(%error, "invalid logind sleep signal"),
            }
        }
        state.send_replace(false);
        tracing::warn!("logind sleep stream ended; sleep-delay capability unavailable");
    }));
    owner
}

/// Re-emit every session edge as its `Lifecycle1` signal.
///
/// Subscribing here rather than emitting from each source is what keeps the two halves honest:
/// the broadcast is the record, and the bus signals are a view of it, so a client watching the
/// bus and the garden host counting in-process can never disagree about what happened.
pub fn spawn_signal_relay(connection: zbus::Connection, edges: &SessionEdges) {
    let mut receiver = edges.subscribe();
    tokio::spawn(async move {
        let emitter = match SignalEmitter::new(&connection, names::LIFECYCLE_PATH) {
            Ok(emitter) => emitter,
            Err(error) => {
                tracing::warn!(%error, "preparing the Lifecycle1 signal emitter");
                return;
            }
        };
        loop {
            match receiver.recv().await {
                Ok(edge) => {
                    if let Err(error) = edge.emit(&emitter).await {
                        tracing::warn!(%error, ?edge, "emitting a session edge");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "the session-edge relay fell behind");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

#[zbus::interface(name = "org.templaros.Session.Lifecycle1")]
impl LifecycleApi {
    /// End this session by stopping its target, not by terminating its logind session.
    ///
    /// The difference is what the display manager sees. `templar-session` is blocked on
    /// `systemctl --user start --wait templaros-session.target` for the session's whole life, and the
    /// display manager is in turn waiting on `templar-session`. Stop the target and that chain unwinds
    /// in order: the target goes down, `templar-session` returns, runs its cleanup and exits, and the
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
    /// `templar-shell` — which already holds the compositor's trusted-shell connection — is what
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

    /// Only the trusted lock client can observe ext-session-lock-v1's presented-frame barrier.
    async fn notify_locked(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        let sender = header.sender().map(ToString::to_string).unwrap_or_default();
        if self.caller.unit_of(&sender).await.as_deref() != Some(SHELL_UNIT) {
            return Err(fdo::Error::AccessDenied(format!(
                "only {SHELL_UNIT} may report a confirmed lock"
            )));
        }
        let _ = self.edges.send(SessionEdge::Locked);
        Ok(())
    }

    /// The trusted shell reporting that it has released the compositor's lock.
    ///
    /// `unlock_and_destroy` is the only thing that clears an `ext-session-lock-v1` lock and
    /// `templar-shell` is the only process that calls it, so `templar-shell` is the only process that
    /// can know. Anything else is refused — not as a privilege boundary (the session bus is
    /// already that, and `Logout` next door is unguarded) but because a stray caller would
    /// corrupt the session-time record for everything downstream of it.
    async fn notify_unlocked(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        let sender = header.sender().map(ToString::to_string).unwrap_or_default();
        match self.caller.unit_of(&sender).await {
            Some(unit) if unit == SHELL_UNIT => {
                let _ = self.edges.send(SessionEdge::Unlocked);
                Ok(())
            }
            other => {
                tracing::warn!(
                    caller = %sender,
                    unit = other.as_deref().unwrap_or("<unidentifiable>"),
                    "refusing NotifyUnlocked: only {SHELL_UNIT} knows the lock is gone"
                );
                Err(fdo::Error::AccessDenied(format!(
                    "only {SHELL_UNIT} may report an unlock"
                )))
            }
        }
    }

    /// The compositor reporting that it turned the screen off for want of input (`true`), or
    /// back on at the next input (`false`). It is the only process that sees the seat, so it is
    /// the only one that may say so; anything else would stop or start session time at will.
    async fn notify_idle(
        &self,
        idle: bool,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        let sender = header.sender().map(ToString::to_string).unwrap_or_default();
        match self.caller.unit_of(&sender).await {
            Some(unit) if unit == COMPOSITOR_UNIT => {
                let _ = self.edges.send(SessionEdge::Idle(idle));
                Ok(())
            }
            other => {
                tracing::warn!(
                    caller = %sender,
                    unit = other.as_deref().unwrap_or("<unidentifiable>"),
                    "refusing NotifyIdle: only {COMPOSITOR_UNIT} sees the seat"
                );
                Err(fdo::Error::AccessDenied(format!(
                    "only {COMPOSITOR_UNIT} may report idleness"
                )))
            }
        }
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

    /// This session began. Emitted once, by [`spawn_signal_relay`], never from a method.
    #[zbus(signal)]
    async fn session_started(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// A lock was asked for.
    #[zbus(signal)]
    async fn locked(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// The shell released the compositor's lock.
    #[zbus(signal)]
    async fn unlocked(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// logind is putting the machine to sleep (`true`) or has brought it back (`false`).
    #[zbus(signal)]
    async fn sleeping(signal_emitter: &SignalEmitter<'_>, going_to_sleep: bool)
    -> zbus::Result<()>;

    /// The compositor turned the screen off for want of input (`true`) or back on (`false`).
    #[zbus(signal)]
    async fn idle(signal_emitter: &SignalEmitter<'_>, idle: bool) -> zbus::Result<()>;
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

    /// A caller whose unit is whatever the test says it is — including nothing, which is what an
    /// unidentifiable caller looks like.
    #[derive(Debug)]
    struct FakeUnit(Option<&'static str>);

    #[async_trait]
    impl CallerUnit for FakeUnit {
        async fn unit_of(&self, _sender: &str) -> Option<String> {
            self.0.map(ToOwned::to_owned)
        }
    }

    fn api(fake: Arc<FakeLogind>) -> LifecycleApi {
        api_with(fake, Arc::new(FakeSession::default()))
    }

    fn api_with(fake: Arc<FakeLogind>, session: Arc<FakeSession>) -> LifecycleApi {
        LifecycleApi::new(
            fake,
            session,
            Arc::new(FakeUnit(Some(SHELL_UNIT))),
            broadcast::channel(EDGE_CAPACITY).0,
        )
    }

    /// Only `templar-shell.service` knows the compositor's lock is gone. Everything else — including
    /// a caller whose unit cannot be established at all — is refused rather than believed.
    #[tokio::test]
    async fn only_the_shell_may_report_an_unlock() {
        let call = call();
        for unit in [None, Some("app-templaros-org.example.Impostor@1.service")] {
            let api = LifecycleApi::new(
                Arc::new(FakeLogind::default()),
                Arc::new(FakeSession::default()),
                Arc::new(FakeUnit(unit)),
                broadcast::channel(EDGE_CAPACITY).0,
            );
            let error = api
                .notify_unlocked(call.header())
                .await
                .expect_err("only the shell may report an unlock");
            assert!(
                matches!(error, fdo::Error::AccessDenied(_)),
                "{unit:?} must be refused, not failed: {error}"
            );
        }

        let edges: SessionEdges = broadcast::channel(EDGE_CAPACITY).0;
        let mut seen = edges.subscribe();
        LifecycleApi::new(
            Arc::new(FakeLogind::default()),
            Arc::new(FakeSession::default()),
            Arc::new(FakeUnit(Some(SHELL_UNIT))),
            edges,
        )
        .notify_unlocked(call.header())
        .await
        .expect("the shell may report an unlock");
        assert_eq!(seen.recv().await.unwrap(), SessionEdge::Unlocked);
    }

    /// Only `templar-comp.service` sees the seat, so only it may stop or start session time for
    /// idleness; the shell is refused like anyone else.
    #[tokio::test]
    async fn only_the_compositor_may_report_idleness() {
        let call = call();
        for unit in [None, Some(SHELL_UNIT)] {
            let api = LifecycleApi::new(
                Arc::new(FakeLogind::default()),
                Arc::new(FakeSession::default()),
                Arc::new(FakeUnit(unit)),
                broadcast::channel(EDGE_CAPACITY).0,
            );
            let error = api
                .notify_idle(true, call.header())
                .await
                .expect_err("only the compositor may report idleness");
            assert!(
                matches!(error, fdo::Error::AccessDenied(_)),
                "{unit:?}: {error}"
            );
        }
        let edges: SessionEdges = broadcast::channel(EDGE_CAPACITY).0;
        let mut seen = edges.subscribe();
        let api = LifecycleApi::new(
            Arc::new(FakeLogind::default()),
            Arc::new(FakeSession::default()),
            Arc::new(FakeUnit(Some(COMPOSITOR_UNIT))),
            edges,
        );
        api.notify_idle(true, call.header()).await.unwrap();
        api.notify_idle(false, call.header()).await.unwrap();
        assert_eq!(seen.recv().await.unwrap(), SessionEdge::Idle(true));
        assert_eq!(seen.recv().await.unwrap(), SessionEdge::Idle(false));
    }

    /// A method call with a sender, which is all `notify_unlocked` reads out of its header.
    fn call() -> zbus::Message {
        zbus::Message::method_call(names::LIFECYCLE_PATH, "NotifyUnlocked")
            .unwrap()
            .sender(":1.42")
            .unwrap()
            .build(&())
            .unwrap()
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
}
