//! How hard the daemon is looking, and why.
//!
//! Kernel watches are not free and they are not sufficient. Measured on the reference laptop:
//! `$HOME` holds 189,089 directories once the index's skip policy is applied, against a watch
//! budget of 125,037 (half of `fs.inotify.max_user_watches`, `watch.rs`). Arming is a full walk
//! that stats every entry, and the walk cannot finish inside the budget. So observation is a
//! *resource the daemon spends*, not a state it simply occupies, and something has to decide when
//! spending it is worth it. That is this module.
//!
//! Four states, one axis:
//!
//! - `Dormant` — no watches armed. Direct mutation from `lege-filed`'s own file operations still
//!   flows (`watcher::OperationSink` talks to the index and never to the watcher), so the index is
//!   still correct about everything LegeOS itself did. It is *unproven* about everything else, and
//!   says so: entering `Dormant` marks every root `Stale{unwatched}`.
//! - `Resuming` — a consumer arrived; the arm-and-repair pass is running. Deliberately not
//!   `RootState::CatchingUp`, which already exists and means a sub-second event backlog on one
//!   root. Two different axes; two different words.
//! - `Active` — armed as far as the budget allowed. Not "fully covered": on a home root the arm
//!   pass hits the ceiling and the root commits `Stale{watcher-exhausted}`, which is the honest
//!   report and stays the honest report while the lifecycle says `Active`.
//! - `Grace` — the last consumer went away, but not long enough ago to pay for a re-arm if it
//!   comes back. A file manager window closing and reopening must not cost two tree walks.
//!
//! A consumer is a D-Bus unique name that called `Index1`. Its death normally arrives as
//! `NameOwnerChanged`; the lease TTL is the fallback for the cases that does not cover.
//!
//! Note the one wakeup this costs: while a lease is outstanding, the loop must wake at its expiry
//! to re-check it, and in `Grace` it must wake once when the grace ends. Neither happens in
//! `Dormant`, and neither happens when no consumer has ever taken a lease — which is why the idle
//! machine still reaches a genuinely unbounded wait.

use std::{
    collections::HashMap,
    sync::{OnceLock, mpsc},
    time::Duration,
};

use lege_file_index::watch::SourceWaker;

/// Where the observation lifecycle is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserverState {
    Dormant,
    Resuming,
    Active,
    Grace,
}

impl ObserverState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dormant => "dormant",
            Self::Resuming => "resuming",
            Self::Active => "active",
            Self::Grace => "grace",
        }
    }
}

/// What the loop must do about a transition. Both are expensive, which is why they are returned
/// rather than performed: they belong on the watcher thread, never on a D-Bus caller's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    /// Arm every tracked root and repair it. Walks the tree; costs seconds.
    Arm,
    /// Drop every watch and mark every root `Stale{unwatched}`.
    Disarm,
}

#[derive(Clone, Copy, Debug)]
pub struct ObserverConfig {
    /// How long a consumer's interest is assumed to last without being renewed. The fallback for
    /// a client that died in a way `NameOwnerChanged` did not describe.
    pub lease_ttl_ms: i64,
    /// How long observation survives the last consumer leaving. A window closing and reopening
    /// must not cost two tree walks.
    pub grace_ms: i64,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            lease_ttl_ms: 5 * 60 * 1000,
            grace_ms: 60 * 1000,
        }
    }
}

/// The lifecycle itself: pure, clock-driven, and free of D-Bus.
#[derive(Clone, Debug)]
pub struct Observer {
    config: ObserverConfig,
    state: ObserverState,
    /// Unique bus name to lease expiry, in the loop's own clock.
    leases: HashMap<String, i64>,
    grace_until_ms: Option<i64>,
    /// Until some consumer has actually asked for observation, the daemon keeps watching the way
    /// it always has. Demoting a machine nobody has ever leased against would turn "no client has
    /// been written yet" into "the index stopped watching", which is a regression dressed as a
    /// feature.
    leased_before: bool,
}

impl Observer {
    #[must_use]
    pub fn new(config: ObserverConfig) -> Self {
        Self {
            config,
            state: ObserverState::Resuming,
            leases: HashMap::new(),
            grace_until_ms: None,
            leased_before: false,
        }
    }

    #[must_use]
    pub fn state(&self) -> ObserverState {
        self.state
    }

    #[must_use]
    pub fn consumers(&self) -> usize {
        self.leases.len()
    }

