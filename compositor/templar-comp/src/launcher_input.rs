//! Keep physical keys ordered while the requested launcher maps and borrows focus.
//! This is a bounded seat queue, not text injection: modifiers and releases use the
//! same Smithay filter as ordinary input. An absent shell cannot strand a held key.

use crate::state::{Backend, TemplarCompState};
use smithay::{
    backend::input::KeyState,
    input::keyboard::{FilterResult, Keycode},
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{Client, backend::ClientId, protocol::wl_surface::WlSurface},
    },
    utils::Serial,
};
use std::time::Duration;

const CAPACITY: usize = 128;
const WAIT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub(crate) struct QueuedKey {
    pub code: Keycode,
    pub key_state: KeyState,
    pub serial: Serial,
    pub time: u32,
}

#[derive(Debug)]
struct Pending {
    owner: ClientId,
    previous: Option<WlSurface>,
    keys: Vec<QueuedKey>,
    timer: Option<RegistrationToken>,
}

#[derive(Debug, Default)]
pub struct LauncherInput {
    generation: u64,
    pending: Option<Pending>,
}

impl<B: Backend + 'static> TemplarCompState<B> {
    pub(crate) fn launcher_focus_waiting(&self) -> bool {
        self.launcher_input.pending.is_some()
    }
    pub(crate) fn begin_launcher_input(&mut self, client: &Client) {
        self.finish_launcher_input(true);
        if self.session_lock.locked || !self.backend_data.session_active() {
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        self.launcher_input.generation = self.launcher_input.generation.wrapping_add(1);
        let generation = self.launcher_input.generation;
        self.launcher_input.pending = Some(Pending {
            owner: client.id(),
            previous: keyboard.current_focus(),
            keys: Vec::with_capacity(CAPACITY),
            timer: None,
        });
        match self
            .loop_handle
            .insert_source(Timer::from_duration(WAIT), move |_, (), state| {
                if state.launcher_input.generation == generation {
                    state.finish_launcher_input(true);
                }
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                if let Some(pending) = &mut self.launcher_input.pending {
                    pending.timer = Some(token);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "launcher focus timer unavailable; retaining normal keyboard routing");
                self.finish_launcher_input(true);
            }
        }
    }

    pub(crate) fn buffer_launcher_key(&mut self, key: QueuedKey) -> bool {
        let Some(pending) = &mut self.launcher_input.pending else {
            return false;
        };
        if self.session_lock.locked
            || !self.backend_data.session_active()
            || self.recovery_protocol.pressure_overlay.is_some()
        {
            self.finish_launcher_input(false);
            return false;
        }
        if pending.keys.len() >= CAPACITY {
            // Overflow ends borrowing rather than dropping a release or growing without bound.
            self.finish_launcher_input(true);
            return false;
        }
        pending.keys.push(key);
        true
    }

    pub(crate) fn complete_launcher_input(&mut self, client: &Client) {
        if self
            .launcher_input
            .pending
            .as_ref()
            .is_none_or(|pending| pending.owner != client.id())
        {
            return;
        }
        let Some(pending) = self.launcher_input.pending.take() else {
            return;
        };
        if let Some(token) = pending.timer {
            self.loop_handle.remove(token);
        }
        // Focus is now on this owner's mapped popup, so replay in physical event order.
        for key in pending.keys {
            self.on_keyboard_key(key);
        }
    }

    pub(crate) fn cancel_launcher_input_for(&mut self, owner: &ClientId) {
        if self
            .launcher_input
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.owner == owner)
        {
            self.finish_launcher_input(true);
        }
    }

    /// Timeout/disconnect forwards only while the previous focus is still current.
    /// A lock, outside click or modal change discards delivery while balancing XKB.
    pub(crate) fn finish_launcher_input(&mut self, forward: bool) {
        let Some(pending) = self.launcher_input.pending.take() else {
            return;
        };
        if let Some(token) = pending.timer {
            self.loop_handle.remove(token);
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let deliver = forward
            && !self.session_lock.locked
            && self.backend_data.session_active()
            && self.recovery_protocol.pressure_overlay.is_none()
            && keyboard.current_focus() == pending.previous;
        for key in pending.keys {
            if deliver {
                self.on_keyboard_key(key);
            } else {
                let home = self.window_service.home_revealed();
                keyboard.input::<(), _>(
                    self,
                    key.code,
                    key.key_state,
                    key.serial,
                    key.time,
                    |state, modifiers, handle| {
                        // Even a swallowed release must update the physical key/modifier state.
                        let _ = state.super_tap.update(
                            key.key_state,
                            handle.modified_sym(),
                            key.time,
                            home,
                        );
                        state.chord_held = modifiers.alt || modifiers.logo;
                        FilterResult::Intercept(())
                    },
                );
            }
        }
    }
}
