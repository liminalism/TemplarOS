//! The private, second `wl_display` socket only the trusted shell connects to. Whoever connects
//! here — and only here — gets `ClientState { trusted_shell: true, .. }`
//! (`crate::state::client_is_trusted_shell`), which is the sole source of `ClientKind::TrustedShell`
//! and of `lege_shell_v1`'s global visibility (`src/protocols_shell.rs`). No request over any
//! protocol can grant trust; only accepting a connection on this socket can.
//!
//! Nothing spawns `lege-shell` from here: the socket is opened and its name logged, and
//! `lege-shell --surface` is pointed at it (`WAYLAND_DISPLAY=<name>`; `shell/lege-shell/src/wayland_bridge.rs`
//! is the client side). When that connection goes away — the shell crashed, or was killed on
//! purpose — `protocols_shell`'s manager `destroyed` hook ends any "go home" reveal it left
//! behind, so its death never changes what the user can reach.

use crate::state::{Backend, ClientState, LegeCompState};
use smithay::wayland::socket::ListeningSocketSource;
use std::sync::Arc;

/// Open the trusted-shell socket (named `legeos-shell-{main_socket_name}`) and register its
/// calloop source. Returns the socket name for logging/env-var wiring.
///
/// The name is a *prefix* of the public one, not a suffix, and that is load-bearing. The public
/// socket is `wayland-N` so that confined applications can reach it — snapd's wayland interface
/// allows `owner /run/user/[0-9]*/wayland-[0-9]* rw,` — and AppArmor's `*` does not stop at a
/// dash, so `wayland-0-shell` would fall inside that same permission. Every snap on the machine
/// would be allowed to open the one socket whose sole purpose is to be unreachable by anything
/// but the shell. Putting the distinguishing part first keeps the name outside the glob while
/// still deriving it from the public socket, so a nested run on `wayland-3` gets its own.
pub fn install<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    main_socket_name: &str,
) -> anyhow::Result<String> {
    let requested_name = format!("legeos-shell-{main_socket_name}");
    let source = ListeningSocketSource::with_name(&requested_name).map_err(|error| {
        anyhow::anyhow!("opening trusted-shell socket {requested_name}: {error}")
    })?;
    let socket_name = source.socket_name().to_string_lossy().into_owned();

    state
        .loop_handle
        .clone()
        .insert_source(source, |client_stream, (), state: &mut LegeCompState<B>| {
            tracing::info!("accepted trusted-shell Wayland connection");
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
