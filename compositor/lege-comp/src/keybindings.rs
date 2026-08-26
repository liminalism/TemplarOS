//! Fixed recovery keybindings (`docs/architecture/accessibility.md`, COMPOSITOR.md's keybinding
//! row): show every window, escape fullscreen/modal, cycle the minimal fallback switcher. These
//! call `lege_window::WindowService` directly from the compositor's own input handler
//! (`src/input.rs`) — never through `lege_shell_v1`, and never gated on a shell connection
//! existing — because their entire purpose is to keep working with `lege-shell` dead.
//!
//! The keysym-to-action table is a pure function so it is testable without a keyboard, an xkb
//! context or a compositor: [`recovery_action`] takes the modifier state and resolved keysym
//! `keyboard.input(...)`'s filter closure already has in hand and returns which action, if any,
//! the combination performs.

use crate::state::{Backend, LegeCompState};
use lege_window::{CommandContext, CommandEffect, RevealHome, WindowCommand};
use smithay::{
    backend::input::KeyState,
    input::keyboard::{Keysym, ModifiersState, keysyms},
};

/// One compositor-reserved recovery action, independent of whether a shell is connected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    /// `WindowService::handle_command(ShowAllWindows, ..)` (`geometry::scatter_layout`).
    ShowAllWindows,
    /// `WindowService::handle_command(EscapeFullscreenOrModal { window_id: None }, ..)`.
    EscapeFullscreenOrModal,
    /// `WindowService::cycle_focus(current, forward: true)`.
    CycleFocusForward,
    /// `WindowService::cycle_focus(current, forward: false)`.
    CycleFocusBackward,
    /// "Go home": toggle the reveal that hides every client window behind the Home Surface
    /// (`WindowCommand::RevealHome`).
    RevealHome,
    /// Move to the next window of this contiguous group, spatially
    /// (`geometry::contiguous_window`).
    GroupNext,
    GroupPrevious,
}

/// How many spatial steps make one contiguous group. A first rule rather than a settled one —
/// it lives here, at the call site, so changing it touches nothing in `lege-window`.
const GROUP_HOPS: u8 = 2;

/// Which [`RecoveryAction`], if any, `keysym` held with exactly `modifiers` performs while a
/// "go home" reveal is (`home_revealed`) or is not in effect. Every recovery action has a
/// binding that needs no logo/"super" key (not present on every keyboard, and this table must
/// work on any keyboard by design), colliding with nothing an ordinary application binds;
/// Super+D is an *additional* convenience for keyboards that have the key, the one arm here that
/// requires it rather than refusing it:
///
/// | Combination            | Action                    |
/// |------------------------|---------------------------|
/// | Ctrl+Alt+Escape        | escape fullscreen/modal   |
/// | Ctrl+Alt+Space         | show every window         |
/// | Alt+Tab                | cycle switcher forward    |
/// | Alt+Shift+Tab          | cycle switcher backward   |
/// | Super+Left / Super+Right | previous/next in group  |
/// | Ctrl+Alt+H             | go home (toggle reveal)   |
/// | Super (tap)            | go home (toggle reveal)   |
/// | Super+D                | go home (legacy shortcut) |
/// | Escape, while revealed | go home (ends the reveal) |
///
/// Plain Escape is reserved *only* while a reveal is in effect — the Home Surface is then the
/// only thing on screen, and every other key still reaches it — never otherwise, since
/// applications use it constantly.
pub fn recovery_action(
    modifiers: ModifiersState,
    keysym: Keysym,
    home_revealed: bool,
) -> Option<RecoveryAction> {
    let raw = keysym.raw();
    if modifiers.ctrl && modifiers.alt && !modifiers.shift && !modifiers.logo {
        if raw == keysyms::KEY_Escape {
            return Some(RecoveryAction::EscapeFullscreenOrModal);
        }
        if raw == keysyms::KEY_space {
            return Some(RecoveryAction::ShowAllWindows);
        }
        if raw == keysyms::KEY_h || raw == keysyms::KEY_H {
            return Some(RecoveryAction::RevealHome);
        }
    }
    if modifiers.alt && !modifiers.ctrl && !modifiers.logo && raw == keysyms::KEY_Tab {
        return Some(if modifiers.shift {
            RecoveryAction::CycleFocusBackward
        } else {
            RecoveryAction::CycleFocusForward
        });
    }
    if modifiers.logo && !modifiers.ctrl && !modifiers.alt && !modifiers.shift {
        if raw == keysyms::KEY_d || raw == keysyms::KEY_D {
            return Some(RecoveryAction::RevealHome);
        }
        if raw == keysyms::KEY_Left {
            return Some(RecoveryAction::GroupPrevious);
        }
        if raw == keysyms::KEY_Right {
            return Some(RecoveryAction::GroupNext);
        }
    }
    if home_revealed
        && !modifiers.ctrl
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.logo
        && raw == keysyms::KEY_Escape
    {
        return Some(RecoveryAction::RevealHome);
    }
    None
}

