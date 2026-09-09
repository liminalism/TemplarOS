//! Fixed recovery keybindings (`docs/architecture/accessibility.md`, COMPOSITOR.md's keybinding
//! row): show every window, escape fullscreen/modal, cycle the minimal fallback switcher. These
//! call `templar_window::WindowService` directly from the compositor's own input handler
//! (`src/input.rs`) — never through `templar_shell_v1`, and never gated on a shell connection
//! existing — because their entire purpose is to keep working with `templar-shell` dead.
//!
//! The keysym-to-action table is a pure function so it is testable without a keyboard, an xkb
//! context or a compositor: [`recovery_action`] takes the modifier state and resolved keysym
//! `keyboard.input(...)`'s filter closure already has in hand and returns which action, if any,
//! the combination performs.

use crate::state::{Backend, TemplarCompState};
use smithay::{
    backend::input::KeyState,
    input::keyboard::{Keysym, ModifiersState, keysyms},
    // For `TemplarShellManagerV1::version` — the terminal chord's `terminal_requested` is a
    // `since = "5"` event and is guarded on the bound object's version like every other one.
    reexports::wayland_server::Resource,
};
use templar_window::{CommandContext, CommandEffect, RevealHome, WindowCommand};

/// One compositor-reserved recovery action, independent of whether a shell is connected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    /// `WindowService::handle_command(ShowAllWindows, ..)` — unshelve everything, end any reveal
    /// — and then [`RecoveryAction::OpenTaskfield`]. The recovery spelling of the overview: the
    /// one chord that gets *every* window back on screen, shelf included, with the shell dead.
    ShowAllWindows,
    /// `WindowService::handle_command(EscapeFullscreenOrModal { window_id: None }, ..)`.
    EscapeFullscreenOrModal,
    /// `Alt+Tab`: the next window in `WindowService::switch_order` — every unshelved client
    /// window, most-recently-activated first — walked as a list frozen for the length of the
    /// hold (`TemplarCompState::step_switcher`). There is no within-group cycling any more: scoping
    /// this to the focused window's group made a group a place Alt+Tab could not leave, which is
    /// the opposite of what a person pressing it wants.
    CycleFocusForward,
    /// `Alt+Shift+Tab`: the previous window in the same frozen list.
    CycleFocusBackward,
    /// `Super+Tab`: open the window overview (`WindowService::enter_taskfield`), or close it
    /// again. It used to cycle between window *groups*; the overview answers that question by
    /// showing every group as a section, which is the same question asked properly.
    OpenTaskfield,
    /// `` Alt+` ``: the next window of the focused window's *unit* — its group when it is grouped,
    /// its application otherwise (`templar_window::task::within_unit`) — walked as a frozen ring
    /// exactly like `Alt+Tab`. The chord reads as "this one"; `Alt+Tab` is never scoped.
    WithinUnitForward,
    /// `` Alt+Shift+` ``: the previous window of that unit.
    WithinUnitBackward,
    /// `Super+Space`: reveal Home and arm its Apps bar (`templar_shell_manager_v1.launcher_focus`,
    /// `since=8`). Not a second launcher — one launcher object, one more route into it.
    RevealLauncher,
    /// `Super+G`: group the focused window with the previously focused one and set it down
    /// beside it (`src/window_groups.rs`, planning doc §2 rules 1 and 6).
    GroupWithPrevious,
    /// `Super+Shift+G`: take the focused window out of its group.
    UngroupFocused,
    /// "Go home": toggle the reveal that hides every client window behind the Home Surface
    /// (`WindowCommand::RevealHome`).
    RevealHome,
    /// Step to the next window to the right, spatially (`geometry::contiguous_window`). Stores
    /// nothing: this is a way of *reaching* a neighbour, not a way of relating two windows —
    /// that is what a window group is (`RecoveryAction::GroupWithPrevious`).
    StepRight,
    StepLeft,
    /// Park the activated window onto Home's shelf.
    ParkFocused,
    /// `Super+grave`: show the drop-down terminal, or put it away again (§4.6). Never closes it.
    ToggleTerminal,
    /// A key pressed while the window overview owns the keyboard ([`taskfield_key`]).
    Taskfield(TaskfieldKey),
}

/// What a key does while `Presentation::Taskfield` is up. The mode is modal — every key is the
/// compositor's, none reaches a client — so this table is total in the sense that matters: a key
/// it does not name is swallowed rather than delivered somewhere the user cannot see.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskfieldKey {
    /// An arrow: the next card in reading order (`templar_window::taskfield::step`).
    Step(templar_window::Direction),
    /// Enter: go to the selected window, and leave. The one act of the mode that activates
    /// anything — leaving any other way activates nothing at all.
    Activate,
    /// Escape: clear the query if there is one, else leave, changing nothing.
    Escape,
    /// Delete: *ask* the selected window's client to close (`xdg_toplevel.close`). Asking, never
    /// killing: a client with unsaved work gets its own say, exactly as the band's close tag does.
    Close,
    /// A printable character typed into the query.
    Type(char),
    /// Backspace: one character back out of it.
    Backspace,
}

