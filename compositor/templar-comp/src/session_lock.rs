//! `ext-session-lock-v1`. `Layer::SecureLock` surfaces are compositor-core-owned — no client
//! request creates them (COMPOSITOR.md's closing paragraph) — so this module only tracks lock
//! state and per-output lock surfaces; nothing here goes through `templar_window::WindowService`,
//! which has no concept of the lock screen at all (it is strictly above every typed layer, drawn
//! and input-routed independently of the one-workspace scene).
//!
//! Input isolation while locked is whole: `src/input.rs` refuses to run recovery keybindings or
//! start a window drag, `TemplarCompState::surface_under` — the one hit-test every pointer path goes
//! through — resolves nothing but the lock surface for the output under the pointer, and the
//! keyboard is handed to the lock surface as soon as one exists. So while `locked`, no window
//! behind the lock screen can be pointed at, typed into or dragged. Only `unlock()` (driven by
//! the lock manager, i.e. `templar-sessiond`) clears the flag, which is what makes the lock a lock
//! rather than an overlay.

use crate::state::{Backend, TemplarCompState};
use smithay::{
    output::Output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    utils::SERIAL_COUNTER,
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct SessionLockState {
    pub locked: bool,
    /// Keyed by output name; recreated on every `lock()`/`unlock()` cycle.
    pub surfaces: HashMap<String, LockSurface>,
    generation: u64,
    confirmation: Option<SessionLocker>,
    coverage: Vec<(Output, bool)>,
    unknown_physical_outputs: bool,
}

impl SessionLockState {
    /// Attached to the exact rendered buffer, never inferred from the lock state at vblank.
    pub fn frame_generation(&self) -> Option<u64> {
        self.locked.then_some(self.generation)
    }

    /// Current-generation physical-output coverage, not a historical Locked acknowledgement.
    /// Inaccessible last-owned outputs remain required and cannot be attested by another output.
    pub fn protected_covered(&self) -> bool {
        self.locked && !self.unknown_physical_outputs && self.coverage.iter().all(|(_, presented)| *presented)
    }

    pub fn requires_protected_frame(&self, output: &Output) -> bool {
        self.locked && self.coverage.iter().any(|(old, presented)| old == output && !presented)
    }

    pub fn reset_coverage(&mut self) {
        if self.locked {
            self.generation = self.generation.checked_add(1).expect("lock generation exhausted");
            for (_, presented) in &mut self.coverage {
                *presented = false;
            }
        }
    }

    pub fn sync_physical_state(&mut self, unknown_physical_outputs: bool) -> bool {
        if self.unknown_physical_outputs == unknown_physical_outputs { return false; }
        self.unknown_physical_outputs = unknown_physical_outputs;
        self.reset_coverage();
        if self.locked { self.confirm_if_covered(); }
        true
    }

    pub fn outputs_changed<'a>(&mut self, outputs: impl Iterator<Item = &'a Output>, unknown_physical_outputs: bool) {
        self.sync_physical_state(unknown_physical_outputs);
        if !self.locked {
            return;
        }
        let previous = std::mem::take(&mut self.coverage);
        self.coverage = outputs
            .map(|output| {
                if let Some(surface) = self.surfaces.get(&output.name())
                    && let Some(visible) = crate::viewport::visible_physical_size(output)
                {
                    let logical: smithay::utils::Size<i32, smithay::utils::Logical> = visible.to_f64().to_logical(output.current_scale().fractional_scale()).to_i32_round();
                    surface.with_pending_state(|state| state.size = Some((logical.w.max(1) as u32, logical.h.max(1) as u32).into()));
                    surface.send_configure();
                }
                let presented = previous
                    .iter()
                    .any(|(old, presented)| old == output && *presented);
                (output.clone(), presented)
            })
            .collect();
        self.confirm_if_covered();
    }

    pub fn frame_presented(&mut self, output: &Output, generation: Option<u64>) {
        if generation != self.frame_generation() || !self.locked {
            return;
        }
        if let Some((_, presented)) = self.coverage.iter_mut().find(|(old, _)| old == output) {
            *presented = true;
        }
        self.confirm_if_covered();
    }

    fn confirm_if_covered(&mut self) {
        if !self.unknown_physical_outputs && self.coverage.iter().all(|(_, presented)| *presented)
            && let Some(confirmation) = self.confirmation.take()
        {
            confirmation.lock();
        }
    }
}

