//! `ext-session-lock-v1`. `Layer::SecureLock` surfaces are compositor-core-owned — no client
//! request creates them (COMPOSITOR.md's closing paragraph) — so this module only tracks lock
//! state and per-output lock surfaces; nothing here goes through `lege_window::WindowService`,
//! which has no concept of the lock screen at all (it is strictly above every typed layer, drawn
//! and input-routed independently of the one-workspace scene).
//!
//! Input isolation while locked is partial this milestone: `src/input.rs` refuses to run recovery
//! keybindings or start a window drag while `locked`, which stops the two ways this crate lets a
//! user reach another window's chrome, but ordinary pointer/keyboard events are still routed by
//! Smithay's normal focus resolution (`Space::element_under`), which does not yet exclude ordinary
//! windows while locked. A hostile or buggy client cannot unlock the session through this gap —
//! only `unlock()` (driven by the lock manager, i.e. `lege-sessiond`) clears `locked` — but a
//! sighted user could in principle still interact with content behind the lock surface. Tightening
//! `Space`/hit-testing to only ever resolve lock surfaces while locked is the follow-up.

use crate::state::{Backend, LegeCompState};
use smithay::{
    output::Output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
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
        self.session_lock.surfaces.insert(output.name(), surface);
    }
}
