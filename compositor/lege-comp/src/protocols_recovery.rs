//! Server-side `lege_recovery_v1`: the pressure overlay `lege-sessiond` (on `lege-resourced`'s
//! behalf) asks the compositor to show. Per `lege-recovery-v1.xml`'s doc, this is the *only*
//! client-requestable surface `lege_recovery_v1` exposes — the scatter view, the escape route and
//! the fallback switcher are compositor-keybinding-only (`src/keybindings.rs`) and never reach the
//! wire at all.
//!
//! There is no distinct "system" socket yet for `lege-sessiond` to connect as (only the trusted
//! shell has a private socket today, `src/shell_socket.rs`), so this global is filtered the same
//! way as `lege_shell_manager_v1` for now. That is a known simplification, not a design decision:
//! the pressure overlay and the shell's own commands are conceptually different trust tiers (one
//! is "the system", one is "the restartable shell process") that happen to share one socket
//! because only one exists. Splitting them is follow-up work, not a correctness gap in what is
//! implemented here.
//!
//! The overlay's four choices (`chrome::layout::PressureChoice`) are kept keyboard/pointer
//! navigable by `src/input.rs`, which calls [`RecoveryProtocolState::move_selection`]/`select`/
//! `confirm_selected` on this state directly — `chrome::mod`'s `collect_elements` only *draws*
//! `pressure_overlay.selected`, it never changes it.

use crate::chrome::layout::PressureChoice;
use crate::protocols::lege_recovery_v1::lege_recovery_manager_v1::{self, LegeRecoveryManagerV1};
use crate::state::{Backend, LegeCompState, client_is_trusted_shell};
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

#[derive(Debug, Default)]
pub struct RecoveryProtocolState {
    pub pressure_overlay: Option<PressureOverlay>,
    /// The `lege_recovery_manager_v1` resource that asked for the currently-shown overlay, kept
    /// so [`confirm_selected`](RecoveryProtocolState::confirm_selected) can send the
    /// `user_choice` event back on the same connection that asked (`lege-sessiond`'s) rather than
    /// broadcasting it — `interfaces/wayland/lege-recovery-v1.xml` has exactly one client of this
    /// interface, but this is what makes that an invariant rather than an assumption.
    resource: Option<LegeRecoveryManagerV1>,
}

#[derive(Debug, Clone)]
pub struct PressureOverlay {
    pub offender_app_id: Option<String>,
    pub offender_instance_id: Option<String>,
    pub message: String,
    /// Which of the four choices is currently highlighted for keyboard navigation
    /// (`src/input.rs`) — always starts at the least destructive
    /// (`PressureChoice::default`), never pre-selecting `Terminate`.
    pub selected: PressureChoice,
}

impl RecoveryProtocolState {
    /// Move the highlighted choice — Left/Right/Tab while the overlay is shown (`src/input.rs`).
    /// A no-op if no overlay is currently showing.
    pub fn move_selection(&mut self, forward: bool) {
        if let Some(overlay) = &mut self.pressure_overlay {
            overlay.selected = overlay.selected.step(forward);
        }
    }

    /// Select a specific choice directly — a pointer click on one of the drawn buttons
    /// (`src/input.rs`'s pointer handling, `chrome::layout::pressure_choice_at`).
    pub fn select(&mut self, choice: PressureChoice) {
        if let Some(overlay) = &mut self.pressure_overlay {
            overlay.selected = choice;
        }
    }

    /// Send the currently-highlighted choice back to whichever connection asked for this overlay
    /// — Enter or a button click confirms (`src/input.rs`). Does *not* hide the overlay itself:
    /// only an explicit `hide_pressure_overlay` request does that (this module's doc), matching
    /// the protocol — the requester decides what the overlay shows next (e.g. "terminating…"),
    /// not the compositor guessing on its behalf.
    pub fn confirm_selected(&mut self) {
        let (Some(overlay), Some(resource)) = (&self.pressure_overlay, &self.resource) else {
            return;
        };
        resource.user_choice(wire_choice(overlay.selected));
    }
}

/// `lege_recovery_manager_v1::Choice`'s wire values are fixed by
/// `interfaces/wayland/lege-recovery-v1.xml`'s `choice` enum; matched here against the
/// wayland-scanner-generated enum's own variants once rather than trusted to line up by
/// transmute (same pattern as `protocols_shell.rs`'s `wire_layer`).
fn wire_choice(choice: PressureChoice) -> lege_recovery_manager_v1::Choice {
    match choice {
        PressureChoice::Wait => lege_recovery_manager_v1::Choice::Wait,
        PressureChoice::CheckpointAndTerminate => {
            lege_recovery_manager_v1::Choice::CheckpointAndTerminate
        }
        PressureChoice::Terminate => lege_recovery_manager_v1::Choice::Terminate,
        PressureChoice::OpenTerminal => lege_recovery_manager_v1::Choice::OpenTerminal,
    }
}

pub struct RecoveryManagerGlobalData;

impl<B: Backend + 'static> GlobalDispatch2<LegeRecoveryManagerV1, LegeCompState<B>>
    for RecoveryManagerGlobalData
{
    fn bind(
        &self,
        _state: &mut LegeCompState<B>,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<LegeRecoveryManagerV1>,
        data_init: &mut DataInit<'_, LegeCompState<B>>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(&self, client: &Client) -> bool {
        client_is_trusted_shell(client)
    }
}

impl<B: Backend + 'static> Dispatch2<LegeRecoveryManagerV1, LegeCompState<B>> for () {
    fn request(
        &self,
        state: &mut LegeCompState<B>,
        client: &Client,
        resource: &LegeRecoveryManagerV1,
        request: lege_recovery_manager_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, LegeCompState<B>>,
    ) {
        if !client_is_trusted_shell(client) {
            return;
        }
        match request {
            lege_recovery_manager_v1::Request::ShowPressureOverlay {
                offender_app_id,
                offender_instance_id,
                message,
            } => {
                state.recovery_protocol.pressure_overlay = Some(PressureOverlay {
                    offender_app_id,
                    offender_instance_id,
                    message,
                    selected: PressureChoice::default(),
                });
                state.recovery_protocol.resource = Some(resource.clone());
            }
            lege_recovery_manager_v1::Request::HidePressureOverlay => {
                state.recovery_protocol.pressure_overlay = None;
                state.recovery_protocol.resource = None;
            }
        }
    }
}