impl<B: Backend + 'static> SessionLockHandler for TemplarCompState<B> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_manager_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        if self.session_lock.locked {
            // A second client cannot replace the owner or release a failed-closed lock.
            return;
        }
        self.session_lock.locked = true;
        self.name_tab_menu = crate::chrome::NameTabMenu::default();
        self.session_lock.generation = self.session_lock.generation.checked_add(1).expect("lock generation exhausted");
        self.session_lock.confirmation = Some(confirmation);
        self.session_lock.coverage.clear();
        self.close_rail("session locked");
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.unset_grab(self);
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), 0);
            pointer.motion(self, None, &smithay::input::pointer::MotionEvent {
                location: pointer.current_location(),
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            });
        }
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
            touch.unset_grab(self);
        }
        self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        self.window_service.clear_tab_interaction();
        self.pending_tab_control = None;
        self.swallowing_press = false;
        self.dwell = None;
        self.screencopy.retire_all_previews();
        let generation = self.screencopy.demand.as_ref().map_or(0, |demand| demand.generation());
        self.screencopy.set_shell_demand(generation, Vec::new());
        self.session_lock.outputs_changed(self.space.outputs(), self.backend_data.unknown_physical_outputs());
        self.wake_for_protected_frame();
        crate::compositor_control::publish_activity(self);
        // A pre-lock frame may still be in flight. Only its successor can cover an output.
        B::request_redraw(self);
    }

    fn unlock(&mut self) {
        self.session_lock.locked = false;
        self.session_lock.surfaces.clear();
        self.session_lock.confirmation = None;
        self.session_lock.coverage.clear();
        self.session_lock.unknown_physical_outputs = false;
        // The keyboard was the lock surface's and that surface is going away, so hand it on the
        // same way parking a window does: the topmost window still on screen, or Home when there
        // is none. Left alone, the seat would hold a dead surface and typing would go nowhere
        // until the user thought to click something.
        let any_left = self.window_service.stacking_order().into_iter().any(|id| {
            self.window_service.is_visible(id) && self.window_service.is_client_window(id)
        });
        crate::input::refocus_after_reveal(self, !any_left);
        crate::compositor_control::publish_activity(self);
        B::request_redraw(self);
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        let Some(geometry) = crate::viewport::output_geometry(&self.space, &output) else {
            return;
        };
        surface.with_pending_state(|state| {
            state.size =
                Some((geometry.size.w.max(0) as u32, geometry.size.h.max(0) as u32).into());
        });
        surface.send_configure();
        // The lock screen takes the keyboard the moment it exists. Without this the password
        // field never receives a key, and whatever held focus when the session locked keeps
        // receiving them — behind the lock surface, where nobody can see what is being typed.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let focus = surface.wl_surface().clone();
            keyboard.set_focus(self, Some(focus), SERIAL_COUNTER.next_serial());
        }
        self.session_lock.surfaces.insert(output.name(), surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_initial_physical_image_prevents_vacuous_lock_coverage() {
        let mut lock = SessionLockState { locked: true, ..Default::default() };
        lock.outputs_changed(std::iter::empty(), true);
        assert!(!lock.protected_covered());
        let unknown_generation = lock.frame_generation();
        lock.outputs_changed(std::iter::empty(), false);
        assert!(lock.protected_covered());
        assert_ne!(lock.frame_generation(), unknown_generation);
    }
}
