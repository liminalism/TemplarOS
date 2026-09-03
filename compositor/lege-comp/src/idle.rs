//! Turning the screen off when nobody is using it, and back on the instant somebody is.
//!
//! **The compositor is the only honest idle source on a Wayland session.** logind's `IdleHint` is
//! not driven by anything here, and no client can see the seat's input; `lege-comp` sees every
//! keyboard, pointer and touch event there is, so this is where "nothing has happened for a
//! while" can be known rather than guessed. `org.legeos.Power1` records the policy
//! (`crates/lege-session-core/src/power.rs`'s `screen_off_after`) and says in as many words that
//! it does not enforce it; this module is the enforcer for the screen-off half of it, and only
//! that half.
//!
//! What this does **not** do, deliberately:
//!
//! * **Dimming.** A dim is a backlight ramp before the blank, and the backlight is
//!   `org.legeos.Backlight1` — a system-bus service this process does not talk to. It would
//!   attach at [`IdleState::expire`]: a first timer at `dim_after` that asks `Backlight1` to drop
//!   the level and remembers the old one, and [`IdleState::wake`] restoring it. That needs a
//!   D-Bus channel out of the render thread (the `lege-comp-integration` pattern: a background
//!   tokio task, never a blocking call here), which is why it is not folded in as a "small
//!   addition".
//! * **Locking.** `screen_off_after` and `lock_after` are separate intervals in the same profile
//!   and mean different things. Locking would attach at the same place, by driving
//!   `ext-session-lock-v1` (`src/session_lock.rs`) — but `Session.Lifecycle1.Lock` is still a
//!   `NotSupported` stub for want of a lock-manager client, so a lock timer here would fire into
//!   nothing. A screen that turns off is not a screen that is locked, and this module never
//!   claims otherwise.
//!
//! The timing policy itself is pure and tested below; the wiring (a calloop timer, the backend's
//! DPMS call) is in `src/state.rs` and `src/backend/`.

use smithay::reexports::calloop::{RegistrationToken, timer::Timer};
use std::time::Duration;

/// The screen-off delay, from a setting in seconds.
///
/// Zero is "never", which is a setting and not an absence — a person who has turned idle blanking
/// off has said something, and the shell sends that as deliberately as any other value.
pub fn timeout_from_seconds(seconds: u32) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)))
}

/// How long to wait before blanking, given the configured timeout and what is going on.
///
/// `None` means "arm no timer at all": either idle blanking is off, or something has said the
/// screen must stay on. Both are the same answer to the event loop, and deliberately the same
/// answer here — an inhibited session is not a session with a very long timeout, it is one with
/// no timer, so removing the inhibitor re-arms from that moment rather than from whenever the
/// last key was pressed.
pub fn delay_before_blanking(
    timeout: Option<Duration>,
    inhibited: bool,
    already_blanked: bool,
) -> Option<Duration> {
    if inhibited || already_blanked {
        return None;
    }
    timeout
}

/// The idle watcher's whole state: the configured delay, the timer currently armed (if any),
/// whether the outputs are off right now, and how many `zwp_idle_inhibitor_v1` objects are alive.
#[derive(Debug, Default)]
pub struct IdleState {
    timeout: Option<Duration>,
    timer: Option<RegistrationToken>,
    blanked: bool,
    inhibitors: usize,
}

impl IdleState {
    /// Whether the outputs are currently off.
    pub fn blanked(&self) -> bool {
        self.blanked
    }

    /// The configured delay, for logging and `--check`'s report.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Whether anything is currently holding the screen awake.
    pub fn inhibited(&self) -> bool {
        self.inhibitors > 0
    }

    /// Apply a new setting. Returns whether the timer needs re-arming (it always does; the return
    /// exists so the caller cannot forget).
    #[must_use]
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> bool {
        self.timeout = timeout;
        true
    }

    /// One more `zwp_idle_inhibitor_v1` is alive.
    pub fn add_inhibitor(&mut self) {
        self.inhibitors = self.inhibitors.saturating_add(1);
    }