/// Which [`TaskfieldKey`], if any, `keysym` performs. Consulted only after
/// [`recovery_action`] has declined the key, so the reserved combinations — `Super+Tab` to leave,
/// `Ctrl+Alt+Escape`, the Home routes — keep working inside the mode and are not swallowed by it.
#[must_use]
pub fn taskfield_key(modifiers: ModifiersState, keysym: Keysym) -> Option<TaskfieldKey> {
    use templar_window::Direction;
    if modifiers.ctrl || modifiers.alt || modifiers.logo {
        return None;
    }
    match keysym.raw() {
        keysyms::KEY_Left => return Some(TaskfieldKey::Step(Direction::Left)),
        keysyms::KEY_Right => return Some(TaskfieldKey::Step(Direction::Right)),
        keysyms::KEY_Up => return Some(TaskfieldKey::Step(Direction::Up)),
        keysyms::KEY_Down => return Some(TaskfieldKey::Step(Direction::Down)),
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => return Some(TaskfieldKey::Activate),
        keysyms::KEY_Escape => return Some(TaskfieldKey::Escape),
        keysyms::KEY_Delete | keysyms::KEY_KP_Delete => return Some(TaskfieldKey::Close),
        keysyms::KEY_BackSpace => return Some(TaskfieldKey::Backspace),
        _ => {}
    }
    // Anything that produces a character goes into the query — including a space, which is why
    // this is checked after the named keys and not by `char::is_alphanumeric`.
    keysym
        .key_char()
        .filter(|character| !character.is_control())
        .map(TaskfieldKey::Type)
}

/// How many spatial hops one step may take to find the next window along. A first rule rather
/// than a settled one — it lives here, at the call site, so changing it touches nothing in
/// `templar-window`.
const STEP_HOPS: u8 = 2;

/// Which [`RecoveryAction`], if any, `keysym` held with exactly `modifiers` performs while a
/// "go home" reveal is (`home_revealed`) or is not in effect. Every *recovery* action has a
/// binding that needs no logo/"super" key (not present on every keyboard, and this table must
/// work on any keyboard by design), colliding with nothing an ordinary application binds. The
/// logo chords are the additions for keyboards that have the key: Super+D for Home, the spatial
/// step, and the window-group chords, which are conveniences rather than recovery routes — a
/// keyboard with no logo key can still reach every window through Alt+Tab and the shelf:
///
/// | Combination            | Action                    |
/// |------------------------|---------------------------|
/// | Ctrl+Alt+Escape        | escape fullscreen/modal   |
/// | Ctrl+Alt+Space         | show every window         |
/// | Alt+Tab                | next window               |
/// | Alt+Shift+Tab          | previous window           |
/// | Super+Tab              | the window overview       |
/// | Alt+`                  | next window here          |
/// | Alt+Shift+`            | previous window here      |
/// | Super+Space            | Home, with the launcher armed |
/// | Super+G                | group with the last window|
/// | Super+Shift+G          | leave the group           |
/// | Super+Left / Super+Right | step left / step right    |
/// | Super+`                | show/hide the terminal    |
/// | Ctrl+Alt+H             | go home (toggle reveal)   |
/// | Ctrl+Alt+P             | park the active window    |
/// | Super (press)          | reveal Home immediately   |
/// | Super (held)           | Home stays, momentarily   |
/// | Super (tap release)    | latch the toggled reveal  |
/// | Super (hold release)   | restore pre-press reveal  |
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
        if raw == keysyms::KEY_p || raw == keysyms::KEY_P {
            return Some(RecoveryAction::ParkFocused);
        }
    }
    if modifiers.alt && !modifiers.ctrl && !modifiers.logo && is_grave(raw) {
        return Some(if modifiers.shift {
            RecoveryAction::WithinUnitBackward
        } else {
            RecoveryAction::WithinUnitForward
        });
    }
    if modifiers.alt && !modifiers.ctrl && !modifiers.logo && is_tab(raw) {
        return Some(if modifiers.shift {
            RecoveryAction::CycleFocusBackward
        } else {
            RecoveryAction::CycleFocusForward
        });
    }
    if modifiers.logo && !modifiers.ctrl && !modifiers.alt {
        // `Super+Tab` opens the overview with or without Shift: a mode has no direction, and a
        // person reaching for the backward variant of the chord they know must not land on
        // nothing. `Super+G` below still takes its Shift variant, so both are checked here rather
        // than duplicated inside the no-Shift block.
        if is_tab(raw) {
            return Some(RecoveryAction::OpenTaskfield);
        }
        if raw == keysyms::KEY_g || raw == keysyms::KEY_G {
            return Some(if modifiers.shift {
                RecoveryAction::UngroupFocused
            } else {
                RecoveryAction::GroupWithPrevious
            });
        }
        if !modifiers.shift {
            if raw == keysyms::KEY_d || raw == keysyms::KEY_D {
                return Some(RecoveryAction::RevealHome);
            }
            if raw == keysyms::KEY_space {
                return Some(RecoveryAction::RevealLauncher);
            }
            // The drop-down terminal. A bare ` or ~ cannot be a global chord — both are typed
            // characters, in the home directory, in Markdown and in every shell — so a
            // compositor grab on either would take them from every application (§4.6).
            if raw == keysyms::KEY_grave {
                return Some(RecoveryAction::ToggleTerminal);
            }
            if raw == keysyms::KEY_Left {
                return Some(RecoveryAction::StepLeft);
            }
            if raw == keysyms::KEY_Right {
                return Some(RecoveryAction::StepRight);
            }
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

/// Whether `keysym` is a Tab, **including the one Shift produces**. With a Shift level in the
/// layout, `Shift+Tab` resolves to `ISO_Left_Tab` rather than `Tab`, so a table that only looked
/// for `Tab` would silently have no backward switcher at all on an ordinary keyboard.
fn is_tab(raw: u32) -> bool {
    raw == keysyms::KEY_Tab || raw == keysyms::KEY_ISO_Left_Tab
}

/// Whether `keysym` is the grave key, **including the tilde Shift produces**. Same trap as
/// [`is_tab`]: `` Alt+Shift+` `` arrives as `asciitilde` on an ordinary layout, so a table that
/// only looked for `grave` would have no backward within-unit walk at all.
fn is_grave(raw: u32) -> bool {
    raw == keysyms::KEY_grave || raw == keysyms::KEY_asciitilde
}

/// Whether this is either physical Super/logo key. The input handler uses this to implement the
/// press-and-hold Home reveal: a bare Super shows Home the moment it goes down, while Super held
/// as part of a chord performs only the chord's action.
pub fn is_super_key(keysym: Keysym) -> bool {
    matches!(keysym.raw(), keysyms::KEY_Super_L | keysyms::KEY_Super_R)
}

/// A release this much after the first Super press is a hold (release restores the reveal state
/// that preceded the press); anything shorter is a tap (release latches the toggled state).
pub const SUPER_TAP_MS: u32 = 400;

/// Result of feeding one keyboard transition into [`SuperTapTracker`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperTapDecision {
    /// This transition is unrelated to the bare-Super gesture and follows the normal key path.
    Forward,
    /// A chord began while the momentary reveal was up: restore the pre-press reveal state
    /// (always hidden — a momentary reveal only ever starts from hidden), then handle the key
    /// normally. The chord's own action, if it is a reveal toggle, lands after the restore.
    ForwardEndingReveal,
    /// Consume a Super transition without changing the reveal.
    Intercept,
    /// The first Super went down with Home hidden: show Home now, momentarily.
    Reveal,
    /// The last held Super was released: set the reveal to exactly this state. A short tap
    /// latches the toggle of the pre-press state; a hold restores the pre-press state itself.
    SetReveal(bool),
}

/// Tracks the bare-Super gesture: press reveals Home immediately, holding keeps it as a
/// momentary surface, a short tap latches the toggle, and any chord key cancels the gesture so
/// release adds nothing to the chord's action.
///
/// Both physical logo keys are tracked independently and behave as one logical hold: the gesture
/// begins at the first press and resolves when the last of them is released.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SuperTapTracker {
    left_down: bool,
    right_down: bool,
    armed: bool,
    pressed_at_ms: u32,
    prior_revealed: bool,
    momentary: bool,
}

