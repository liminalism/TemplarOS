//! The greeter screen: username, a generic auth prompt (masked exactly when greetd says
//! `secret`), a session picker, and an error line — pixelkit state over `crate::login`/
//! `crate::sessions`'s already-tested, socket-free models, the same shape
//! `shell/lege-shell/src/surface/home_screen.rs` uses for its own drawer.
//!
//! # Why one field, not "a username field and a password field"
//!
//! `greetd-ipc(7)` is explicit that a greeter "should not make any assumptions about the messages"
//! PAM asks — there can be zero, one, or several, of any of the four kinds, in any order (a TOTP
//! code after the password is a normal PAM stack, not an edge case). So this screen has exactly one
//! primary input slot whose meaning is contextual: the username before authentication starts, then
//! whatever [`crate::login::Prompt`] greetd just sent, masked or not exactly as it says. In the
//! overwhelmingly common case (one `secret` prompt reading "Password:") this looks and behaves like
//! an ordinary username-then-password form; it does not *assume* that shape the way a hardcoded
//! second field would.
//!
//! # Keyboard-first, by construction
//!
//! `docs/architecture/accessibility.md`: "The recovery overlay, window shelf and 'show every
//! window' are keyboard-reachable without the shell" — and a greeter is the one surface with no
//! shell, no recovery overlay and no fallback switcher to reach if the pointer does not work, so it
//! must be the least optional of all of them. Tab/Shift-Tab cycle three slots
//! (primary field → session picker → sign-in button) through [`pixelkit_ui::Focus`], Enter submits
//! from any of them, and Escape cancels. Two of those three slots need a keyboard escape hatch
//! `pixelkit-ui` does not build in on its own — see the doc comments on
//! [`GreeterScreen::draw_session_picker`] and [`GreeterScreen::draw_submit_button`] for exactly
//! what is missing and how this screen supplies it locally, which is the pattern
//! `pixelkit_ui::focus`'s own module doc names for exactly this situation ("a widget that wants
//! arrows to move among its own items only ... is expected to do that itself").

use pixelkit_raster::Rect;
use pixelkit_shell::KeyInput;
use pixelkit_text::Align;
use pixelkit_ui::{Dropdown, Focus, TextFieldState, Ui};

use crate::login::LoginFlow;
use crate::sessions::SessionEntry;

/// What the user asked for this frame. `crate::surface::app` is the only caller, and the only
/// place any of these turn into a `LoginController` call or a process exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreeterAction {
    None,
    /// Enter (or the button) with the username field showing and non-empty.
    SubmitUsername(String),
    /// Enter (or the button) with an active prompt. `None` for an info/error message that needs no
    /// typed answer; `Some(text)` — often a password — for a visible/secret one.
    SubmitResponse(Option<String>),
    /// Escape: abandon whatever is in flight.
    Cancel,
}

/// Everything the screen reads but does not own.
#[derive(Debug)]
pub struct GreeterData<'a> {
    pub flow: &'a LoginFlow,
    pub sessions: &'a [SessionEntry],
}

/// The screen's own widget state. **Deliberately does not derive `Debug`** — see the hand-written
/// impl below for why, and `debug_output_never_contains_the_typed_response` for the check.
pub struct GreeterScreen {
    /// The username field, shown before authentication starts. Not a secret — typed in the clear —
    /// so this one is fine to see in a debugger.
    pub username: TextFieldState,
    /// Whatever is typed in answer to the current prompt. This is the field that holds a password
    /// far more often than not, which is the entire reason this struct's `Debug` is hand-written.
    pub response: TextFieldState,
    pub focus: Focus,
    pub session_dropdown: Dropdown,
    pub selected_session: usize,
}

impl std::fmt::Debug for GreeterScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GreeterScreen")
            .field("username", &self.username)
            .field("response", &"<redacted>")
            .field("selected_session", &self.selected_session)
            .finish_non_exhaustive()
    }
}

impl Default for GreeterScreen {
    fn default() -> GreeterScreen {
        GreeterScreen {
            username: TextFieldState::new(),
            response: TextFieldState::new(),
            focus: Focus::new(),
            session_dropdown: Dropdown::new(),
            selected_session: 0,
        }
    }
}

const CARD_WIDTH: i32 = 420;
const CARD_HEIGHT: i32 = 360;
const FIELD_HEIGHT: i32 = 40;
const ROW_GAP: i32 = 16;
const HEADING_HEIGHT: i32 = 28;

