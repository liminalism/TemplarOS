//! The private, second `wl_display` socket only the trusted shell connects to. Whoever connects
//! here — and only here — gets `ClientState { trusted_shell: true, .. }`
//! (`crate::state::client_is_trusted_shell`), which is the sole source of `ClientKind::TrustedShell`
//! and of `lege_shell_v1`'s global visibility (`src/protocols_shell.rs`). No request over any
//! protocol can grant trust; only accepting a connection on this socket can.
//!
//! `lege-shell` (`shell/lege-shell`) has no Wayland client code yet this milestone (it is a
//! `--print`-only CLI today — see its own module doc) — nothing spawns it from here. The socket is
//! still opened and its name logged, so a `lege-shell` build that grows a real Wayland surface can
//! be pointed at it (`WAYLAND_DISPLAY=<name>`) without any change on this side.

use crate::state::{Backend, ClientState, LegeCompState};
use smithay::wayland::socket::ListeningSocketSource;
use std::sync::Arc;

/// Open the trusted-shell socket (named `{main_socket_name}-shell`) and register its calloop
/// source. Returns the socket name for logging/env-var wiring.
pub fn install<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    main_socket_name: &str,
) -> anyhow::Result<String> {
    let requested_name = format!("{main_socket_name}-shell");
    let source = ListeningSocketSource::with_name(&requested_name).map_err(|error| {
        anyhow::anyhow!("opening trusted-shell socket {requested_name}: {error}")
    })?;
    let socket_name = source.socket_name().to_string_lossy().into_owned();

    state
        .loop_handle
        .clone()
        .insert_source(source, |client_stream, (), state: &mut LegeCompState<B>| {
            let client_state = ClientState {
                trusted_shell: true,
                ..ClientState::default()
            };
            if let Err(error) = state
                .display_handle
                .insert_client(client_stream, Arc::new(client_state))
            {
                tracing::warn!(%error, "failed to add trusted-shell client");
            }
        })
        .map_err(|error| anyhow::anyhow!("inserting trusted-shell socket source: {error}"))?;

    Ok(socket_name)
}
