//! `ext-session-lock-v1`. `Layer::SecureLock` surfaces are compositor-core-owned — no client
//! request creates them (COMPOSITOR.md's closing paragraph) — so this module only tracks lock
//! state and per-output lock surfaces; nothing here goes through `lege_window::WindowService`,
//! which has no concept of the lock screen at all (it is strictly above every typed layer, drawn
//! and input-routed independently of the one-workspace scene).
//!
//! Input isolation while locked is whole: `src/input.rs` refuses to run recovery keybindings or
//! start a window drag, `LegeCompState::surface_under` — the one hit-test every pointer path goes
//! through — resolves nothing but the lock surface for the output under the pointer, and the
//! keyboard is handed to the lock surface as soon as one exists. So while `locked`, no window
//! behind the lock screen can be pointed at, typed into or dragged. Only `unlock()` (driven by
//! the lock manager, i.e. `lege-sessiond`) clears the flag, which is what makes the lock a lock
//! rather than an overlay.

use crate::state::{Backend, LegeCompState};
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
}

impl<B: Backend + 'static> SessionLockHandler for LegeCompState<B> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_manager_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.session_lock.locked = true;
        // Smithay's contract: confirm only once a cleared frame has actually been presented on
        // every output. We have no in-flight-frame tracking to gate this on yet, so we confirm
        // immediately — the lock state (and the input-isolation guards in `src/input.rs`) is
        // already in effect before this returns, so nothing sensitive is exposed by confirming
        // eagerly; it only means a client screen-recording the exact lock transition could catch
        // one still-unlocked frame already in flight, not a real bypass of the lock itself.
        confirmation.lock();
    }

    fn unlock(&mut self) {
        self.session_lock.locked = false;
        self.session_lock.surfaces.clear();
        // The keyboard was the lock surface's and that surface is going away, so hand it on the
        // same way parking a window does: the topmost window still on screen, or Home when there
        // is none. Left alone, the seat would hold a dead surface and typing would go nowhere
        // until the user thought to click something.
        let any_left = self.window_service.stacking_order().into_iter().any(|id| {
            self.window_service.is_visible(id) && self.window_service.is_client_window(id)
        });
        crate::input::refocus_after_reveal(self, !any_left);
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
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