impl GreeterScreen {
    /// Draw one frame at `area` (the window's full content rect) and report what the user asked
    /// for. Selecting a session, typing, and moving focus are all handled internally, the same
    /// division `HomeSurfaceScreen::draw` uses.
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &GreeterData<'_>) -> GreeterAction {
        ui.painter.fill_rect(area, ui.theme.background);

        // Read before anything below consumes the frame's keys — `text_field`'s own doc: a
        // focused field takes every key, Enter included, even though its own `apply` does nothing
        // with Enter. `home_screen.rs`'s search field reads Enter the same way, before drawing.
        let keys = ui.input.keys().to_vec();
        let enter = keys.contains(&KeyInput::Enter);
        let escape = keys.contains(&KeyInput::Escape);
        self.focus.handle_tab(&keys, ui.input.modifiers());

        // On the very first frame nothing has focus yet, and a keyboard-only user has no pointer
        // to click the username field into focus with — so default the first slot to focused
        // rather than requiring a "wasted" first Tab press before the form is reachable at all.
        if self.focus.current().is_none() {
            self.focus.set(0);
        }

        let card = Rect::new(
            area.x + (area.w - ui.px(CARD_WIDTH)).max(0) / 2,
            area.y + (area.h - ui.px(CARD_HEIGHT)).max(0) / 2,
            ui.px(CARD_WIDTH).min(area.w),
            ui.px(CARD_HEIGHT).min(area.h),
        );
        let inner = ui.panel(card);
        let mut cursor = inner.y;

        ui.heading(
            Rect::new(inner.x, cursor, inner.w, ui.px(HEADING_HEIGHT)),
            "Log in",
        );
        cursor += ui.px(HEADING_HEIGHT) + ui.px(ROW_GAP);

        let primary_action = self.draw_primary_field(ui, &mut cursor, area, data, enter);

        let session_action = self.draw_session_picker(ui, &mut cursor, inner, data.sessions, enter);

        let button_action = self.draw_submit_button(ui, &mut cursor, inner, data, enter);

        Self::draw_status_line(ui, cursor, inner, data);

