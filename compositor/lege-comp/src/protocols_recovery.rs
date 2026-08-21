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

use crate::protocols::lege_recovery_v1::lege_recovery_manager_v1::{self, LegeRecoveryManagerV1};
use crate::state::{Backend, LegeCompState, client_is_trusted_shell};
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, New};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

#[derive(Debug, Default)]
pub struct RecoveryProtocolState {
    pub pressure_overlay: Option<PressureOverlay>,
}

#[derive(Debug, Clone)]
pub struct PressureOverlay {
    pub offender_app_id: Option<String>,
    pub offender_instance_id: Option<String>,
    pub message: String,
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
        _resource: &LegeRecoveryManagerV1,
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
                });
            }
            lege_recovery_manager_v1::Request::HidePressureOverlay => {
                state.recovery_protocol.pressure_overlay = None;
            }
        }
    }
}