/// Whether this is either physical Super/logo key. The input handler uses this to implement a
/// release-time tap: a lone Super toggles Home, while Super held as part of a future chord does
/// not fire the tap action.
pub fn is_super_key(keysym: Keysym) -> bool {
    matches!(keysym.raw(), keysyms::KEY_Super_L | keysyms::KEY_Super_R)
}

/// Result of feeding one keyboard transition into [`SuperTapTracker`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperTapDecision {
    /// This transition is unrelated to the bare-Super gesture and follows the normal key path.
    Forward,
    /// Consume a Super transition, but do not toggle Home yet.
    Intercept,
    /// The last held Super key was released without any other key participating.
    ToggleHome,
}

/// Tracks a bare-Super tap without mistaking Super chords (or two held Super keys) for taps.
///
/// Both physical logo keys are tracked independently. Pressing any non-Super key while either is
/// down cancels the gesture; releasing the final Super then remains intercepted but does nothing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SuperTapTracker {
    left_down: bool,
    right_down: bool,
    armed: bool,
}

impl SuperTapTracker {
    pub fn update(&mut self, state: KeyState, keysym: Keysym) -> SuperTapDecision {
        if !is_super_key(keysym) {
            if state == KeyState::Pressed && (self.left_down || self.right_down) {
                self.armed = false;
            }
            return SuperTapDecision::Forward;
        }
        let left = keysym.raw() == keysyms::KEY_Super_L;

        match state {
            KeyState::Pressed => {
                if !self.left_down && !self.right_down {
                    self.armed = true;
                }
                if left {
                    self.left_down = true;
                } else {
                    self.right_down = true;
                }
                SuperTapDecision::Intercept
            }
            KeyState::Released => {
                if left {
                    self.left_down = false;
                } else {
                    self.right_down = false;
                }
                if !self.left_down && !self.right_down && std::mem::take(&mut self.armed) {
                    SuperTapDecision::ToggleHome
                } else {
                    SuperTapDecision::Intercept
                }
            }
        }
    }
}

/// Keyboard navigation for the pressure overlay's four buttons
/// (`lege_recovery_v1.show_pressure_overlay`) while it is showing — captured entirely by
/// `src/input.rs::on_keyboard` before any key can reach a client, exactly like [`RecoveryAction`]
/// but scoped to "the overlay is up" rather than a fixed global combination, since Left/Right/
/// Enter are ordinary keys every application also uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayAction {
    MovePrevious,
    MoveNext,
    Confirm,
}

/// Which [`OverlayAction`], if any, `keysym` performs while the pressure overlay is shown. Left
/// and Up both step backward, Right/Down/Tab both step forward — matching the two-axis layout
/// (`chrome::layout::pressure_overlay_buttons`'s row-or-grid shape) without requiring the user to
/// know which one is currently in effect for this output's size.
pub fn overlay_action(modifiers: ModifiersState, keysym: Keysym) -> Option<OverlayAction> {
    let raw = keysym.raw();
    if raw == keysyms::KEY_Left || raw == keysyms::KEY_Up {
        return Some(OverlayAction::MovePrevious);
    }
    if raw == keysyms::KEY_Right || raw == keysyms::KEY_Down {
        return Some(OverlayAction::MoveNext);
    }
    if raw == keysyms::KEY_Tab {
        return Some(if modifiers.shift {
            OverlayAction::MovePrevious
        } else {
            OverlayAction::MoveNext
        });
    }
    if raw == keysyms::KEY_Return || raw == keysyms::KEY_KP_Enter || raw == keysyms::KEY_space {
        return Some(OverlayAction::Confirm);
    }
    None
}

