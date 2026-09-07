//! The lock screen: a password field, an error line, and deliberately nothing else.
//!
//! Everything here is a pure state machine over typed characters, so the one part of the lock
//! that has to be right — what happens to the password a person typed — is checked by tests
//! rather than by locking a real session and trying it. The Wayland surface it is painted on is
//! `crate::session_lock`'s; the authentication is `lege-unlock`'s.
//!
//! Two rules the tests hold to. A rejected attempt clears the field, because leaving a wrong
//! password in it means the next Enter sends the same wrong password again. And nothing is
//! accepted while an attempt is in flight, because two attempts racing on one PAM handle is a
//! bug nobody would find by looking.
//!
//! There is no "switch user", no session list and no accessibility menu. This locks *this*
//! session; anything else is a door in the wall of a room whose only purpose is to have no doors.

use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use crate::surface::design;

/// The longest password this will hold. Not a policy — PAM decides what is valid — just a bound
/// on what a stuck key can put in memory.
const MAX_PASSWORD: usize = 256;

/// What the host must do about a key the lock screen just saw.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LockRequest {
    /// Check this password. The host spawns `lege-unlock` and reports back.
    Authenticate(String),
}

#[derive(Debug, Default)]
pub struct LockScreen {
    password: String,
    /// Why the last attempt failed, shown until the next keystroke.
    error: Option<String>,
    /// An attempt is with PAM right now. Further keys are ignored rather than queued.
    checking: bool,
}

impl LockScreen {
    /// How many characters the field holds, which is all a painter needs to know about it: the
    /// password itself never leaves this struct except through [`LockRequest::Authenticate`].
    pub fn len(&self) -> usize {
        self.password.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.password.is_empty()
    }