    /// A consumer asked something of the index. Returns `Some(Arm)` when that has to wake the
    /// tree up.
    pub fn lease(&mut self, client: &str, now_ms: i64) -> Option<Transition> {
        self.leases.insert(
            client.to_owned(),
            now_ms.saturating_add(self.config.lease_ttl_ms),
        );
        self.leased_before = true;
        self.grace_until_ms = None;
        match self.state {
            ObserverState::Dormant => {
                self.state = ObserverState::Resuming;
                Some(Transition::Arm)
            }
            ObserverState::Grace => {
                // Still armed; the grace window existed precisely for this.
                self.state = ObserverState::Active;
                None
            }
            ObserverState::Resuming | ObserverState::Active => None,
        }
    }

    /// A consumer went away — `NameOwnerChanged`, or a lease that ran out.
    pub fn release(&mut self, client: &str, now_ms: i64) {
        self.leases.remove(client);
        self.enter_grace_if_idle(now_ms);
    }

    /// The arm pass finished. `Active` means "armed as far as the budget allowed", never "fully
    /// covered" — the per-root `Stale{watcher-exhausted}` is where coverage is reported.
    pub fn armed(&mut self) {
        if self.state == ObserverState::Resuming {
            self.state = ObserverState::Active;
        }
    }

    /// Expire what has run out and decide whether the state must change.
    pub fn tick(&mut self, now_ms: i64) -> Option<Transition> {
        self.leases.retain(|_, expiry| *expiry > now_ms);
        self.enter_grace_if_idle(now_ms);
        if self.state == ObserverState::Grace
            && self.grace_until_ms.is_some_and(|until| now_ms >= until)
        {
            self.state = ObserverState::Dormant;
            self.grace_until_ms = None;
            return Some(Transition::Disarm);
        }
        None
    }

    fn enter_grace_if_idle(&mut self, now_ms: i64) {
        if !self.leases.is_empty() || !self.leased_before {
            return;
        }
        if matches!(self.state, ObserverState::Resuming | ObserverState::Active) {
            self.state = ObserverState::Grace;
            self.grace_until_ms = Some(now_ms.saturating_add(self.config.grace_ms));
        }
    }

    /// The next moment the lifecycle itself needs the loop awake, if any.
    ///
    /// `None` in `Dormant`, and `None` while no lease has ever been taken: those are the two
    /// shapes of a genuinely idle machine, and neither may cost a timer.
    #[must_use]
    pub fn next_deadline(&self, now_ms: i64) -> Option<Duration> {
        let deadline = match self.state {
            ObserverState::Dormant => return None,
            ObserverState::Grace => self.grace_until_ms?,
            ObserverState::Resuming | ObserverState::Active => *self.leases.values().min()?,
        };
        Some(Duration::from_millis(
            u64::try_from(deadline.saturating_sub(now_ms)).unwrap_or(0),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// The control surface a D-Bus caller reaches the watcher thread through.
// ---------------------------------------------------------------------------------------------

/// What a caller asks the watcher thread to do about a consumer.
#[derive(Clone, Debug)]
pub enum Command {
    Lease(String),
    Release(String),
}

/// A non-blocking handle onto the watcher loop.
///
/// Arming walks and stats a tree; it must never happen on the task answering a D-Bus method. So
/// the method sends a message and pokes the waker, and returns — the whole interaction is a
/// channel send plus, at most, one `mpsc::Sender::send` on the event source.
#[derive(Clone, Debug)]
pub struct WatcherControl {
    commands: mpsc::Sender<Command>,
    waker: Option<SourceWaker>,
}

impl WatcherControl {
    #[must_use]
    pub fn new(commands: mpsc::Sender<Command>, waker: Option<SourceWaker>) -> Self {
        Self { commands, waker }
    }

    /// Best effort by construction: a watcher that has stopped is not a reason to fail a query.
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            self.wake();
        }
    }

    pub fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake();
        }
    }
}

/// The one control handle in the process.
///
/// A `static` rather than a field on `FileService` because the watcher is started *after* the bus
/// name is claimed and the interfaces are served (`main.rs`) — deliberately, so no client ever
/// waits on a scan — which means the D-Bus objects exist before the handle they would carry does.
static CONTROL: OnceLock<WatcherControl> = OnceLock::new();

/// Called once, by `main.rs`, after the watcher thread starts.
pub fn register(control: WatcherControl) {
    let _ = CONTROL.set(control);
}

#[must_use]
pub fn control() -> Option<&'static WatcherControl> {
    CONTROL.get()
}