impl SuperTapTracker {
    /// Feed one keyboard transition. `time_ms` is the input event's millisecond timestamp;
    /// `home_revealed` is the reveal state as of this transition, sampled on the first Super
    /// press to become the baseline a hold restores.
    pub fn update(
        &mut self,
        state: KeyState,
        keysym: Keysym,
        time_ms: u32,
        home_revealed: bool,
    ) -> SuperTapDecision {
        if !is_super_key(keysym) {
            if state == KeyState::Pressed && (self.left_down || self.right_down) {
                self.armed = false;
                if std::mem::take(&mut self.momentary) {
                    return SuperTapDecision::ForwardEndingReveal;
                }
            }
            return SuperTapDecision::Forward;
        }
        let left = keysym.raw() == keysyms::KEY_Super_L;

        match state {
            KeyState::Pressed => {
                let first = !self.left_down && !self.right_down;
                if left {
                    self.left_down = true;
                } else {
                    self.right_down = true;
                }
                if first {
                    self.armed = true;
                    self.pressed_at_ms = time_ms;
                    self.prior_revealed = home_revealed;
                    if !home_revealed {
                        self.momentary = true;
                        return SuperTapDecision::Reveal;
                    }
                }
                SuperTapDecision::Intercept
            }
            KeyState::Released => {
                if left {
                    self.left_down = false;
                } else {
                    self.right_down = false;
                }
                if self.left_down || self.right_down {
                    return SuperTapDecision::Intercept;
                }
                self.momentary = false;
                if !std::mem::take(&mut self.armed) {
                    return SuperTapDecision::Intercept;
                }
                let held_ms = time_ms.wrapping_sub(self.pressed_at_ms);
                if held_ms < SUPER_TAP_MS {
                    SuperTapDecision::SetReveal(!self.prior_revealed)
                } else {
                    SuperTapDecision::SetReveal(self.prior_revealed)
                }
            }
        }
    }
}

/// Keyboard navigation for the pressure overlay's four buttons
/// (`templar_recovery_v1.show_pressure_overlay`) while it is showing — captured entirely by
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