    pub fn checking(&self) -> bool {
        self.checking
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// One typed character. Control characters are not text; a stuck key stops at [`MAX_PASSWORD`].
    pub fn typed(&mut self, character: char) {
        if self.checking || character.is_control() || self.password.chars().count() >= MAX_PASSWORD
        {
            return;
        }
        self.error = None;
        self.password.push(character);
    }

    pub fn backspace(&mut self) {
        if self.checking {
            return;
        }
        self.error = None;
        self.password.pop();
    }

    /// Escape: throw the attempt away without unlocking. The screen stays locked — this is not a
    /// cancel button, it is the way out of a half-typed password.
    pub fn clear(&mut self) {
        if self.checking {
            return;
        }
        self.error = None;
        self.password.clear();
    }

    /// Enter. An empty field asks nothing (PAM would refuse it, slowly), and neither does a
    /// second Enter while the first is still being checked.
    pub fn submit(&mut self) -> Option<LockRequest> {
        if self.checking || self.password.is_empty() {
            return None;
        }
        self.checking = true;
        self.error = None;
        Some(LockRequest::Authenticate(self.password.clone()))
    }

    /// The attempt failed. The field is cleared: a wrong password left in it is a wrong password
    /// the next Enter sends again.
    pub fn rejected(&mut self, reason: impl Into<String>) {
        self.checking = false;
        self.password.clear();
        self.error = Some(reason.into());
    }

    /// The attempt succeeded and the session is unlocking. Nothing is kept.
    pub fn accepted(&mut self) {
        self.checking = false;
        self.password.clear();
        self.error = None;
    }

    /// Paint the whole surface. `user` is who is being asked, `clock` the same string the bar
    /// shows — a locked screen is still the thing a person glances at for the time.
    pub fn draw(&self, ui: &mut Ui<'_>, area: Rect, user: &str, clock: &str) {
        ui.painter.fill_rect(area, design::ground());

        let card_w = ui.px(360).min(area.w);
        let card_h = ui.px(196).min(area.h);
        let card = Rect::new(
            area.x + (area.w - card_w) / 2,
            area.y + (area.h - card_h) / 2,
            card_w,
            card_h,
        );
        design::shell(
            ui,
            card,
            design::shell_fill(),
            design::edge_strong(),
            design::Elevation::Lifted,
        );

        let pad = ui.px(20);
        let inner = card.inset(pad);
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, ui.px(22)),
            clock,
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + ui.px(28), inner.w, ui.px(28)),
            user,
            design::system_label_style(ui),
            design::text(),
            Align::Left,
        );

        // The field. Dots rather than the characters: this is drawn on a screen whose whole
        // purpose is that somebody else is looking at it.
        let field = Rect::new(inner.x, inner.y + ui.px(66), inner.w, ui.px(38));
        ui.painter
            .fill_rounded_rect(field, ui.px(design::WELL_RADIUS), design::recessed());
        ui.painter
            .stroke_rect(field, ui.px(2).max(1), design::accent());
        let dots = "•".repeat(self.len().min(32));
        let placeholder = if self.checking {
            "Checking…"
        } else {
            "Password"
        };
        let (text, ink) = if self.password.is_empty() {
            (placeholder, design::text_faint())
        } else {
            (dots.as_str(), design::text())
        };
        ui.label_styled(
            field.inset(ui.px(10)),
            text,
            design::title_style(ui),
            ink,
            Align::Left,
        );

        let line = Rect::new(inner.x, field.bottom() + ui.px(10), inner.w, ui.px(20));
        match self.error() {
            Some(error) => ui.label_styled(
                line,
                error,
                design::state_style(ui),
                design::danger(),
                Align::Left,
            ),
            None => ui.label_styled(
                line,
                "Type your password and press Enter",
                design::state_style(ui),
                design::text_faint(),
                Align::Left,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_fills_the_field_and_enter_hands_the_password_over_exactly_once() {
        let mut screen = LockScreen::default();
        assert!(screen.is_empty());
        assert_eq!(screen.submit(), None, "an empty field asked PAM anyway");

        for character in "hunter2".chars() {
            screen.typed(character);
        }
        assert_eq!(screen.len(), 7);
        assert_eq!(
            screen.submit(),
            Some(LockRequest::Authenticate("hunter2".to_owned()))
        );
        assert!(screen.checking());
        // A second Enter while the first attempt is in flight must not start another: two
        // attempts racing on one PAM handle is a bug nobody would find by looking.
        assert_eq!(screen.submit(), None);
        // And nothing typed during the wait is kept, so a person cannot silently edit the
        // password that is already being checked.
        screen.typed('x');
        screen.backspace();
        screen.clear();
        assert_eq!(screen.len(), 7);
    }

    #[test]
    fn a_rejected_attempt_clears_the_field_and_says_why() {
        let mut screen = LockScreen::default();
        for character in "wrong".chars() {
            screen.typed(character);
        }
        screen.submit().expect("submitted");
        screen.rejected("Incorrect password");

        assert!(!screen.checking());
        assert!(
            screen.is_empty(),
            "the wrong password was left in the field"
        );
        assert_eq!(screen.error(), Some("Incorrect password"));
        // The message goes the moment the next attempt begins, rather than sitting under a
        // password that has nothing to do with it.
        screen.typed('a');
        assert_eq!(screen.error(), None);
    }

    #[test]
    fn escape_throws_the_attempt_away_without_unlocking_anything() {
        let mut screen = LockScreen::default();
        for character in "half-typed".chars() {
            screen.typed(character);
        }
        screen.clear();
        assert!(screen.is_empty());
        assert!(!screen.checking(), "clearing the field unlocked something");
        assert_eq!(
            screen.submit(),
            None,
            "an empty field after Escape still asked PAM"
        );
    }

    #[test]
    fn the_field_holds_text_only_and_is_bounded() {
        let mut screen = LockScreen::default();
        for character in ['\n', '\t', '\u{7}', '\u{1b}'] {
            screen.typed(character);
        }
        assert!(
            screen.is_empty(),
            "a control character reached the password"
        );

        for _ in 0..(MAX_PASSWORD + 50) {
            screen.typed('a');
        }
        assert_eq!(screen.len(), MAX_PASSWORD, "a stuck key grew without bound");

        // Non-ASCII is text and belongs in a password like anything else.
        screen.clear();
        screen.typed('é');
        screen.typed('日');
        assert_eq!(screen.len(), 2);
        assert_eq!(
            screen.submit(),
            Some(LockRequest::Authenticate("é日".to_owned()))
        );
    }

    #[test]
    fn accepting_keeps_nothing() {
        let mut screen = LockScreen::default();
        screen.typed('p');
        screen.submit().expect("submitted");
        screen.accepted();
        assert!(screen.is_empty());
        assert!(!screen.checking());
        assert_eq!(screen.error(), None);
    }
}