/// A D-Bus caller touched the index. `None` is the p2p case, where there is no unique name to
/// lease against and nothing to do.
pub fn note_consumer(client: Option<&str>) {
    if let (Some(control), Some(client)) = (control(), client) {
        control.send(Command::Lease(client.to_owned()));
    }
}

/// A unique name lost its owner: the client is gone, whatever it did or did not call.
pub fn forget_consumer(client: &str) {
    if let Some(control) = control() {
        control.send(Command::Release(client.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ObserverConfig {
        ObserverConfig {
            lease_ttl_ms: 1_000,
            grace_ms: 500,
        }
    }

    /// The whole cycle, on a clock the test owns: a consumer arrives, the pass finishes, the
    /// consumer leaves, grace runs out, and only then does the daemon stop looking.
    #[test]
    fn a_consumer_arrives_leaves_and_the_daemon_goes_dormant_after_grace() {
        let mut observer = Observer::new(config());
        assert_eq!(observer.state(), ObserverState::Resuming);

        // Nothing has ever leased, so the startup pass finishing leaves it Active and idle: no
        // deadline at all, which is what a blocking wait needs.
        observer.armed();
        assert_eq!(observer.state(), ObserverState::Active);
        assert_eq!(observer.next_deadline(0), None);
        assert_eq!(observer.tick(10_000), None, "nobody has asked for anything");
        assert_eq!(observer.state(), ObserverState::Active);

        assert_eq!(observer.lease(":1.42", 0), None, "already armed");
        assert_eq!(observer.consumers(), 1);
        assert_eq!(
            observer.next_deadline(0),
            Some(Duration::from_millis(1_000))
        );

        observer.release(":1.42", 100);
        assert_eq!(observer.state(), ObserverState::Grace);
        assert_eq!(
            observer.next_deadline(100),
            Some(Duration::from_millis(500))
        );

        assert_eq!(observer.tick(500), None, "the grace window is not over");
        assert_eq!(observer.tick(600), Some(Transition::Disarm));
        assert_eq!(observer.state(), ObserverState::Dormant);
        assert_eq!(
            observer.next_deadline(600),
            None,
            "a dormant watcher must cost no timer at all"
        );
    }

    /// The point of the grace window: a window closing and reopening costs no tree walk.
    #[test]
    fn a_consumer_returning_within_grace_costs_no_arm() {
        let mut observer = Observer::new(config());
        observer.armed();
        observer.lease(":1.7", 0);
        observer.release(":1.7", 10);
        assert_eq!(observer.state(), ObserverState::Grace);

        assert_eq!(observer.lease(":1.7", 20), None, "still armed");
        assert_eq!(observer.state(), ObserverState::Active);
        assert_eq!(observer.tick(1_000_000), None, "the lease is fresh enough");
    }

    /// A client that died without `NameOwnerChanged` still stops holding the tree open.
    #[test]
    fn an_expired_lease_is_as_good_as_a_departure() {
        let mut observer = Observer::new(config());
        observer.armed();
        observer.lease(":1.9", 0);
        assert_eq!(observer.tick(999), None);
        assert_eq!(observer.state(), ObserverState::Active);

        assert_eq!(observer.tick(1_001), None, "the grace window starts now");
        assert_eq!(observer.state(), ObserverState::Grace);
        assert_eq!(observer.consumers(), 0);
        assert_eq!(observer.tick(1_501), Some(Transition::Disarm));
        assert_eq!(observer.state(), ObserverState::Dormant);
    }

    /// Waking a dormant watcher is the one transition that costs a walk, so it is the one the
    /// caller is told about.
    #[test]
    fn leasing_a_dormant_watcher_asks_for_an_arm() {
        let mut observer = Observer::new(config());
        observer.armed();
        observer.lease(":1.1", 0);
        observer.release(":1.1", 0);
        observer.tick(600);
        assert_eq!(observer.state(), ObserverState::Dormant);

        assert_eq!(observer.lease(":1.2", 700), Some(Transition::Arm));
        assert_eq!(observer.state(), ObserverState::Resuming);
        observer.armed();
        assert_eq!(observer.state(), ObserverState::Active);
    }

    /// Two consumers, one leaving: observation belongs to whoever is still there.
    #[test]
    fn the_last_consumer_out_starts_the_grace_window() {
        let mut observer = Observer::new(config());
        observer.armed();
        observer.lease(":1.1", 0);
        observer.lease(":1.2", 0);
        observer.release(":1.1", 10);
        assert_eq!(observer.state(), ObserverState::Active);
        observer.release(":1.2", 20);
        assert_eq!(observer.state(), ObserverState::Grace);
    }
}