impl<B: Backend + 'static> TemplarCompState<B> {
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
                // The recovery spelling: get *every* window back, the shelf included, and then
                // show them. `ShowAllWindows` is what unshelves and ends any reveal; the scatter
                // rectangles it also returns are the layout Taskfield replaced, and are dropped.
                if !self.leave_taskfield() {
                    let _ = self
                        .window_service
                        .handle_command(WindowCommand::ShowAllWindows, ctx);
                    self.enter_taskfield(templar_window::TaskFilter::All);
                }
                crate::space_sync::sync_after_change(self);
            }
            RecoveryAction::OpenTaskfield => {
                if !self.leave_taskfield() {
                    self.enter_taskfield(templar_window::TaskFilter::All);
                }
                crate::space_sync::sync_after_change(self);
            }
            RecoveryAction::RevealLauncher => {
                self.reveal_home(RevealHome::Show);
                crate::protocols_shell::send_launcher_focus(self);
            }
            RecoveryAction::RevealHome => self.reveal_home(RevealHome::Toggle),
            RecoveryAction::EscapeFullscreenOrModal => {
                if let Ok(templar_window::CommandEffect::Escaped { action }) =
                    self.window_service.handle_command(
                        WindowCommand::EscapeFullscreenOrModal { window_id: None },
                        ctx,
                    )
                {
                    tracing::debug!(?action, "recovery: escaped fullscreen/modal");
                    // The service has already restored the geometry; the client still has to be
                    // told the size and have its fullscreen bit cleared, or it keeps painting a
                    // whole screen into a window-sized rectangle. Idempotent: the state flip
                    // itself is already done.
                    if let templar_window::EscapeAction::ExitedFullscreen { window_id } = action {
                        crate::xdg_shell::apply_fullscreen(self, window_id, false);
                    }
                    crate::space_sync::sync_after_change(self);
                }
            }
            RecoveryAction::StepRight | RecoveryAction::StepLeft => {
                let direction = if matches!(action, RecoveryAction::StepRight) {
                    templar_window::geometry::Direction::Right
                } else {
                    templar_window::geometry::Direction::Left
                };
                // Only windows a person can actually see and reach: shelved ones are parked on
                // Home by definition, and a spatial step onto something invisible would be a
                // window vanishing under the user's hands.
                let windows: Vec<(templar_window::WindowId, templar_window::Rect)> = self
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
                    && let Some(next) = templar_window::geometry::contiguous_window(
                        &windows, current, direction, STEP_HOPS,
                    )
                {
                    crate::input::activate_window(self, next);
                }
            }
            RecoveryAction::ParkFocused => self.park_focused(ctx),
            RecoveryAction::ToggleTerminal => self.toggle_terminal(),
            RecoveryAction::GroupWithPrevious => self.group_with_previous(),
            RecoveryAction::UngroupFocused => {
                if let Some(window_id) = self.activated_window() {
                    crate::window_groups::ungroup_window(self, window_id);
                }
            }
            RecoveryAction::WithinUnitForward | RecoveryAction::WithinUnitBackward => {
                self.step_switcher(true, matches!(action, RecoveryAction::WithinUnitForward));
            }
            RecoveryAction::CycleFocusForward | RecoveryAction::CycleFocusBackward => {
                self.step_switcher(false, matches!(action, RecoveryAction::CycleFocusForward));
            }
            RecoveryAction::Taskfield(key) => self.apply_taskfield_key(key),
        }
    }

    /// One key, while the window overview owns the keyboard.
    ///
    /// Only [`TaskfieldKey::Activate`] activates anything. Leaving any other way — Escape, the
    /// chord again, a click on nothing — puts the scene back exactly as it was and hands the
    /// keyboard back to the window that already had it, because "I looked at my windows" is not
    /// "I chose one".
    fn apply_taskfield_key(&mut self, key: TaskfieldKey) {
        let templar_window::Presentation::Taskfield(taskfield) = self.window_service.presentation()
        else {
            return;
        };
        let selected = taskfield.selected;
        let changed = match key {
            TaskfieldKey::Step(direction) => {
                let next =
                    templar_window::taskfield::step(&taskfield.sections, selected, direction);
                next.is_some() && self.window_service.taskfield_selected(next)
            }
            TaskfieldKey::Activate => {
                self.leave_taskfield();
                if let Some(window_id) = selected {
                    crate::input::activate_window(self, window_id);
                }
                crate::space_sync::sync_after_change(self);
                return;
            }
            TaskfieldKey::Escape => {
                if taskfield.query.is_empty() {
                    self.leave_taskfield();
                    crate::space_sync::sync_after_change(self);
                    return;
                }
                self.window_service.taskfield_query(String::new())
            }
            TaskfieldKey::Close => {
                if let Some(window_id) = selected {
                    crate::backend::close_window(self, window_id);
                }
                // The card goes when the client actually closes the window, not when it is asked.
                false
            }
            TaskfieldKey::Type(character) => {
                let mut query = taskfield.query.clone();
                query.push(character);
                self.window_service.taskfield_query(query)
            }
            TaskfieldKey::Backspace => {
                let mut query = taskfield.query.clone();
                query.pop();
                self.window_service.taskfield_query(query)
            }
        };
        if changed {
            crate::protocols_shell::send_taskfield_state(self);
            B::request_redraw(self);
        }
    }

    /// One `Alt+Tab` (`within_unit: false`) or `` Alt+` `` (`within_unit: true`) step.
    ///
    /// The list is built at the first press and *frozen* until the modifier is released
    /// (`src/input.rs::on_keyboard` drops the ring), because every step records activation and
    /// changes the order a newly built semantic MRU list would expose. `Alt+Tab` walks
    /// `WindowService::recent()` — the seven most-recently-activated application windows, which is
    /// what "go back to what I was just doing" means; everything beyond that is the overview's
    /// question, not a chord's. `` Alt+` `` walks `task::within_unit`: the windows of the focused
    /// window's unit, its group when it is grouped and its application otherwise, so the group
    /// feature costs nothing until a group exists. `Alt+Tab` itself is never scoped to either —
    /// that made a group a place `Alt+Tab` could not leave.
    ///
    /// ponytail: each step activates as it goes, so walking past three windows raises three. The
    /// upgrade — commit only on release, as Windows does — costs a second commit path for the
    /// cases where the release is never seen; this way the keyboard is always somewhere real.
    fn step_switcher(&mut self, within_unit: bool, forward: bool) {
        let ring = match self.switch_ring.take() {
            Some((ring, walking_unit)) if walking_unit == within_unit => Some(ring),
            _ => {
                let targets = if within_unit {
                    let units = self.window_service.task_units();
                    self.activated_window()
                        .map(|current| templar_window::task::within_unit(&units, current))
                        .unwrap_or_default()
                } else {
                    let targets = self.window_service.recent();
                    for window in self.window_service.windows() {
                        if let Some(reason) = self.window_service.switch_exclusion_reason(window.id)
                        {
                            tracing::debug!(
                                window_id = window.id.0,
                                ?window.role,
                                mapped = window.mapped,
                                shelved = window.shelved,
                                fullscreen = window.fullscreen,
                                layer = ?self.window_service.layer_of(window.id),
                                reason,
                                "toplevel excluded from Alt-Tab"
                            );
                        }
                    }
                    tracing::debug!(?targets, "froze Alt-Tab targets in semantic MRU order");
                    targets
                };
                templar_window::SwitchRing::new(targets, self.activated_window())
            }
        };
        let Some(mut ring) = ring else {
            return;
        };
        let next = ring.step(forward);
        // The compositor-owned switcher HUD (`src/chrome/mod.rs`), so cycling is visible with the
        // shell dead. It draws the frozen list, which is the list actually being walked.
        self.switcher = Some(if within_unit {
            self.unit_switcher(&ring, next)
        } else {
            self.window_switcher(&ring, next)
        });
        self.switch_ring = Some((ring, within_unit));
        crate::input::activate_window(self, next);
    }

    /// The window that currently holds the keyboard, if any.
    fn activated_window(&self) -> Option<templar_window::WindowId> {
        self.window_service
            .windows()
            .find(|window| window.activated)
            .map(|window| window.id)
    }

    /// `Super+G`: group the focused window with the one focused before it, and set the focused
    /// one down beside that (planning doc §2.1). The previously focused window is the anchor, so
    /// the window the person is looking at is the one that moves — moving the *other* one would
    /// rearrange a window they are not currently attending to.
    fn group_with_previous(&mut self) {
        let Some(focused) = self.activated_window() else {
            return;
        };
        let Some(previous) = self.previous_focus.filter(|id| *id != focused) else {
            tracing::debug!("Super+G with no previously focused window to group with");
            return;
        };
        crate::window_groups::group_windows(self, previous, focused);
    }

    /// The `Alt+Tab` HUD: the frozen list, titles top-first, the current step highlighted.
    fn window_switcher(
        &self,
        ring: &templar_window::SwitchRing,
        selected: templar_window::WindowId,
    ) -> crate::chrome::SwitcherState {
        crate::chrome::SwitcherState {
            heading: None,
            accent: None,
            entries: ring
                .targets()
                .iter()
                .map(|id| {
                    let title = self
                        .window_service
                        .window(*id)
                        .map(|window| window.title.clone())
                        .unwrap_or_default();
                    (*id, title)
                })
                .collect(),
            selected,
        }
    }

    /// The `` Alt+` `` HUD: the windows of one unit, titled with the unit's own name — the same
    /// frozen list the chord is stepping through, in the same order. Titles, not counts: every row
    /// is a window here, so the count each row of the old group HUD carried has nothing to say.
    fn unit_switcher(
        &self,
        ring: &templar_window::SwitchRing,
        selected: templar_window::WindowId,
    ) -> crate::chrome::SwitcherState {
        let units = self.window_service.task_units();
        let heading =
            templar_window::task::unit_of(&units, selected).map(|unit| unit.label.clone());
        let accent = templar_window::task::unit_of(&units, selected).and_then(|unit| unit.accent);
        crate::chrome::SwitcherState {
            heading,
            accent,
            entries: ring
                .targets()
                .iter()
                .map(|id| {
                    let title = self
                        .window_service
                        .window(*id)
                        .map(|window| window.title.clone())
                        .unwrap_or_default();
                    (*id, title)
                })
                .collect(),
            selected,
        }
    }

    /// `Super+grave`, the drop-down terminal (§4.6). "The terminal" is the most recently
    /// activated window whose canonical application id is the one the shell named with
    /// `set_terminal` — most recently activated meaning topmost in the stacking order, since
    /// activating is what raises a window.
    ///
    /// No such window: ask the shell to launch the provider (`terminal_requested`), once.
    /// Shelved, or simply not focused: bring it back and focus it. Focused: park it and hand the
    /// keyboard back to the window that had it. **Never closed** — an accidental press has to
    /// cost nothing, which is the whole difference between this and a chord that quits things.
    fn toggle_terminal(&mut self) {
        let Some(window_id) = self
            .shell_protocol
            .terminal_app_id
            .clone()
            .and_then(|app_id| self.terminal_window(&app_id))
        else {
            self.request_terminal();
            return;
        };
        let Some(window) = self.window_service.window(window_id) else {
            return;
        };
        if window.activated && !window.shelved {
            self.hide_terminal(window_id);
            return;
        }
        if window.shelved {
            let ctx = CommandContext {
                trusted_shell: true,
                now_monotonic_ns: crate::window_bridge::now_monotonic_ns(&self.clock),
            };
            if let Ok(CommandEffect::Unshelved { geometry, .. }) = self
                .window_service
                .handle_command(WindowCommand::Unshelve { window_id }, ctx)
            {
                crate::space_sync::apply_resized_geometry(self, window_id, geometry, false);
            }
        }
        crate::input::activate_window(self, window_id);
    }

    /// The terminal's window: the topmost client window whose *resolved* identity is `app_id`.
    /// Identity comes from the cgroup through `DbusIdentityCache`, never from what the client
    /// called itself, so this cannot be spoofed by an application setting an `app_id`.
    fn terminal_window(&self, app_id: &str) -> Option<templar_window::WindowId> {
        self.window_service.stacking_order().into_iter().find(|id| {
            self.window_service.is_client_window(*id)
                && self.window_service.window(*id).is_some_and(|window| {
                    window
                        .identity
                        .app_id
                        .as_ref()
                        .is_some_and(|resolved| resolved.as_str() == app_id)
                })
        })
    }

    /// Park the terminal and give the keyboard back to whatever had it before the terminal took
    /// it, falling back to the topmost window still on screen — or to Home when there is none.
    fn hide_terminal(&mut self, window_id: templar_window::WindowId) {
        let ctx = CommandContext {
            trusted_shell: true,
            now_monotonic_ns: crate::window_bridge::now_monotonic_ns(&self.clock),
        };
        if self
            .window_service
            .handle_command(WindowCommand::Shelve { window_id }, ctx)
            .is_err()
        {
            return;
        }
        crate::space_sync::sync_after_change(self);
        let back = self.previous_focus.filter(|id| {
            *id != window_id
                && self.window_service.is_visible(*id)
                && self.window_service.is_client_window(*id)
        });
        if let Some(back) = back {
            crate::input::activate_window(self, back);
            return;
        }
        let any_left = self.window_service.stacking_order().into_iter().any(|id| {
            self.window_service.is_visible(id) && self.window_service.is_client_window(id)
        });
        crate::input::refocus_after_reveal(self, !any_left);
    }

    /// Ask the shell to launch the `terminal` provider. Sent to one manager, once per press: with
    /// no shell there is nothing to ask, and the chord simply does nothing — which is the honest
    /// half of "with the shell dead the chord still toggles an existing terminal; only the launch
    /// needs the shell".
    ///
    /// Sending an event straight from a keybinding does not cross COMPOSITOR.md's "never encode
    /// events synchronously from a call site that touched `WindowService`" rule: this path
    /// touched nothing — there was no window to touch.
    fn request_terminal(&mut self) {
        let Some(manager) = self
            .shell_protocol
            .managers
            .iter()
            .find(|manager| manager.version() >= 5)
        else {
            tracing::debug!("terminal chord: no terminal window, and no shell to ask for one");
            return;
        };
        manager.terminal_requested();
    }

    fn park_focused(&mut self, ctx: CommandContext) {
        let current = self
            .window_service
            .windows()
            .find(|window| window.activated && !window.shelved)
            .map(|window| window.id);
        if let Some(window_id) = current
            && self
                .window_service
                .handle_command(WindowCommand::Shelve { window_id }, ctx)
                .is_ok()
        {
            crate::space_sync::sync_after_change(self);
        }
    }

    /// Open the window overview over `filter`, and say so. The one entry point every route into
    /// the mode takes — the chord, the Peek corner's promotion, `window.show-all`, the shell's
    /// `open_taskfield` — so what is torn down on the way in cannot differ between them: a
    /// switcher walk and the overview are two answers to the same question, and a reveal has
    /// already ended inside `WindowService::enter_taskfield`.
    ///
    /// Refused, with nothing changed and nothing sent, when the filter selects no window: an
    /// overview of nothing is indistinguishable from a black screen.
    pub(crate) fn enter_taskfield(&mut self, filter: templar_window::TaskFilter) -> bool {
        if !self.window_service.enter_taskfield(filter) {
            tracing::debug!("Taskfield refused: the filter selects no window");
            return false;
        }
        let cards = match self.window_service.presentation() {
            templar_window::Presentation::Taskfield(taskfield) => taskfield.cards().count(),
            _ => 0,
        };
        tracing::info!(cards, "entering Taskfield");
        self.switcher = None;
        self.switch_ring = None;
        self.taskfield_press = None;
        crate::protocols_shell::send_taskfield_state(self);
        // Entering and leaving change *only* where windows are drawn — no `Space` membership, no
        // geometry, no client commit — so a damage-tracked backend has nothing else to notice.
        // The one place the mode has to ask for the frame it needs.
        B::request_redraw(self);
        true
    }

    /// Leave the overview, restoring the ordinary scene exactly, and say so (`active` 0). `false`
    /// when no overview was up, which is what makes every caller able to spell "close it if it is
    /// open" as one call.
    pub(crate) fn leave_taskfield(&mut self) -> bool {
        if !self.window_service.leave_taskfield() {
            return false;
        }
        tracing::info!("leaving Taskfield");
        self.taskfield_press = None;
        self.shell_protocol.taskfield_key.clear();
        crate::protocols_shell::send_taskfield_state(self);
        B::request_redraw(self);
        true
    }

    /// "Go home" from either route — the keybindings above or the shell's
    /// `templar_shell_manager_v1.reveal_home`. The command is `WindowService`'s; what is left here
    /// is the compositor-side consequence: the
    /// window overview and the switcher HUD cannot coexist with a reveal, the
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
            // Home and the overview are two answers to the same question, so one ends the other;
            // `leave_taskfield` is a no-op when no overview is up.
            self.leave_taskfield();
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
    fn super_d_goes_home_and_unclaimed_logo_chords_stay_free() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_d), false),
            Some(RecoveryAction::RevealHome)
        );
        // The logo key never changes the meaning of the any-keyboard combinations, and a logo
        // chord this table does not name is left to whatever the user is running.
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

    fn logo_shift() -> ModifiersState {
        ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        }
    }

    /// Shift+Tab is `ISO_Left_Tab` on an ordinary layout, not a shifted `Tab`. Both switchers
    /// have to know that, or "previous" quietly does nothing at all.
    #[test]
    fn shift_tab_reaches_the_backward_switcher_as_iso_left_tab_too() {
        for backward in [keysyms::KEY_Tab, keysyms::KEY_ISO_Left_Tab] {
            assert_eq!(
                recovery_action(mods(false, true, true), Keysym::new(backward), false),
                Some(RecoveryAction::CycleFocusBackward)
            );
            // `Super+Tab` is a mode, not a direction: both spellings open the overview, so a
            // person reaching for the backward variant of a chord they know lands somewhere.
            assert_eq!(
                recovery_action(logo_shift(), Keysym::new(backward), false),
                Some(RecoveryAction::OpenTaskfield)
            );
        }
        // `` Alt+Shift+` `` has the same trap: it arrives as a tilde.
        for backward in [keysyms::KEY_grave, keysyms::KEY_asciitilde] {
            assert_eq!(
                recovery_action(mods(false, true, true), Keysym::new(backward), false),
                Some(RecoveryAction::WithinUnitBackward)
            );
        }
    }

    /// `Super+Tab` opens the window overview — and `Ctrl+Tab` is reserved by nothing here,
    /// because it belongs to the browser, the terminal and the editor (planning doc §2.1).
    #[test]
    fn super_tab_opens_the_overview_and_ctrl_tab_is_never_reserved() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_Tab), false),
            Some(RecoveryAction::OpenTaskfield)
        );
        for modifiers in [mods(true, false, false), mods(true, false, true)] {
            assert_eq!(
                recovery_action(modifiers, Keysym::new(keysyms::KEY_Tab), false),
                None,
                "Ctrl+Tab must reach the application"
            );
        }
    }

    /// The overview's modal table: arrows move, Enter chooses, Escape backs out, Delete asks,
    /// and everything else printable is a query. A combination `recovery_action` claims is never
    /// reached here — the caller asks it first — which is what keeps `Super+Tab` able to close a
    /// mode that swallows every other key.
    #[test]
    fn the_overview_reads_arrows_enter_escape_and_typing() {
        use templar_window::Direction;
        let none = mods(false, false, false);
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_Right)),
            Some(TaskfieldKey::Step(Direction::Right))
        );
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_Up)),
            Some(TaskfieldKey::Step(Direction::Up))
        );
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_Return)),
            Some(TaskfieldKey::Activate)
        );
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_Escape)),
            Some(TaskfieldKey::Escape)
        );
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_Delete)),
            Some(TaskfieldKey::Close)
        );
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_BackSpace)),
            Some(TaskfieldKey::Backspace)
        );
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_f)),
            Some(TaskfieldKey::Type('f'))
        );
        // A space is a query character, not a named key — which is why the named keys are
        // checked first rather than by asking whether the character is alphanumeric.
        assert_eq!(
            taskfield_key(none, Keysym::new(keysyms::KEY_space)),
            Some(TaskfieldKey::Type(' '))
        );
        // A modified key belongs to `recovery_action` or to nothing: typing into the query is
        // what an *unmodified* key does.
        assert_eq!(taskfield_key(logo(), Keysym::new(keysyms::KEY_f)), None);
        assert_eq!(
            taskfield_key(mods(true, false, false), Keysym::new(keysyms::KEY_f)),
            None
        );
        // Shift is not a modifier here: it is how a capital letter is typed.
        assert_eq!(
            taskfield_key(mods(false, false, true), Keysym::new(keysyms::KEY_F)),
            Some(TaskfieldKey::Type('F'))
        );
    }

    #[test]
    fn super_g_groups_with_the_previous_window_and_shift_leaves_the_group() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_g), false),
            Some(RecoveryAction::GroupWithPrevious)
        );
        assert_eq!(
            recovery_action(logo_shift(), Keysym::new(keysyms::KEY_G), false),
            Some(RecoveryAction::UngroupFocused)
        );
        // Neither is reachable without the logo key: G is a letter people type.
        assert_eq!(
            recovery_action(
                mods(false, false, false),
                Keysym::new(keysyms::KEY_g),
                false
            ),
            None
        );
        assert_eq!(
            recovery_action(mods(true, true, false), Keysym::new(keysyms::KEY_g), false),
            None
        );
    }

    /// The Shift-less logo chords stay Shift-less: Super+Shift+D and Super+Shift+Right are not
    /// ours, so an application can still bind them.
    #[test]
    fn the_shiftless_logo_chords_are_not_claimed_with_shift_held() {
        for key in [
            keysyms::KEY_d,
            keysyms::KEY_Right,
            keysyms::KEY_Left,
            keysyms::KEY_grave,
        ] {
            assert_eq!(recovery_action(logo_shift(), Keysym::new(key), false), None);
        }
    }

    /// The terminal chord is `Super+grave` and nothing else: a bare backtick or tilde is a
    /// character people type constantly, and grabbing either would take it from every
    /// application (§4.6).
    #[test]
    fn super_grave_toggles_the_terminal_and_a_bare_backtick_is_never_reserved() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_grave), false),
            Some(RecoveryAction::ToggleTerminal)
        );
        for keysym in [keysyms::KEY_grave, keysyms::KEY_asciitilde] {
            assert_eq!(
                recovery_action(mods(false, false, false), Keysym::new(keysym), false),
                None
            );
            assert_eq!(
                recovery_action(mods(false, false, false), Keysym::new(keysym), true),
                None,
                "not even while Home is revealed"
            );
        }
    }

    #[test]
    fn both_super_keys_are_recognised_for_the_bare_tap() {
        assert!(is_super_key(Keysym::new(keysyms::KEY_Super_L)));
        assert!(is_super_key(Keysym::new(keysyms::KEY_Super_R)));
        assert!(!is_super_key(Keysym::new(keysyms::KEY_Control_L)));
    }

    fn super_l() -> Keysym {
        Keysym::new(keysyms::KEY_Super_L)
    }

    fn super_r() -> Keysym {
        Keysym::new(keysyms::KEY_Super_R)
    }

    #[test]
    fn bare_super_tap_latches_the_toggle_without_a_second_toggle_on_release() {
        let mut tap = SuperTapTracker::default();
        // Home hidden: press reveals immediately, a short release latches the reveal on —
        // one toggle for the whole tap, not one per transition.
        assert_eq!(
            tap.update(KeyState::Pressed, super_l(), 0, false),
            SuperTapDecision::Reveal
        );
        assert_eq!(
            tap.update(KeyState::Released, super_l(), SUPER_TAP_MS - 1, true),
            SuperTapDecision::SetReveal(true)
        );
        // Home latched: the next tap latches it off, and the press adds no second reveal.
        assert_eq!(
            tap.update(KeyState::Pressed, super_l(), 1_000, true),
            SuperTapDecision::Intercept
        );
        assert_eq!(
            tap.update(KeyState::Released, super_l(), 1_100, true),
            SuperTapDecision::SetReveal(false)
        );
    }

    #[test]
    fn super_press_reveals_immediately_and_a_held_release_restores_the_prior_state() {
        let mut tap = SuperTapTracker::default();
        // Hidden before the press: down shows Home at once, a hold's release hides it again.
        assert_eq!(
            tap.update(KeyState::Pressed, super_l(), 0, false),
            SuperTapDecision::Reveal
        );
        assert_eq!(
            tap.update(KeyState::Released, super_l(), SUPER_TAP_MS, true),
            SuperTapDecision::SetReveal(false)
        );
        // Already revealed before the press: a hold's release leaves it revealed.
        assert_eq!(
            tap.update(KeyState::Pressed, super_l(), 2_000, true),
            SuperTapDecision::Intercept
        );
        assert_eq!(
            tap.update(KeyState::Released, super_l(), 3_000, true),
            SuperTapDecision::SetReveal(true)
        );
    }

    #[test]
    fn super_chord_performs_only_its_action_and_ends_the_momentary_reveal() {
        let mut tap = SuperTapTracker::default();
        assert_eq!(
            tap.update(KeyState::Pressed, super_l(), 0, false),
            SuperTapDecision::Reveal
        );
        // The chord key both restores the pre-press reveal state and takes the normal key
        // path, so the declared accelerator fires from the restored state.
        assert_eq!(
            tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_d), 50, true),
            SuperTapDecision::ForwardEndingReveal
        );
        assert_eq!(
            tap.update(KeyState::Released, Keysym::new(keysyms::KEY_d), 80, false),
            SuperTapDecision::Forward
        );
        // Even a quick release after the chord latches nothing extra.
        assert_eq!(
            tap.update(KeyState::Released, super_l(), 100, false),
            SuperTapDecision::Intercept
        );
        // A second chord key during the same hold has no reveal left to end.
        tap.update(KeyState::Pressed, super_l(), 200, false);
        tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_d), 210, true);
        assert_eq!(
            tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_x), 220, false),
            SuperTapDecision::Forward
        );
    }

    #[test]
    fn super_hold_spans_both_logo_keys_as_one_logical_hold() {
        let mut tap = SuperTapTracker::default();
        assert_eq!(
            tap.update(KeyState::Pressed, super_l(), 0, false),
            SuperTapDecision::Reveal
        );
        assert_eq!(
            tap.update(KeyState::Pressed, super_r(), 100, true),
            SuperTapDecision::Intercept
        );
        assert_eq!(
            tap.update(KeyState::Released, super_l(), 200, true),
            SuperTapDecision::Intercept
        );
        // The hold is timed from the first press, and resolves only at the last release.
        assert_eq!(
            tap.update(KeyState::Released, super_r(), 1_000, true),
            SuperTapDecision::SetReveal(false)
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

    /// Every chord this compositor reserves is also *declared* in `templar_types::action`, and the
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
        use templar_types::action::{self, Chord, Key};

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
                Key::Character('p') => keysyms::KEY_p,
                Key::Character('g') => keysyms::KEY_g,
                Key::Grave => keysyms::KEY_grave,
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
            (RecoveryAction::StepRight, "window.step-right"),
            (RecoveryAction::StepLeft, "window.step-left"),
            (RecoveryAction::ParkFocused, "window.park"),
            (RecoveryAction::OpenTaskfield, "taskfield.open"),
            (RecoveryAction::RevealLauncher, "launcher.show-all"),
            (
                RecoveryAction::WithinUnitForward,
                "window.within-unit-forward",
            ),
            (
                RecoveryAction::WithinUnitBackward,
                "window.within-unit-backward",
            ),
            (RecoveryAction::GroupWithPrevious, "window.group"),
            (RecoveryAction::UngroupFocused, "window.ungroup"),
            (RecoveryAction::ToggleTerminal, "terminal.toggle"),
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
    fn super_arrows_step_to_the_next_window_along_and_compose_with_the_bare_tap() {
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_Right), false),
            Some(RecoveryAction::StepRight)
        );
        assert_eq!(
            recovery_action(logo(), Keysym::new(keysyms::KEY_Left), false),
            Some(RecoveryAction::StepLeft)
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

        // The arrow arriving while Super is held cancels the bare-Super tap and ends the
        // momentary reveal the press started, so releasing Super afterwards does not also
        // toggle Home — the same mechanism `Super+D` already relies on.
        let mut tap = SuperTapTracker::default();
        tap.update(
            KeyState::Pressed,
            Keysym::new(keysyms::KEY_Super_L),
            0,
            false,
        );
        assert_eq!(
            tap.update(KeyState::Pressed, Keysym::new(keysyms::KEY_Right), 50, true),
            SuperTapDecision::ForwardEndingReveal
        );
        assert_eq!(
            tap.update(
                KeyState::Released,
                Keysym::new(keysyms::KEY_Super_L),
                100,
                false
            ),
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
