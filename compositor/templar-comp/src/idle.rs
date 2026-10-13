//! Turning the screen off when nobody is using it, and back on the instant somebody is.
//!
//! **The compositor is the only honest idle source on a Wayland session.** logind's `IdleHint` is
//! not driven by anything here, and no client can see the seat's input; `templar-comp` sees every
//! keyboard, pointer and touch event there is, so this is where "nothing has happened for a
//! while" can be known rather than guessed. Sessiond owns `Power1` policy and schedules its
//! dim/lock/suspend decisions from native samples; this module enforces native screen blanking.
//!
//! What this does **not** do, deliberately:
//!
//! * **Dimming.** Sessiond conditionally owns a Backlight1 raw target and restores only if actual
//!   device/scale/raw readback still belongs to that owner. The render thread does not call D-Bus.
//! * **Locking.** Screen-off and lock intervals are separate policies. Sessiond/logind owns
//!   lock/dim/suspend decisions; this module owns their monotonic seat-activity/inhibitor
//!   epoch. Secure lock completion remains the exact presented-generation barrier in
//!   `src/session_lock.rs`, never a blanked-screen observation.
//!
//! The timing policy itself is pure and tested below; the wiring (a calloop timer, the backend's
//! DPMS call) is in `src/state.rs` and `src/backend/`.

use smithay::reexports::calloop::{RegistrationToken, timer::Timer};
use std::time::{Duration, Instant};

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
/// whether the outputs are off right now, and how many visible fullscreen videos inhibit it.
#[derive(Debug)]
pub struct IdleState {
    timeout: Option<Duration>,
    timer: Option<RegistrationToken>,
    blanked: bool,
    inhibitors: usize,
    last_activity: Instant,
    activity_serial: u64,
}
impl Default for IdleState {
    fn default() -> Self {
        Self {
            timeout: None,
            timer: None,
            blanked: false,
            inhibitors: 0,
            last_activity: Instant::now(),
            activity_serial: 0,
        }
    }
}

impl IdleState {
    /// Advances for actual seat events, including input while already unblanked.
    pub fn note_activity(&mut self) {
        self.activity_serial = self
            .activity_serial
            .checked_add(1)
            .expect("seat activity serial exhausted");
        self.last_activity = Instant::now();
    }
    pub fn activity_serial(&self) -> u64 {
        self.activity_serial
    }
    pub fn idle_elapsed_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.last_activity)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }
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

    /// Applying an effective source/profile policy begins a fresh idle epoch even if its blank
    /// timeout is unchanged. This never advances the real-seat activity serial.
    #[must_use]
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> bool {
        self.last_activity = Instant::now();
        self.timeout = timeout;
        true
    }

    /// Reconcile eligible playback sources. Pausing, leaving fullscreen, shelving and closing
    /// all release inhibition and begin a fresh timeout without inventing seat activity.
    /// Unchanged sources do not re-arm a timer on every video frame.
    pub fn set_inhibitor_count(&mut self, count: usize) -> bool {
        if self.inhibitors == count {
            return false;
        }
        let was_inhibited = self.inhibited();
        self.inhibitors = count;
        let inhibited = self.inhibited();
        if was_inhibited && !inhibited {
            self.last_activity = Instant::now();
        }
        was_inhibited != inhibited
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
        self.next_delay_at(Instant::now())
    }
    fn next_delay_at(&self, now: Instant) -> Option<Duration> {
        delay_before_blanking(self.timeout, self.inhibited(), self.blanked).map(|timeout| {
            timeout.saturating_sub(now.saturating_duration_since(self.last_activity))
        })
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
    fn reapplying_equal_policy_starts_a_new_epoch_without_seat_activity() {
        let mut idle = IdleState::default();
        let timeout = Some(Duration::from_secs(60));
        let _ = idle.set_timeout(timeout);
        idle.last_activity -= Duration::from_secs(10);
        let old_epoch = idle.last_activity;
        let serial = idle.activity_serial();
        let _ = idle.set_timeout(timeout);
        assert!(idle.last_activity > old_epoch);
        assert_eq!(idle.activity_serial(), serial);
        assert_eq!(idle.next_delay_at(idle.last_activity), timeout);
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

    /// Inhibitors nest: two playing fullscreen videos each hold one, and the screen may
    /// blank again only once both are gone.
    #[test]
    fn inhibitors_nest_and_the_screen_stays_awake_until_the_last_one_goes() {
        let mut idle = IdleState::default();
        let _ = idle.set_timeout(Some(Duration::from_secs(300)));
        assert!(idle.set_inhibitor_count(1));
        assert!(!idle.set_inhibitor_count(2));
        assert_eq!(idle.next_delay(), None);
        assert!(!idle.set_inhibitor_count(1));
        assert_eq!(idle.next_delay(), None);
        assert!(idle.set_inhibitor_count(0));
        assert_eq!(
            idle.next_delay_at(idle.last_activity),
            Some(Duration::from_secs(300))
        );
        // Repeated scene updates cannot underflow or extend the timeout forever.
        let epoch = idle.last_activity;
        assert!(!idle.set_inhibitor_count(0));
        assert_eq!(idle.last_activity, epoch);
        assert_eq!(
            idle.next_delay_at(idle.last_activity),
            Some(Duration::from_secs(300))
        );
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
        let _ = idle.set_timeout(None);
        assert_eq!(idle.next_delay(), None);
        assert!(idle.timeout().is_none());
    }

    #[test]
    fn real_activity_and_inhibitor_removal_share_one_monotonic_epoch() {
        let mut idle = IdleState::default();
        let _ = idle.set_timeout(Some(Duration::from_secs(300)));
        let baseline = idle.last_activity;
        assert_eq!(
            idle.idle_elapsed_ms(baseline + Duration::from_secs(20)),
            20_000
        );
        assert_eq!(
            idle.next_delay_at(baseline + Duration::from_secs(20)),
            Some(Duration::from_secs(280))
        );
        idle.note_activity();
        assert_eq!(idle.activity_serial(), 1);
        assert!(!idle.wake());
        idle.note_activity();
        assert_eq!(idle.activity_serial(), 2);
        idle.set_inhibitor_count(1);
        idle.set_inhibitor_count(0);
        assert_eq!(
            idle.activity_serial(),
            2,
            "inhibitor lifecycle is not invented seat input"
        );
        assert_eq!(idle.idle_elapsed_ms(idle.last_activity), 0);
        assert_eq!(
            idle.next_delay_at(idle.last_activity),
            Some(Duration::from_secs(300))
        );
    }
}