    /// One fewer — saturating, because a client that destroys an inhibitor it never created is a
    /// protocol error Smithay rejects before this is reached, and an underflow here would turn a
    /// bug into a screen that never blanks again.
    pub fn remove_inhibitor(&mut self) {
        self.inhibitors = self.inhibitors.saturating_sub(1);
    }

    /// Mark the screen off. Returns whether it was on before, so the caller only reaches for the
    /// backend when there is something to change.
    pub fn expire(&mut self) -> bool {
        let changed = !self.blanked;
        self.blanked = true;
        changed
    }

    /// Mark the screen on. Returns whether it was off before.
    pub fn wake(&mut self) -> bool {
        let changed = self.blanked;
        self.blanked = false;
        changed
    }

    /// The delay to arm right now, or `None` for "arm nothing".
    pub fn next_delay(&self) -> Option<Duration> {
        delay_before_blanking(self.timeout, self.inhibited(), self.blanked)
    }

    /// Take the armed timer's registration token, if there is one, so the caller can remove it.
    pub fn take_timer(&mut self) -> Option<RegistrationToken> {
        self.timer.take()
    }

    /// Remember the timer the caller just inserted.
    pub fn set_timer(&mut self, token: RegistrationToken) {
        self.timer = Some(token);
    }

    /// The calloop source for [`next_delay`], ready to insert.
    ///
    /// [`next_delay`]: Self::next_delay
    pub fn timer_source(&self) -> Option<Timer> {
        self.next_delay().map(Timer::from_duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seconds_is_never_rather_than_immediately() {
        assert_eq!(timeout_from_seconds(0), None);
        assert_eq!(timeout_from_seconds(60), Some(Duration::from_secs(60)));
    }

    #[test]
    fn an_inhibitor_arms_no_timer_at_all() {
        let timeout = Some(Duration::from_secs(300));
        assert_eq!(delay_before_blanking(timeout, false, false), timeout);
        assert_eq!(delay_before_blanking(timeout, true, false), None);
        assert_eq!(delay_before_blanking(None, false, false), None);
    }

    /// Nothing re-arms while the screen is already off: the next thing that happens is input,
    /// and input wakes rather than re-times.
    #[test]
    fn a_blanked_screen_arms_no_further_timer() {
        let timeout = Some(Duration::from_secs(300));
        assert_eq!(delay_before_blanking(timeout, false, true), None);
    }

    /// Inhibitors nest: a video player and a presentation tool each hold one, and the screen may
    /// blank again only once both are gone.
    #[test]
    fn inhibitors_nest_and_the_screen_stays_awake_until_the_last_one_goes() {
        let mut idle = IdleState::default();
        let _ = idle.set_timeout(Some(Duration::from_secs(300)));
        idle.add_inhibitor();
        idle.add_inhibitor();
        assert_eq!(idle.next_delay(), None);
        idle.remove_inhibitor();
        assert_eq!(idle.next_delay(), None);
        idle.remove_inhibitor();
        assert_eq!(idle.next_delay(), Some(Duration::from_secs(300)));
        // An unbalanced destroy must not underflow into "inhibited forever".
        idle.remove_inhibitor();
        assert_eq!(idle.next_delay(), Some(Duration::from_secs(300)));
    }

    /// Blanking and waking each report whether they changed anything, so the backend's DPMS call
    /// happens once per transition rather than once per input event.
    #[test]
    fn blanking_and_waking_report_only_real_transitions() {
        let mut idle = IdleState::default();
        assert!(!idle.wake(), "an already-lit screen is not woken");
        assert!(idle.expire());
        assert!(
            !idle.expire(),
            "an already-dark screen is not blanked twice"
        );
        assert!(idle.blanked());
        assert!(idle.wake());
        assert!(!idle.blanked());
    }

    /// "Never" means never, whatever else is going on.
    #[test]
    fn a_timeout_of_never_arms_nothing_however_idle_the_session_is() {
        let mut idle = IdleState::default();
        let _ = idle.set_timeout(timeout_from_seconds(0));
        assert_eq!(idle.next_delay(), None);
        assert!(idle.timeout().is_none());
    }
}
