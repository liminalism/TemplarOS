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
use lege_window::{CommandContext, WindowCommand};
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

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
}

/// Which [`RecoveryAction`], if any, `keysym` held with exactly `modifiers` performs. Chosen to
/// need no logo/"super" key (not present on every keyboard, and this table must work on any
/// keyboard by design) and to collide with nothing an ordinary application binds:
///
/// | Combination           | Action                    |
/// |------------------------|---------------------------|
/// | Ctrl+Alt+Escape        | escape fullscreen/modal   |
/// | Ctrl+Alt+Space         | show every window         |
/// | Alt+Tab                | cycle switcher forward    |
/// | Alt+Shift+Tab           | cycle switcher backward   |
pub fn recovery_action(modifiers: ModifiersState, keysym: Keysym) -> Option<RecoveryAction> {
    let raw = keysym.raw();
    if modifiers.ctrl && modifiers.alt && !modifiers.shift && !modifiers.logo {
        if raw == keysyms::KEY_Escape {
            return Some(RecoveryAction::EscapeFullscreenOrModal);
        }
        if raw == keysyms::KEY_space {
            return Some(RecoveryAction::ShowAllWindows);
        }
    }
    if modifiers.alt && !modifiers.ctrl && !modifiers.logo && raw == keysyms::KEY_Tab {
        return Some(if modifiers.shift {
            RecoveryAction::CycleFocusBackward
        } else {
            RecoveryAction::CycleFocusForward
        });
    }
    None
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
                                .filter(|window| !window.shelved)
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

    #[test]
    fn ctrl_alt_escape_escapes_fullscreen_or_modal() {
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_Escape)),
            Some(RecoveryAction::EscapeFullscreenOrModal)
        );
    }

    #[test]
    fn ctrl_alt_space_shows_every_window() {
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_space)),
            Some(RecoveryAction::ShowAllWindows)
        );
    }

    #[test]
    fn alt_tab_cycles_forward_and_shift_reverses_it() {
        assert_eq!(
            recovery_action(mods(false, true, false), Keysym::new(keysyms::KEY_Tab)),
            Some(RecoveryAction::CycleFocusForward)
        );
        assert_eq!(
            recovery_action(mods(false, true, true), Keysym::new(keysyms::KEY_Tab)),
            Some(RecoveryAction::CycleFocusBackward)
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

    #[test]
    fn plain_escape_and_unrelated_combinations_are_not_reserved() {
        assert_eq!(
            recovery_action(mods(false, false, false), Keysym::new(keysyms::KEY_Escape)),
            None
        );
        assert_eq!(
            recovery_action(mods(true, false, false), Keysym::new(keysyms::KEY_Escape)),
            None
        );
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_a)),
            None
        );
    }
}