        if escape {
            return GreeterAction::Cancel;
        }
        primary_action
            .or(session_action)
            .or(button_action)
            .unwrap_or(GreeterAction::None)
    }

    /// Slot 0: the username field before authentication starts, or the field answering
    /// `data.flow`'s current prompt once it has. Returns `Some` only when Enter submitted it.
    fn draw_primary_field(
        &mut self,
        ui: &mut Ui<'_>,
        cursor: &mut i32,
        area: Rect,
        data: &GreeterData<'_>,
        enter: bool,
    ) -> Option<GreeterAction> {
        let inner = Rect::new(
            area.x + (area.w - ui.px(CARD_WIDTH)).max(0) / 2 + ui.theme.padding,
            *cursor,
            ui.px(CARD_WIDTH).min(area.w) - ui.theme.padding * 2,
            ui.px(FIELD_HEIGHT),
        );
        let focused = self.focus.register();

        let action = match data.flow {
            LoginFlow::Idle { .. } => {
                ui.label(
                    Rect::new(inner.x, inner.y - ui.px(18), inner.w, ui.px(16)),
                    "Username",
                    ui.theme.text_dim,
                    Align::Left,
                );
                ui.text_field(&mut self.username, inner, "Username", focused, false);
                let username = self.username.text();
                (enter && focused && !username.is_empty())
                    .then_some(GreeterAction::SubmitUsername(username.to_owned()))
            }
            LoginFlow::Authenticating { prompt, .. } => {
                ui.label(
                    Rect::new(inner.x, inner.y - ui.px(18), inner.w, ui.px(16)),
                    &prompt.message,
                    ui.theme.text_dim,
                    Align::Left,
                );
                if prompt.needs_response {
                    ui.text_field(&mut self.response, inner, "", focused, prompt.secret);
                    let text = self.response.text();
                    (enter && focused)
                        .then_some(GreeterAction::SubmitResponse(Some(text.to_owned())))
                } else {
                    ui.panel(inner);
                    ui.label(
                        inner.inset(ui.theme.padding / 2),
                        "Press Enter to continue",
                        ui.theme.text_dim,
                        Align::Left,
                    );
                    (enter && focused).then_some(GreeterAction::SubmitResponse(None))
                }
            }
            LoginFlow::ReadyToStart { username } => {
                ui.panel(inner);
                ui.label(
                    inner.inset(ui.theme.padding / 2),
                    &format!("Starting session for {username}…"),
                    ui.theme.text_dim,
                    Align::Left,
                );
                None
            }
        };

        *cursor += ui.px(FIELD_HEIGHT) + ui.px(ROW_GAP);
        action
    }

    /// Slot 1: which session `start_session` will run. Fully mouse-driven through
    /// [`pixelkit_ui::Dropdown`] (click to open, click a row to pick) exactly like every other
    /// `Ui::dropdown` caller — but `Dropdown` itself only ever opens or closes in response to a
    /// click (`pixelkit-ui/src/dropdown.rs`: `Ui::dropdown` calls `self.input.take_click(area)`
    /// and nothing else changes `dropdown.open`), so a keyboard-only user could never reach a
    /// second session at all without a way in from here. Left/Right, while this slot has focus,
    /// cycle `selected_session` directly on the closed field — never opening the popup — which is
    /// exactly the escape hatch `pixelkit_ui::focus`'s own module doc describes for "a widget that
    /// wants arrows to move among its own items only": scoped to this one control's own count, not
    /// going through `Focus::handle_arrows` (which would make Left/Right mean the same thing for
    /// every focused control on the screen, including the text fields' own caret movement).
    fn draw_session_picker(
        &mut self,
        ui: &mut Ui<'_>,
        cursor: &mut i32,
        inner: Rect,
        sessions: &[SessionEntry],
        _enter: bool,
    ) -> Option<GreeterAction> {
        let area = Rect::new(inner.x, *cursor, inner.w, ui.px(FIELD_HEIGHT));
        let focused = self.focus.register();

        if sessions.is_empty() {
            ui.panel(area);
            ui.label(
                area.inset(ui.theme.padding / 2),
                "No sessions found",
                ui.theme.alarm,
                Align::Left,
            );
        } else {
            self.selected_session = self.selected_session.min(sessions.len() - 1);
            let names: Vec<&str> = sessions.iter().map(|entry| entry.name.as_str()).collect();

            if focused {
                for key in ui.input.keys() {
                    match key {
                        KeyInput::Right | KeyInput::Down => {
                            self.selected_session = (self.selected_session + 1) % sessions.len();
                        }
                        KeyInput::Left | KeyInput::Up => {
                            self.selected_session =
                                (self.selected_session + sessions.len() - 1) % sessions.len();
                        }
                        _ => {}
                    }
                }
            }

            if ui.dropdown(
                &mut self.session_dropdown,
                area,
                &names,
                self.selected_session,
            ) {
                // `Ui::dropdown` already toggled `self.session_dropdown`'s open/closed state.
            }
        }

        *cursor += ui.px(FIELD_HEIGHT) + ui.px(ROW_GAP);
        None
    }

    /// The popup a mouse click on the session picker opened, if any — drawn last, after
    /// everything else, per [`Dropdown::draw`]'s own contract. Returns the picked index.
    pub fn draw_session_popup(&mut self, ui: &mut Ui<'_>, bounds: Rect) {
        if let Some(picked) = self.session_dropdown.draw(ui, bounds) {
            self.selected_session = picked;
        }
    }

    /// Slot 2: the primary action button. `Ui::button` only reports a press on
    /// `Input::take_click` (`pixelkit-ui/src/widget.rs`), so — the same gap as the session picker
    /// above, and the same fix — Enter while this slot has focus is treated as a click here rather
    /// than inside `pixelkit-ui` itself.
    fn draw_submit_button(
        &mut self,
        ui: &mut Ui<'_>,
        cursor: &mut i32,
        inner: Rect,
        data: &GreeterData<'_>,
        enter: bool,
    ) -> Option<GreeterAction> {
        let area = Rect::new(inner.x, *cursor, inner.w, ui.px(FIELD_HEIGHT));
        let focused = self.focus.register();
        let label = match data.flow {
            LoginFlow::Authenticating { prompt, .. } if !prompt.needs_response => "Continue",
            LoginFlow::Idle { .. } | LoginFlow::Authenticating { .. } => "Sign in",
            LoginFlow::ReadyToStart { .. } => "Starting…",
        };
        let clicked = ui.button(area, label);
        *cursor += ui.px(FIELD_HEIGHT);

        if !(clicked || (enter && focused)) {
            return None;
        }
        match data.flow {
            LoginFlow::Idle { .. } => {
                let username = self.username.text();
                (!username.is_empty()).then_some(GreeterAction::SubmitUsername(username.to_owned()))
            }
            LoginFlow::Authenticating { prompt, .. } if prompt.needs_response => Some(
                GreeterAction::SubmitResponse(Some(self.response.text().to_owned())),
            ),
            LoginFlow::Authenticating { .. } => Some(GreeterAction::SubmitResponse(None)),
            LoginFlow::ReadyToStart { .. } => None,
        }
    }

    fn draw_status_line(ui: &mut Ui<'_>, cursor: i32, inner: Rect, data: &GreeterData<'_>) {
        let LoginFlow::Idle { error: Some(error) } = data.flow else {
            return;
        };
        let area = Rect::new(inner.x, cursor + ui.px(ROW_GAP), inner.w, ui.px(40));
        ui.label(area, error, ui.theme.alarm, Align::Left);
    }

    /// Clear whatever was typed for the last prompt. `crate::surface::app` calls this once
    /// `data.flow` leaves `Authenticating` (an error, a cancel, or a session actually starting) —
    /// there is never a reason to keep old input sitting in memory once it can no longer be
    /// submitted, which is as far as this crate goes towards minimising how long a typed secret's
    /// bytes stay resident (see the crate root doc for what this does *not* do: no zeroing).
    pub fn clear_response(&mut self) {
        self.response.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login::Prompt;
    use crate::protocol::AuthMessageType;
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, MouseButton, Scale};
    use pixelkit_text::TextCache;
    use pixelkit_text::font::test_fonts::set;

    /// The same headless harness `pixelkit-ui`'s own `dropdown.rs` tests use: a real `Ui` over a
    /// plain buffer and deterministic test fonts, no window, no compositor, no display.
    struct Harness {
        buffer: WindowBuffer,
        text: TextCache,
        kernel: RasterKernel,
        input: Input,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                buffer: WindowBuffer::new(800, 600),
                text: TextCache::new(set()),
                kernel: RasterKernel::new(),
                input: Input::new(),
            }
        }

        fn frame<R>(&mut self, run: impl FnOnce(&mut Ui<'_>) -> R) -> R {
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                pixelkit_ui::Theme::default(),
                Scale::ONE,
                &mut self.kernel,
            );
            let result = run(&mut ui);
            self.input.end_frame();
            result
        }
    }

    fn area() -> Rect {
        Rect::new(0, 0, 800, 600)
    }

    fn legeos_session() -> SessionEntry {
        SessionEntry {
            name: "LegeOS".to_owned(),
            exec: vec!["lege-session".to_owned()],
            kind: crate::sessions::SessionKind::Wayland,
            source: "/usr/share/wayland-sessions/legeos.desktop".into(),
        }
    }

    fn cosmic_session() -> SessionEntry {
        SessionEntry {
            name: "COSMIC".to_owned(),
            exec: vec!["/usr/bin/start-cosmic".to_owned()],
            kind: crate::sessions::SessionKind::Wayland,
            source: "/usr/share/wayland-sessions/cosmic.desktop".into(),
        }
    }

    #[test]
    fn the_username_field_has_focus_on_the_first_frame_with_no_tab_pressed() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            screen.focus.current(),
            Some(0),
            "keyboard-only, no click, must still land somewhere"
        );
    }

    #[test]
    fn tab_cycles_through_the_three_focusable_slots_and_wraps() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session(), cosmic_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data)); // establishes the count (3)
        assert_eq!(screen.focus.current(), Some(0));

        h.input.key(KeyInput::Tab);
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            screen.focus.current(),
            Some(1),
            "username field -> session picker"
        );

        h.input.key(KeyInput::Tab);
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            screen.focus.current(),
            Some(2),
            "session picker -> sign-in button"
        );

        h.input.key(KeyInput::Tab);
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            screen.focus.current(),
            Some(0),
            "wraps back to the username field"
        );
    }

    #[test]
    fn typing_a_username_and_pressing_enter_submits_it() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data)); // focus lands on the username field

        for character in "alice".chars() {
            h.input.key(KeyInput::Character(character));
        }
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(screen.username.text(), "alice");

        h.input.key(KeyInput::Enter);
        let action = h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(action, GreeterAction::SubmitUsername("alice".to_owned()));
    }

    #[test]
    fn enter_on_an_empty_username_field_submits_nothing() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));

        h.input.key(KeyInput::Enter);
        let action = h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(action, GreeterAction::None);
    }

    #[test]
    fn a_secret_prompt_masks_the_field_and_enter_submits_the_typed_text() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        screen.focus.set(0);
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::Authenticating {
            username: "alice".to_owned(),
            prompt: Prompt::new(AuthMessageType::Secret, "Password:".to_owned()),
        };
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));

        for character in "hunter2".chars() {
            h.input.key(KeyInput::Character(character));
        }
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(screen.response.text(), "hunter2");

        h.input.key(KeyInput::Enter);
        let action = h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            action,
            GreeterAction::SubmitResponse(Some("hunter2".to_owned()))
        );
    }

    #[test]
    fn an_info_message_needs_no_typed_text_and_enter_sends_none() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        screen.focus.set(0);
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::Authenticating {
            username: "alice".to_owned(),
            prompt: Prompt::new(AuthMessageType::Info, "one moment please".to_owned()),
        };
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));

        h.input.key(KeyInput::Enter);
        let action = h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(action, GreeterAction::SubmitResponse(None));
    }

    #[test]
    fn escape_always_cancels_regardless_of_which_slot_has_focus() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::Authenticating {
            username: "alice".to_owned(),
            prompt: Prompt::new(AuthMessageType::Secret, "Password:".to_owned()),
        };
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));

        h.input.key(KeyInput::Escape);
        let action = h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(action, GreeterAction::Cancel);
    }

    #[test]
    fn arrow_keys_cycle_the_session_picker_purely_by_keyboard_when_it_has_focus() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session(), cosmic_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));
        h.input.key(KeyInput::Tab); // username field -> session picker
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(screen.focus.current(), Some(1));
        assert_eq!(screen.selected_session, 0);

        h.input.key(KeyInput::Right);
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            screen.selected_session, 1,
            "moved with no click or open popup involved"
        );
        assert!(
            !screen.session_dropdown.is_open(),
            "cycling never opens the popup"
        );

        h.input.key(KeyInput::Left);
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(screen.selected_session, 0, "wraps back");
    }

    #[test]
    fn arrow_keys_do_nothing_to_the_session_picker_when_it_does_not_have_focus() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions = vec![legeos_session(), cosmic_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data)); // focus is on the username field, slot 0

        h.input.key(KeyInput::Right);
        h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(
            screen.selected_session, 0,
            "the field with focus should see the arrow, not the picker"
        );
    }

    #[test]
    fn no_sessions_found_is_shown_and_does_not_panic_when_cycled() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        let sessions: Vec<SessionEntry> = Vec::new();
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        h.frame(|ui| screen.draw(ui, area(), &data));
        h.input.key(KeyInput::Tab);
        h.input.key(KeyInput::Right);
        h.frame(|ui| screen.draw(ui, area(), &data)); // must not panic on an empty session list
        assert_eq!(screen.selected_session, 0);
    }

    #[test]
    fn clicking_the_sign_in_button_submits_the_username_without_pressing_enter() {
        let mut h = Harness::new();
        let mut screen = GreeterScreen::default();
        screen.username.set_text("bob");
        let sessions = vec![legeos_session()];
        let flow = LoginFlow::idle();
        let data = GreeterData {
            flow: &flow,
            sessions: &sessions,
        };
        // The button is the third of three stacked rows below the heading (primary field, session
        // picker, button), each `FIELD_HEIGHT` tall with `ROW_GAP` between — computed from the
        // same named constants `draw` itself uses (scale is `Scale::ONE` in this harness) so this
        // stays correct if the layout is retuned, rather than a hand-guessed pixel coordinate.
        let card_x = (800 - CARD_WIDTH) / 2;
        let card_y = (600 - CARD_HEIGHT) / 2;
        let inner_top = card_y + pixelkit_ui::Theme::default().padding;
        let button_top = inner_top + HEADING_HEIGHT + ROW_GAP + 2 * (FIELD_HEIGHT + ROW_GAP);
        h.input.cursor_moved(
            (card_x + CARD_WIDTH / 2) as f32,
            (button_top + FIELD_HEIGHT / 2) as f32,
        );
        h.input.mouse(MouseButton::Left, true);
        let action = h.frame(|ui| screen.draw(ui, area(), &data));
        assert_eq!(action, GreeterAction::SubmitUsername("bob".to_owned()));
    }

    #[test]
    fn clear_response_empties_the_field() {
        let mut screen = GreeterScreen::default();
        screen.response.set_text("hunter2");
        screen.clear_response();
        assert!(screen.response.is_empty());
    }

    /// Rule 3 of the task brief, checked at the UI-state level this time: build a screen with a
    /// distinctive secret sitting in the response field (exactly the state a live password prompt
    /// leaves it in) and confirm `Debug`-formatting the whole screen never shows it.
    #[test]
    fn debug_output_never_contains_the_typed_response() {
        let mut screen = GreeterScreen::default();
        screen.response.set_text("hunter2-do-not-log-me");
        let debug_output = format!("{screen:?}");
        assert!(!debug_output.contains("hunter2-do-not-log-me"));
        assert!(debug_output.contains("redacted"));
    }

    #[test]
    fn debug_output_still_shows_the_username_which_is_not_secret() {
        let mut screen = GreeterScreen::default();
        screen.username.set_text("alice");
        let debug_output = format!("{screen:?}");
        assert!(debug_output.contains("alice"));
    }
}