impl<B: Backend + 'static> LegeCompState<B> {
    /// Run `action` straight against `WindowService`, exactly as COMPOSITOR.md's keybinding row
    /// specifies: `handle_command` with `trusted_shell: true` because the *compositor itself* is
    /// the caller (there is no wire request here to gate — the trust boundary this crosses is
    /// "the fixed key combination reached the compositor's own input handler", not a protocol
    /// connection), and `cycle_focus` called directly rather than through any command.
    pub(crate) fn run_recovery_action(&mut self, action: RecoveryAction) {
        let ctx = CommandContext {
            trusted_shell: true,
            now_monotonic_ns: crate::window_bridge::now_monotonic_ns(&self.clock),
        };
        match action {
            RecoveryAction::ShowAllWindows => {
                if self.show_all_windows.take().is_none()
                    && let Ok(lege_window::CommandEffect::ShowAllWindows { layout }) = self
                        .window_service
                        .handle_command(WindowCommand::ShowAllWindows, ctx)
                {
                    self.show_all_windows = Some(layout);
                }
                crate::space_sync::sync_after_change(self);
            }
            RecoveryAction::RevealHome => self.reveal_home(RevealHome::Toggle),
            RecoveryAction::EscapeFullscreenOrModal => {
                if let Ok(lege_window::CommandEffect::Escaped { action }) =
                    self.window_service.handle_command(
                        WindowCommand::EscapeFullscreenOrModal { window_id: None },
                        ctx,
                    )
                {
                    tracing::debug!(?action, "recovery: escaped fullscreen/modal");
                    crate::space_sync::sync_after_change(self);
                }
            }
            RecoveryAction::GroupNext | RecoveryAction::GroupPrevious => {
                let direction = if matches!(action, RecoveryAction::GroupNext) {
                    lege_window::geometry::Direction::Right
                } else {
                    lege_window::geometry::Direction::Left
                };
                // Only windows a person can actually see and reach: shelved ones are parked on
                // Home by definition, and a spatial step onto something invisible would be a
                // window vanishing under the user's hands.
                let windows: Vec<(lege_window::WindowId, lege_window::Rect)> = self
                    .window_service
                    .windows()
                    .filter(|window| {
                        self.window_service.is_client_window(window.id) && !window.shelved
                    })
                    .map(|window| (window.id, window.geometry))
                    .collect();
                let current = windows
                    .iter()
                    .find(|(id, _)| {
                        self.window_service
                            .window(*id)
                            .is_some_and(|window| window.activated)
                    })
                    .map(|(id, _)| *id);
                if let Some(current) = current
                    && let Some(next) = lege_window::geometry::contiguous_window(
                        &windows, current, direction, GROUP_HOPS,
                    )
                {
                    crate::input::activate_window(self, next);
                }
            }
            RecoveryAction::CycleFocusForward | RecoveryAction::CycleFocusBackward => {
                let current = self
                    .window_service
                    .windows()
                    .find(|window| window.activated)
                    .map(|window| window.id);
                let forward = matches!(action, RecoveryAction::CycleFocusForward);
                if let Some(next) = self.window_service.cycle_focus(current, forward) {
                    crate::input::activate_window(self, next);
                    // Draw the compositor-owned fallback switcher HUD (`src/chrome/mod.rs`) so
                    // cycling is visible without the shell — stacking order for a stable,
                    // deterministic list (`WindowService::windows()`'s own order is not
                    // guaranteed). `src/input.rs` clears this on the next input that is not
                    // itself another switcher step.
                    let entries = self
                        .window_service
                        .stacking_order()
                        .into_iter()
                        .filter_map(|id| {
                            self.window_service
                                .window(id)
                                .filter(|window| {
                                    self.window_service.is_client_window(id) && !window.shelved
                                })
                                .map(|window| (id, window.title.clone()))
                        })
                        .collect();
                    self.switcher = Some(crate::chrome::SwitcherState {
                        entries,
                        selected: next,
                    });
                }
            }
        }
    }

    /// "Go home" from either route — the keybindings above or the shell's
    /// `lege_shell_manager_v1.reveal_home`. The command is `WindowService`'s; what is left here
    /// is the compositor-side consequence: the
    /// "show every window" scatter and the switcher HUD cannot coexist with a reveal, the
    /// `Space` mirror must follow the new visibility, and the keyboard must go where the user
    /// can see (`crate::input::refocus_after_reveal`).
    pub(crate) fn reveal_home(&mut self, reveal: RevealHome) {
        let ctx = CommandContext {
            trusted_shell: true,
            now_monotonic_ns: crate::window_bridge::now_monotonic_ns(&self.clock),
        };
        let Ok(CommandEffect::HomeRevealed { revealed }) = self
            .window_service
            .handle_command(WindowCommand::RevealHome { reveal }, ctx)
        else {
            return;
        };
        if revealed {
            self.show_all_windows = None;
            self.switcher = None;
        }
        crate::space_sync::sync_after_change(self);
        crate::input::refocus_after_reveal(self, revealed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool) -> ModifiersState {
        ModifiersState {
            ctrl,
            alt,
            shift,
            ..Default::default()
        }
    }

    fn logo() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..Default::default()
        }
    }

    #[test]
    fn ctrl_alt_escape_escapes_fullscreen_or_modal() {
        assert_eq!(
            recovery_action(
                mods(true, true, false),
                Keysym::new(keysyms::KEY_Escape),
                false
            ),
            Some(RecoveryAction::EscapeFullscreenOrModal)
        );
    }

    #[test]
    fn ctrl_alt_space_shows_every_window() {
        assert_eq!(
            recovery_action(
                mods(true, true, false),
                Keysym::new(keysyms::KEY_space),
                false
            ),
            Some(RecoveryAction::ShowAllWindows)
        );
    }

    #[test]
    fn alt_tab_cycles_forward_and_shift_reverses_it() {
        assert_eq!(
            recovery_action(
                mods(false, true, false),
                Keysym::new(keysyms::KEY_Tab),
                false
            ),
            Some(RecoveryAction::CycleFocusForward)
        );
        assert_eq!(
            recovery_action(
                mods(false, true, true),
                Keysym::new(keysyms::KEY_Tab),
                false
            ),
            Some(RecoveryAction::CycleFocusBackward)
        );
    }

    #[test]
    fn ctrl_alt_h_goes_home_on_any_keyboard() {
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_h), false),
            Some(RecoveryAction::RevealHome)
        );
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_H), true),
            Some(RecoveryAction::RevealHome)
        );
    }

    #[test]
    fn super_d_goes_home_and_is_the_only_logo_binding() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_d), false),
            Some(RecoveryAction::RevealHome)
        );
        // The logo key never changes the meaning of the any-keyboard combinations, and no other
        // logo chord is reserved.
        let with_logo = ModifiersState {
            ctrl: true,
            alt: true,
            logo: true,
            ..Default::default()
        };
        assert_eq!(
            recovery_action(with_logo, Keysym::new(keysyms::KEY_space), false),
            None
        );
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_a), false),
            None
        );
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_Escape), false),
            None
        );
    }

    #[test]
    fn both_super_keys_are_recognised_for_the_bare_tap() {
        assert!(is_super_key(Keysym::new(keysyms::KEY_Super_L)));
        assert!(is_super_key(Keysym::new(keysyms::KEY_Super_R)));
        assert!(!is_super_key(Keysym::new(keysyms::KEY_Control_L)));
    }

    #[test]
    fn a_bare_super_press_toggles_only_on_release() {
        let mut tap = SuperTapTracker::default();
        assert_eq!(
            tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Super_L)),
            SuperTapDecision::Intercept
        );
        assert_eq!(
            tap.update(KeyState::Released, Keysym::new(keysyms::KEY_Super_L)),
            SuperTapDecision::ToggleHome
        );
    }

    #[test]
    fn a_super_chord_cancels_the_tap_without_leaking_super_releases() {
        let mut tap = SuperTapTracker::default();
        tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Super_L));
        assert_eq!(
            tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_d)),
            SuperTapDecision::Forward
        );
        assert_eq!(
            tap.update(KeyState::Released, Keysym::new(keysyms::KEY_Super_L)),
            SuperTapDecision::Intercept
        );
    }

    #[test]
    fn two_held_super_keys_toggle_once_when_the_last_one_is_released() {
        let mut tap = SuperTapTracker::default();
        tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Super_L));
        tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Super_R));
        assert_eq!(
            tap.update(KeyState::Released, Keysym::new(keysyms::KEY_Super_L)),
            SuperTapDecision::Intercept
        );
        assert_eq!(
            tap.update(KeyState::Released, Keysym::new(keysyms::KEY_Super_R)),
            SuperTapDecision::ToggleHome
        );
    }

    #[test]
    fn plain_escape_ends_a_reveal_and_is_otherwise_free() {
        let none = mods(false, false, false);
        assert_eq!(
            recovery_action(none, Keysym::new(keysyms::KEY_Escape), true),
            Some(RecoveryAction::RevealHome)
        );
        assert_eq!(
            recovery_action(none, Keysym::new(keysyms::KEY_Escape), false),
            None
        );
        // Only bare Escape: a modified Escape while revealed is not stolen either.
        assert_eq!(
            recovery_action(
                mods(false, false, true),
                Keysym::new(keysyms::KEY_Escape),
                true
            ),
            None
        );
    }

    #[test]
    fn overlay_arrows_and_tab_move_the_selection_in_both_directions() {
        let none = mods(false, false, false);
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_Right)),
            Some(OverlayAction::MoveNext)
        );
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_Down)),
            Some(OverlayAction::MoveNext)
        );
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_Left)),
            Some(OverlayAction::MovePrevious)
        );
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_Up)),
            Some(OverlayAction::MovePrevious)
        );
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_Tab)),
            Some(OverlayAction::MoveNext)
        );
        assert_eq!(
            overlay_action(mods(false, false, true), Keysym::new(keysyms::KEY_Tab)),
            Some(OverlayAction::MovePrevious)
        );
    }

    #[test]
    fn overlay_enter_and_space_confirm() {
        let none = mods(false, false, false);
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_Return)),
            Some(OverlayAction::Confirm)
        );
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_KP_Enter)),
            Some(OverlayAction::Confirm)
        );
        assert_eq!(
            overlay_action(none, Keysym::new(keysyms::KEY_space)),
            Some(OverlayAction::Confirm)
        );
    }

    #[test]
    fn overlay_action_ignores_unrelated_keys() {
        assert_eq!(
            overlay_action(mods(false, false, false), Keysym::new(keysyms::KEY_a)),
            None
        );
        assert_eq!(
            overlay_action(mods(false, false, false), Keysym::new(keysyms::KEY_Escape)),
            None
        );
    }

    /// Every chord this compositor reserves is also *declared* in `lege_types::action`, and the
    /// two agree.
    ///
    /// The compositor deliberately does not read the manifest at run time. These bindings exist
    /// to keep working when the shell — and the whole session bus — is dead, which is the entire
    /// reason `recovery_action` is a fixed table of keysyms with no lookup in it. What the
    /// manifest gives is a *description* of the same facts, so the menu and the cheat sheet can
    /// show a person what `Ctrl+Alt+Space` does without a second, hand-maintained copy that
    /// quietly drifts. This test is the seam that keeps the description true.
    #[test]
    fn the_reserved_chords_match_what_the_action_manifest_declares() {
        use lege_types::action::{self, Chord, Key};

        /// The keysym and modifier state a declared chord stands for.
        fn resolve(chord: Chord) -> Option<(ModifiersState, Keysym)> {
            let raw = match chord.key {
                Key::Escape => keysyms::KEY_Escape,
                Key::Space => keysyms::KEY_space,
                Key::Tab => keysyms::KEY_Tab,
                Key::Left => keysyms::KEY_Left,
                Key::Right => keysyms::KEY_Right,
                Key::Character('h') => keysyms::KEY_h,
                Key::Character('d') => keysyms::KEY_d,
                // Not a combination the keysym table can answer: the bare tap is a gesture,
                // tracked by `SuperTapTracker`, and the rest belong to the shell.
                _ => return None,
            };
            let modifiers = ModifiersState {
                ctrl: chord.modifiers.ctrl,
                alt: chord.modifiers.alt,
                shift: chord.modifiers.shift,
                logo: chord.modifiers.logo,
                ..Default::default()
            };
            Some((modifiers, Keysym::new(raw)))
        }

        // Which manifest action each compositor-side recovery action is the dispatch for.
        let expected = [
            (RecoveryAction::ShowAllWindows, "window.show-all"),
            (
                RecoveryAction::EscapeFullscreenOrModal,
                "window.escape-fullscreen",
            ),
            (RecoveryAction::CycleFocusForward, "window.cycle-forward"),
            (RecoveryAction::CycleFocusBackward, "window.cycle-backward"),
            (RecoveryAction::RevealHome, "home.reveal"),
            (RecoveryAction::GroupNext, "window.group-next"),
            (RecoveryAction::GroupPrevious, "window.group-previous"),
        ];
        for (action, id) in expected {
            let declaration = action::action(id).unwrap_or_else(|| panic!("{id} is not declared"));
            let mut checked = 0;
            for chord in declaration.chords {
                let Some((modifiers, keysym)) = resolve(*chord) else {
                    continue;
                };
                assert_eq!(
                    recovery_action(modifiers, keysym, false),
                    Some(action),
                    "the manifest says {chord} performs {id}, but the compositor disagrees"
                );
                checked += 1;
            }
            assert!(checked > 0, "no chord of {id} was checkable");
        }

        // The bare Super tap is declared as Home's primary route and must stay a gesture, not a
        // combination — `SuperTapTracker` owns it, and `recovery_action` must not claim it.
        assert!(
            action::action("home.reveal")
                .is_some_and(|home| home.chords.contains(&Chord::bare(Key::LogoTap))),
            "the primary route to Home is undeclared"
        );
        assert_eq!(
            recovery_action(
                mods(false, false, false),
                Keysym::new(keysyms::KEY_h),
                false
            ),
            None,
            "an unmodified key is never reserved"
        );
    }

    #[test]
    fn super_arrows_step_through_a_contiguous_group_and_compose_with_the_bare_tap() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_Right), false),
            Some(RecoveryAction::GroupNext)
        );
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_Left), false),
            Some(RecoveryAction::GroupPrevious)
        );
        // Without the logo key these are ordinary arrows and must stay so.
        assert_eq!(
            recovery_action(
                mods(false, false, false),
                Keysym::new(keysyms::KEY_Right),
                false
            ),
            None
        );
        // And a modified Super+Arrow is not ours either.
        let logo_shift = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            recovery_action(logo_shift, Keysym::new(keysyms::KEY_Right), false),
            None
        );

        // The arrow arriving while Super is held cancels the bare-Super tap, so releasing Super
        // afterwards does not also toggle Home — the same mechanism `Super+D` already relies on.
        let mut tap = SuperTapTracker::default();
        tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Super_L));
        assert_eq!(
            tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Right)),
            SuperTapDecision::Forward
        );
        assert_eq!(
            tap.update(KeyState::Released, Keysym::new(keysyms::KEY_Super_L)),
            SuperTapDecision::Intercept,
            "a Super+Arrow chord must not fall through to a Home toggle"
        );
    }

    #[test]
    fn plain_escape_and_unrelated_combinations_are_not_reserved() {
        assert_eq!(
            recovery_action(
                mods(false, false, false),
                Keysym::new(keysyms::KEY_Escape),
                false
            ),
            None
        );
        assert_eq!(
            recovery_action(
                mods(true, false, false),
                Keysym::new(keysyms::KEY_Escape),
                false
            ),
            None
        );
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_a), false),
            None
        );
    }
}
