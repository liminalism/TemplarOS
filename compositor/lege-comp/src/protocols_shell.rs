//! Server-side `lege_shell_v1`: the private, authenticated protocol between the compositor core
//! and the one connection accepted as the trusted shell (`src/shell_socket.rs`). The global itself
//! is filtered to that connection (`ShellManagerGlobalData::can_view`), and every request handler
//! below checks `client_is_trusted_shell` again — belt and suspenders, since this is the one
//! protocol boundary in the crate a bug in the filter would actually matter for.
//!
//! [`sync_toplevels`]/[`sync_foreign_toplevels`] (called from `src/space_sync.rs` after every
//! `WindowService`-mutating handler) are the only place these objects' state changes: nothing here
//! sends events synchronously from inside a request handler that touched `WindowService`, matching
//! COMPOSITOR.md's "never encode/send `lege_shell_v1` events synchronously from the same call site".

use crate::protocols::lege_shell_v1::{
    lege_shell_manager_v1::{self, LegeShellManagerV1},
    lege_shell_toplevel_list_v1::{self, LegeShellToplevelListV1},
    lege_shell_toplevel_v1::{self, LegeShellToplevelV1},
};
use crate::state::{Backend, LegeCompState, client_is_trusted_shell};
use crate::window_bridge::now_monotonic_ns;
use lege_comp_integration::ShellStateSnapshot;
use lege_window::{
    ActivationGrant, CommandContext, Layer, RevealHome, WindowCommand, WindowId,
    geometry::{Edge, Exclusive},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource, WEnum,
    backend::{ClientId, ObjectId},
    protocol::wl_surface::WlSurface,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use std::collections::{HashMap, HashSet};

/// The protocol version this compositor implements (`interfaces/wayland/lege-shell-v1.xml`).
/// Both peers ship from this repository, so a version bump is a pair of edits, not a
/// compatibility matrix — but every `since="2"` event below is still guarded on the bound
/// object's version rather than assumed.
pub const SHELL_PROTOCOL_VERSION: u32 = 2;

/// Bookkeeping for currently-bound `lege_shell_v1` objects.
#[derive(Debug, Default)]
pub struct ShellProtocolState {
    /// Every bound manager: where `home_revealed` is reported, and whose destruction — the shell
    /// disconnecting — ends a reveal so a dead shell never leaves the user facing a blank backdrop.
    pub managers: Vec<LegeShellManagerV1>,
    pub toplevel_lists: Vec<LegeShellToplevelListV1>,
    pub toplevel_objects: HashMap<WindowId, LegeShellToplevelV1>,
    /// Toplevel objects that have had their `identifier` event, which is sent exactly once.
    pub identified: HashSet<WindowId>,
    /// One entry per surface with a live `set_layer_surface` claim.
    pub layer_surfaces: HashMap<ObjectId, (WlSurface, lege_window::Layer)>,
    /// One entry per surface with a live (non-zero) `set_exclusive_zone` claim: which output it
    /// was on, and the claim itself. Released automatically when the surface is destroyed
    /// (`lege-shell-v1.xml`'s `set_exclusive_zone` doc) — enforced in `on_shell_surface_destroyed`.
    pub exclusive_zones: HashMap<ObjectId, (String, Exclusive)>,
}

/// Global-visibility filter: only the trusted-shell connection ever sees `lege_shell_manager_v1`
/// exists at all, so an ordinary application cannot even attempt to bind it.
pub struct ShellManagerGlobalData;

impl<B: Backend + 'static> GlobalDispatch2<LegeShellManagerV1, LegeCompState<B>>
    for ShellManagerGlobalData
{
    fn bind(
        &self,
        state: &mut LegeCompState<B>,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<LegeShellManagerV1>,
        data_init: &mut DataInit<'_, LegeCompState<B>>,
    ) {
        let manager = data_init.init(resource, ());
        if manager.version() >= 2 {
            manager.home_revealed(u32::from(state.window_service.home_revealed()));
        }
        state.shell_protocol.managers.push(manager);
    }

    fn can_view(&self, client: &Client) -> bool {
        client_is_trusted_shell(client)
    }
}

impl<B: Backend + 'static> Dispatch2<LegeShellManagerV1, LegeCompState<B>> for () {
    fn request(
        &self,
        state: &mut LegeCompState<B>,
        client: &Client,
        _resource: &LegeShellManagerV1,
        request: lege_shell_manager_v1::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, LegeCompState<B>>,
    ) {
        if !client_is_trusted_shell(client) {
            return;
        }
        let ctx = trusted_ctx(state);
        match request {
            lege_shell_manager_v1::Request::GetToplevelList { id } => {
                let list = data_init.init(id, ());
                for record in state.window_service.shell_toplevels() {
                    send_new_toplevel(state, &list, record.window_id);
                }
                state.shell_protocol.toplevel_lists.push(list);
            }
            lege_shell_manager_v1::Request::ShowAllWindows => {
                state.run_recovery_action(crate::keybindings::RecoveryAction::ShowAllWindows);
            }
            lege_shell_manager_v1::Request::EscapeFullscreenOrModal { toplevel } => {
                let window_id =
                    toplevel.and_then(|resource| window_id_for_toplevel(state, &resource));
                if state
                    .window_service
                    .handle_command(WindowCommand::EscapeFullscreenOrModal { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            lege_shell_manager_v1::Request::SetLayerSurface { surface, layer } => {
                if let WEnum::Value(layer) = layer {
                    let layer = lege_comp_integration::protocol::layer_from_wire(layer as u32);
                    if let Some(layer) = layer
                        && !layer.is_core_only()
                    {
                        smithay::wayland::compositor::add_destruction_hook::<LegeCompState<B>, _>(
                            &surface,
                            |state, surface| on_shell_surface_destroyed(state, &surface.id()),
                        );
                        state
                            .shell_protocol
                            .layer_surfaces
                            .insert(surface.id(), (surface, layer));
                    }
                }
            }
            lege_shell_manager_v1::Request::SetExclusiveZone {
                surface,
                edge,
                size,
            } => {
                handle_set_exclusive_zone(state, &surface, edge, size);
            }
            lege_shell_manager_v1::Request::RevealHome { reveal } => {
                if let WEnum::Value(reveal) = reveal {
                    let reveal = match reveal {
                        lege_shell_manager_v1::Reveal::Show => RevealHome::Show,
                        lege_shell_manager_v1::Reveal::Hide => RevealHome::Hide,
                        lege_shell_manager_v1::Reveal::Toggle => RevealHome::Toggle,
                    };
                    state.reveal_home(reveal);
                }
            }
        }
    }

    /// The manager going away is the shell going away (it is bound once, for the process's
    /// lifetime). A reveal it may have left behind ends here, so the windows come back without
    /// anyone having to press anything — the shell is a restartable process, and its death is
    /// not allowed to change what the user can reach.
    fn destroyed(
        &self,
        state: &mut LegeCompState<B>,
        _client: ClientId,
        resource: &LegeShellManagerV1,
    ) {
        state
            .shell_protocol
            .managers
            .retain(|manager| manager != resource);
        if state.shell_protocol.managers.is_empty() && state.window_service.home_revealed() {
            state.reveal_home(RevealHome::Hide);
        }
    }
}

fn trusted_ctx<B: Backend + 'static>(state: &LegeCompState<B>) -> CommandContext {
    CommandContext {
        trusted_shell: true,
        now_monotonic_ns: now_monotonic_ns(&state.clock),
    }
}

fn window_id_for_toplevel<B: Backend + 'static>(
    state: &LegeCompState<B>,
    resource: &LegeShellToplevelV1,
) -> Option<WindowId> {
    state
        .shell_protocol
        .toplevel_objects
        .iter()
        .find(|(_, object)| *object == resource)
        .map(|(id, _)| *id)
}

fn handle_set_exclusive_zone<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    surface: &WlSurface,
    edge: WEnum<lege_shell_manager_v1::Edge>,
    size: u32,
) {
    let WEnum::Value(edge) = edge else {
        return;
    };
    let edge = match edge {
        lege_shell_manager_v1::Edge::Top => Edge::Top,
        lege_shell_manager_v1::Edge::Bottom => Edge::Bottom,
        lege_shell_manager_v1::Edge::Left => Edge::Left,
        lege_shell_manager_v1::Edge::Right => Edge::Right,
    };
    let Some(output_name) = state
        .space
        .outputs()
        .next()
        .map(smithay::output::Output::name)
    else {
        return;
    };
    let object_id = surface.id();
    if size == 0 {
        state.shell_protocol.exclusive_zones.remove(&object_id);
    } else {
        // Idempotent alongside `SetLayerSurface`'s identical registration: a surface that only
        // ever calls `set_exclusive_zone` (never `set_layer_surface`) still needs its claim
        // released on destruction.
        smithay::wayland::compositor::add_destruction_hook::<LegeCompState<B>, _>(
            surface,
            |state, surface| on_shell_surface_destroyed(state, &surface.id()),
        );
        state.shell_protocol.exclusive_zones.insert(
            object_id,
            (
                output_name,
                Exclusive {
                    edge,
                    size: size.min(i32::MAX as u32) as i32,
                },
            ),
        );
    }
    crate::backend::recompute_outputs(state);
}

