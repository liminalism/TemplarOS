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
    ActivationGrant, ClientKind, CommandContext, Layer, Rect, RevealHome, WindowCommand, WindowId,
    geometry::{Edge, Exclusive, shell_surface_placement},
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
pub const SHELL_PROTOCOL_VERSION: u32 = 4;

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
                            .insert(surface.id(), (surface.clone(), layer));
                        apply_surface_claims_if_mapped(state, &surface);
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
            lege_shell_manager_v1::Request::ArrangeWindows { preset } => {
                if let WEnum::Value(preset) = preset {
                    crate::xdg_shell::apply_arrangement(
                        state,
                        WindowCommand::ArrangeWindows {
                            preset: preset_from_wire(preset),
                        },
                    );
                }
            }
            lege_shell_manager_v1::Request::RestoreArrangement => {
                crate::xdg_shell::apply_arrangement(state, WindowCommand::RestoreArrangement);
            }
            lege_shell_manager_v1::Request::SetIdleTimeout { seconds } => {
                // Policy in, enforcement here: the number is `Power1`'s `screen_off_after`, which
                // the shell read and relayed, and this process is the only one that can see the
                // seat go quiet (`src/idle.rs`).
                state.set_idle_timeout(seconds);
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

/// The wire enum's values match `lege_window::ArrangementPreset`'s declaration order
/// (`interfaces/wayland/lege-shell-v1.xml`'s `arrangement_preset` doc); a test holds them in
/// agreement.
pub(crate) fn preset_from_wire(
    preset: lege_shell_manager_v1::ArrangementPreset,
) -> lege_window::ArrangementPreset {
    match preset {
        lege_shell_manager_v1::ArrangementPreset::Focus => lege_window::ArrangementPreset::Focus,
        lege_shell_manager_v1::ArrangementPreset::Halves => lege_window::ArrangementPreset::Halves,
        lege_shell_manager_v1::ArrangementPreset::Thirds => lege_window::ArrangementPreset::Thirds,
        lege_shell_manager_v1::ArrangementPreset::Quarters => {
            lege_window::ArrangementPreset::Quarters
        }
        lege_shell_manager_v1::ArrangementPreset::MainAndStack => {
            lege_window::ArrangementPreset::MainAndStack
        }
        lege_shell_manager_v1::ArrangementPreset::Cushion => {
            lege_window::ArrangementPreset::Cushion
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
    apply_surface_claims_if_mapped(state, surface);
}

/// Apply the trusted shell's claims once the corresponding xdg-toplevel exists in
/// `WindowService`. The protocol requests normally arrive before the first buffer/map commit, so
/// storing them is not enough: mapping must consume them and configure the client to the claimed
/// output geometry. This also handles a claim changed after mapping.
pub fn apply_surface_claims<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    surface: &WlSurface,
    window_id: WindowId,
    requested: Rect,
) {
    let object_id = surface.id();
    let claimed_layer = state
        .shell_protocol
        .layer_surfaces
        .get(&object_id)
        .map(|(_, layer)| *layer);
    let exclusive_claim = state
        .shell_protocol
        .exclusive_zones
        .get(&object_id)
        .cloned();
    if claimed_layer.is_none() && exclusive_claim.is_none() {
        return;
    }

    let layer = claimed_layer
        .or_else(|| state.window_service.layer_of(window_id))
        .unwrap_or(Layer::Normal);
    if let Err(error) = state
        .window_service
        .set_layer(window_id, layer, ClientKind::TrustedShell)
    {
        tracing::warn!(%error, ?window_id, ?layer, "rejecting trusted-shell layer claim");
        return;
    }

    let preferred_output = exclusive_claim.as_ref().map(|(name, _)| name.as_str());
    let output = state
        .window_service
        .outputs()
        .iter()
        .find(|output| preferred_output.is_some_and(|name| output.name == name))
        .or_else(|| state.window_service.outputs().first())
        .cloned();
    let Some(output) = output else {
        tracing::warn!(
            ?window_id,
            ?layer,
            "shell claim has no output to place it on"
        );
        return;
    };
    let exclusive = exclusive_claim.map(|(_, claim)| claim);
    let geometry = shell_surface_placement(layer, exclusive, &output, requested);
    state.window_service.set_geometry(window_id, geometry);

    if let Some(window) = state.windows.element_of(window_id)
        && let Some(toplevel) = window.toplevel()
    {
        let client = crate::space_sync::client_area(state, window_id);
        toplevel.with_pending_state(|pending| {
            pending.size = Some((client.width, client.height).into());
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
    tracing::info!(
        ?window_id,
        ?layer,
        ?geometry,
        output = %output.name,
        "trusted-shell surface claim applied"
    );
    crate::space_sync::sync_after_change(state);
    if layer == Layer::HomeSurface {
        crate::input::focus_home_if_needed(state, surface);
    }
}

fn apply_surface_claims_if_mapped<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    surface: &WlSurface,
) {
    let Some(window_id) = state.windows.window_id_for(&surface.id()) else {
        return;
    };
    let Some(requested) = state
        .window_service
        .window(window_id)
        .map(|record| record.geometry)
    else {
        return;
    };
    apply_surface_claims(state, surface, window_id, requested);
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

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::WindowClaims;
    use lege_window::{ClientKind, Output, WindowService};

    struct NullCache;
    impl lege_window::IdentityCache for NullCache {
        fn resolve_cached(
            &self,
            _pid: u32,
            _claims: &WindowClaims,
        ) -> Option<lege_window::CachedIdentity> {
            None
        }
        fn request_resolution(&self, _window_id: WindowId, _pid: u32, _claims: WindowClaims) {}
    }

    fn service_with_windows(count: u32) -> (WindowService<NullCache>, Vec<WindowId>) {
        let outputs = vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)];
        let mut service = WindowService::new(NullCache, outputs);
        let ids = (0..count)
            .map(|index| {
                service.on_toplevel_mapped(
                    index + 1,
                    ClientKind::Application,
                    WindowClaims::default(),
                    format!("window {index}"),
                    Rect::new(100 + 50 * i32::try_from(index).unwrap_or(0), 100, 400, 300),
                )
            })
            .collect();
        (service, ids)
    }

    fn trusted(service: &WindowService<NullCache>) -> CommandContext {
        let _ = service;
        CommandContext {
            trusted_shell: true,
            now_monotonic_ns: 0,
        }
    }

    /// The protocol enum's wire values follow `lege_window::ArrangementPreset`'s declaration
    /// order, as `interfaces/wayland/lege-shell-v1.xml` promises.
    #[test]
    fn arrangement_wire_values_match_the_window_service_presets() {
        for (index, preset) in lege_window::ArrangementPreset::ALL.into_iter().enumerate() {
            let wire =
                lege_shell_manager_v1::ArrangementPreset::try_from(u32::try_from(index).unwrap())
                    .expect("every preset has a wire value");
            assert_eq!(preset_from_wire(wire), preset);
        }
    }

    /// `dynamic-window-set` as the compositor drives it: a closed window is ignored, a newly
    /// mapped one joins only on the next explicit request, and losing the output repairs the
    /// restore instead of losing a window.
    #[test]
    fn arrangement_requests_ignore_closed_windows_and_join_new_ones_on_the_next_press() {
        let (mut service, ids) = service_with_windows(2);
        service
            .handle_command(
                lege_window::WindowCommand::ArrangeWindows {
                    preset: lege_window::ArrangementPreset::Halves,
                },
                trusted(&service),
            )
            .unwrap();
        let settled = service.window(ids[1]).unwrap().geometry;

        // A third window maps: nothing moves until the next explicit press, then it joins.
        let third = service.on_toplevel_mapped(
            9,
            ClientKind::Application,
            WindowClaims::default(),
            "late".into(),
            Rect::new(700, 300, 300, 300),
        );
        assert_eq!(service.window(ids[1]).unwrap().geometry, settled);
        let effect = service
            .handle_command(
                lege_window::WindowCommand::ArrangeWindows {
                    preset: lege_window::ArrangementPreset::Halves,
                },
                trusted(&service),
            )
            .unwrap();
        let lege_window::CommandEffect::Arranged { moves } = effect else {
            panic!("expected Arranged");
        };
        assert!(moves.iter().any(|(id, _)| *id == third));

        // One closes, and the output shrinks under the baseline: restore skips the departed
        // and repairs the survivors onto what remains.
        service.on_toplevel_destroyed(ids[0]);
        service.on_outputs_changed(vec![Output::new("eDP-1", Rect::new(0, 0, 800, 600), 1000)]);
        let effect = service
            .handle_command(
                lege_window::WindowCommand::RestoreArrangement,
                trusted(&service),
            )
            .unwrap();
        let lege_window::CommandEffect::Arranged { moves } = effect else {
            panic!("expected Arranged");
        };
        assert!(moves.iter().all(|(id, _)| *id != ids[0]));
        let outputs = service.outputs().to_vec();
        for (_, rect) in &moves {
            assert!(
                lege_window::is_recoverable(*rect, &outputs),
                "restored geometry must stay reachable after the topology change: {rect:?}"
            );
        }
    }
}