/// A shell surface with a live layer/exclusive-zone claim was destroyed: release both — the
/// `lege_shell_v1` doc's "reservations are per surface" contract.
pub fn on_shell_surface_destroyed<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    surface: &ObjectId,
) {
    state.shell_protocol.layer_surfaces.remove(surface);
    if state
        .shell_protocol
        .exclusive_zones
        .remove(surface)
        .is_some()
    {
        crate::backend::recompute_outputs(state);
    }
}

impl<B: Backend + 'static> Dispatch2<LegeShellToplevelListV1, LegeCompState<B>> for () {
    fn request(
        &self,
        _state: &mut LegeCompState<B>,
        _client: &Client,
        _resource: &LegeShellToplevelListV1,
        _request: lege_shell_toplevel_list_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, LegeCompState<B>>,
    ) {
    }

    fn destroyed(
        &self,
        state: &mut LegeCompState<B>,
        _client: ClientId,
        resource: &LegeShellToplevelListV1,
    ) {
        state
            .shell_protocol
            .toplevel_lists
            .retain(|list| list != resource);
    }
}

impl<B: Backend + 'static> Dispatch2<LegeShellToplevelV1, LegeCompState<B>> for () {
    fn request(
        &self,
        state: &mut LegeCompState<B>,
        client: &Client,
        resource: &LegeShellToplevelV1,
        request: lege_shell_toplevel_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, LegeCompState<B>>,
    ) {
        if !client_is_trusted_shell(client) {
            return;
        }
        let Some(window_id) = window_id_for_toplevel(state, resource) else {
            return;
        };
        let ctx = trusted_ctx(state);
        match request {
            lege_shell_toplevel_v1::Request::Activate { activation_token } => {
                let grant = ActivationGrant {
                    token: activation_token,
                    requesting_instance: None,
                    target_instance: None,
                    expires_at_monotonic_ns: ctx.now_monotonic_ns.saturating_add(5_000_000_000),
                    had_recent_input_evidence: true,
                };
                if state
                    .window_service
                    .handle_command(WindowCommand::Activate { window_id, grant }, ctx)
                    .is_ok()
                {
                    crate::input::activate_window(state, window_id);
                    crate::space_sync::sync_after_change(state);
                }
            }
            lege_shell_toplevel_v1::Request::Close => {
                if let Ok(lege_window::CommandEffect::Closed { .. }) = state
                    .window_service
                    .handle_command(WindowCommand::Close { window_id }, ctx)
                {
                    crate::backend::close_window(state, window_id);
                }
            }
            lege_shell_toplevel_v1::Request::MoveToVisibleArea => {
                if state
                    .window_service
                    .handle_command(WindowCommand::MoveToVisibleArea { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            lege_shell_toplevel_v1::Request::Shelve => {
                if state
                    .window_service
                    .handle_command(WindowCommand::Shelve { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            lege_shell_toplevel_v1::Request::Unshelve => {
                if state
                    .window_service
                    .handle_command(WindowCommand::Unshelve { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            lege_shell_toplevel_v1::Request::SetMaximized { maximized } => {
                crate::xdg_shell::apply_maximized(state, window_id, maximized != 0);
            }
        }
    }

    fn destroyed(
        &self,
        state: &mut LegeCompState<B>,
        _client: ClientId,
        resource: &LegeShellToplevelV1,
    ) {
        state
            .shell_protocol
            .toplevel_objects
            .retain(|_, object| object != resource);
    }
}

/// Everything a version-2 `lege_shell_toplevel_v1` carries beyond version 1: the foreign-toplevel
/// identifier (once), the geometry and the maximized flag. A no-op on a version-1 object.
fn send_v2_toplevel_events<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    object: &LegeShellToplevelV1,
    toplevel: &lege_window::ShellToplevel,
) {
    if object.version() < 2 {
        return;
    }
    if !state
        .shell_protocol
        .identified
        .contains(&toplevel.window_id)
        && let Some(identifier) = state.foreign_toplevels.identifier_for(toplevel.window_id)
    {
        object.identifier(identifier.to_owned());
        state.shell_protocol.identified.insert(toplevel.window_id);
    }
    let geometry = toplevel.geometry;
    object.geometry(geometry.x, geometry.y, geometry.width, geometry.height);
    object.maximized(u32::from(toplevel.maximized));
}

fn send_new_toplevel<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    list: &LegeShellToplevelListV1,
    window_id: WindowId,
) {
    let Some(client) = list.client() else { return };
    let Ok(toplevel) = client.create_resource::<LegeShellToplevelV1, (), LegeCompState<B>>(
        &state.display_handle,
        list.version(),
        (),
    ) else {
        return;
    };
    list.toplevel(&toplevel);
    state
        .shell_protocol
        .toplevel_objects
        .insert(window_id, toplevel);
}

/// Push every `lege_shell_toplevel_v1` up to date with `snapshot`: create objects for
/// newly-mapped windows, refresh identity/title/state/layer on every one, and close+drop objects
/// for windows no longer present. Always sends a full state, not a diff — simple and correct at
/// the cost of some redundant wire traffic (`src/space_sync.rs`'s module doc).
pub fn sync_toplevels<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    snapshot: &ShellStateSnapshot,
) {
    let live: std::collections::HashSet<WindowId> =
        snapshot.toplevels.iter().map(|t| t.window_id).collect();

    for list in state.shell_protocol.toplevel_lists.clone() {
        for toplevel in &snapshot.toplevels {
            if !state
                .shell_protocol
                .toplevel_objects
                .contains_key(&toplevel.window_id)
            {
                send_new_toplevel(state, &list, toplevel.window_id);
            }
        }
    }

    for toplevel in &snapshot.toplevels {
        let Some(object) = state
            .shell_protocol
            .toplevel_objects
            .get(&toplevel.window_id)
            .cloned()
        else {
            continue;
        };
        object.identity(
            toplevel.canonical_app_id.as_ref().map(ToString::to_string),
            toplevel.instance_id.map(|id| id.to_string()),
            u32::from(toplevel.verified_identity),
        );
        object.title(toplevel.title.clone());
        let shelf_rank = snapshot
            .shelf
            .iter()
            .position(|shelved| shelved.window_id == toplevel.window_id)
            .map_or(0, |position| (snapshot.shelf.len() - position) as u32);
        object.state(
            u32::from(toplevel.activated),
            u32::from(toplevel.fullscreen),
            u32::from(toplevel.shelved),
            shelf_rank,
        );
        object.layer(wire_layer(toplevel.layer));
        send_v2_toplevel_events(state, &object, toplevel);
    }

    state
        .shell_protocol
        .toplevel_objects
        .retain(|window_id, object| {
            if live.contains(window_id) {
                true
            } else {
                object.closed();
                false
            }
        });
    state
        .shell_protocol
        .identified
        .retain(|window_id| live.contains(window_id));

    for manager in &state.shell_protocol.managers {
        if manager.version() >= 2 {
            manager.home_revealed(u32::from(snapshot.home_revealed));
        }
    }
}

/// `lege_window::Layer`'s wire mapping is fixed by `lege-shell-v1.xml`'s `layer` enum
/// (`interfaces/wayland/README.md`); `lege_comp_integration::protocol::layer_to_wire` gives the
/// numeric discriminant, matched here against the wayland-scanner-generated enum's own variants
/// once rather than trusted to line up by transmute.
fn wire_layer(layer: Layer) -> lege_shell_manager_v1::Layer {
    match layer {
        Layer::SecureLock => lege_shell_manager_v1::Layer::SecureLock,
        Layer::Recovery => lege_shell_manager_v1::Layer::Recovery,
        Layer::SystemModal => lege_shell_manager_v1::Layer::SystemModal,
        Layer::ShellOverlay => lege_shell_manager_v1::Layer::ShellOverlay,
        Layer::Notification => lege_shell_manager_v1::Layer::Notification,
        Layer::AppOnTop => lege_shell_manager_v1::Layer::AppOnTop,
        Layer::Normal => lege_shell_manager_v1::Layer::Normal,
        Layer::HomeSurface => lege_shell_manager_v1::Layer::HomeSurface,
    }
}
