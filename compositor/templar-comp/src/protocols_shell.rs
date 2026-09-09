//! Server-side `templar_shell_v1`: the private, authenticated protocol between the compositor core
//! and the one connection accepted as the trusted shell (`src/shell_socket.rs`). The global itself
//! is filtered to that connection (`ShellManagerGlobalData::can_view`), and every request handler
//! below checks `client_is_trusted_shell` again — belt and suspenders, since this is the one
//! protocol boundary in the crate a bug in the filter would actually matter for.
//!
//! [`sync_toplevels`]/[`sync_foreign_toplevels`] (called from `src/space_sync.rs` after every
//! `WindowService`-mutating handler) are the only place these objects' state changes: nothing here
//! sends events synchronously from inside a request handler that touched `WindowService`, matching
//! COMPOSITOR.md's "never encode/send `templar_shell_v1` events synchronously from the same call site".

use crate::protocols::templar_shell_v1::{
    templar_shell_manager_v1::{self, TemplarShellManagerV1},
    templar_shell_toplevel_list_v1::{self, TemplarShellToplevelListV1},
    templar_shell_toplevel_v1::{self, TemplarShellToplevelV1},
};
use crate::state::{Backend, TemplarCompState, client_is_trusted_shell};
use crate::window_bridge::now_monotonic_ns;
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource, WEnum,
    backend::{ClientId, ObjectId},
    protocol::wl_surface::WlSurface,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use std::collections::{HashMap, HashSet};
use templar_comp_integration::ShellStateSnapshot;
use templar_window::{
    ActivationGrant, ClientKind, CommandContext, Layer, Rect, RevealHome, WindowCommand, WindowId,
    geometry::{Alignment, Edge, Exclusive, shell_surface_placement},
};

/// The protocol version this compositor implements (`interfaces/wayland/templar-shell-v1.xml`).
/// Both peers ship from this repository, so a version bump is a pair of edits, not a
/// compatibility matrix — but every `since="2"` event below is still guarded on the bound
/// object's version rather than assumed.
pub const SHELL_PROTOCOL_VERSION: u32 = 7;

/// The value of `templar_shell_manager_v1.group_cycle_binding` this compositor sends: 0 is
/// `Super+Tab`, the only binding there is (planning doc §2.1 — one chord, always the same, no
/// setting). The event exists so a future setting could move it without a protocol change.
pub const GROUP_CYCLE_BINDING_SUPER_TAB: u32 = 0;

/// Bookkeeping for currently-bound `templar_shell_v1` objects.
#[derive(Debug, Default)]
pub struct ShellProtocolState {
    /// Every bound manager: where `home_revealed` is reported, and whose destruction — the shell
    /// disconnecting — ends a reveal so a dead shell never leaves the user facing a blank backdrop.
    pub managers: Vec<TemplarShellManagerV1>,
    pub toplevel_lists: Vec<TemplarShellToplevelListV1>,
    pub toplevel_objects: HashMap<WindowId, TemplarShellToplevelV1>,
    /// Toplevel objects that have had their `identifier` event, which is sent exactly once.
    pub identified: HashSet<WindowId>,
    /// One entry per surface with a live `set_layer_surface` claim.
    pub layer_surfaces: HashMap<ObjectId, (WlSurface, templar_window::Layer)>,
    /// One entry per surface with a live `set_exclusive_zone` claim: which output it was on, and
    /// the claim itself. A claim of size 0 is a live claim — it anchors the surface to its edge
    /// and reserves nothing. Released only when the surface is destroyed
    /// (`templar-shell-v1.xml`'s `set_exclusive_zone` doc) — enforced in `on_shell_surface_destroyed`.
    pub exclusive_zones: HashMap<ObjectId, (String, Exclusive)>,
    /// One entry per surface that has sent `set_edge_alignment`, for the edge it named.
    ///
    /// Only here because the two requests may arrive in either order: the alignment lives on the
    /// claim in `exclusive_zones`, which is what every reader reads, and this is what a claim
    /// arriving *after* its alignment is built from. Cleared with the claim on destruction.
    pub edge_alignments: HashMap<ObjectId, (Edge, Alignment)>,
    /// The canonical application id of the selected `terminal` provider, from
    /// `set_terminal` (§4.6). Deliberately kept when the shell disconnects: the chord toggles an
    /// existing terminal window with the shell dead, and only *launching* one needs it back.
    /// Compared as a string against `CachedIdentity`'s resolved id — the compositor never
    /// interprets an application id, it only recognises one.
    pub terminal_app_id: Option<String>,
}

/// Global-visibility filter: only the trusted-shell connection ever sees `templar_shell_manager_v1`
/// exists at all, so an ordinary application cannot even attempt to bind it.
pub struct ShellManagerGlobalData;

impl<B: Backend + 'static> GlobalDispatch2<TemplarShellManagerV1, TemplarCompState<B>>
    for ShellManagerGlobalData
{
    fn bind(
        &self,
        state: &mut TemplarCompState<B>,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<TemplarShellManagerV1>,
        data_init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        let manager = data_init.init(resource, ());
        if manager.version() >= 2 {
            manager.home_revealed(u32::from(state.window_service.home_revealed()));
        }
        if manager.version() >= 5 {
            manager.group_cycle_binding(GROUP_CYCLE_BINDING_SUPER_TAB);
        }
        if manager.version() >= 7 {
            manager.bar_peeked(peeked_output(state));
        }
        state.shell_protocol.managers.push(manager);
    }

    fn can_view(&self, client: &Client) -> bool {
        client_is_trusted_shell(client)
    }
}

impl<B: Backend + 'static> Dispatch2<TemplarShellManagerV1, TemplarCompState<B>> for () {
    fn request(
        &self,
        state: &mut TemplarCompState<B>,
        client: &Client,
        _resource: &TemplarShellManagerV1,
        request: templar_shell_manager_v1::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        if !client_is_trusted_shell(client) {
            return;
        }
        let ctx = trusted_ctx(state);
        match request {
            templar_shell_manager_v1::Request::GetToplevelList { id } => {
                let list = data_init.init(id, ());
                for record in state.window_service.shell_toplevels() {
                    send_new_toplevel(state, &list, record.window_id);
                }
                state.shell_protocol.toplevel_lists.push(list);
            }
            templar_shell_manager_v1::Request::ShowAllWindows => {
                state.run_recovery_action(crate::keybindings::RecoveryAction::ShowAllWindows);
            }
            templar_shell_manager_v1::Request::EscapeFullscreenOrModal { toplevel } => {
                let window_id =
                    toplevel.and_then(|resource| window_id_for_toplevel(state, &resource));
                if let Ok(templar_window::CommandEffect::Escaped { action }) = state
                    .window_service
                    .handle_command(WindowCommand::EscapeFullscreenOrModal { window_id }, ctx)
                {
                    // Same as the recovery chord's branch: the geometry is restored, the client
                    // still needs the size and the cleared fullscreen bit.
                    if let templar_window::EscapeAction::ExitedFullscreen { window_id } = action {
                        crate::xdg_shell::apply_fullscreen(state, window_id, false);
                    }
                    crate::space_sync::sync_after_change(state);
                }
            }
            templar_shell_manager_v1::Request::SetLayerSurface { surface, layer } => {
                if let WEnum::Value(layer) = layer {
                    let layer = templar_comp_integration::protocol::layer_from_wire(layer as u32);
                    if let Some(layer) = layer
                        && !layer.is_core_only()
                    {
                        smithay::wayland::compositor::add_destruction_hook::<TemplarCompState<B>, _>(
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
            templar_shell_manager_v1::Request::SetExclusiveZone {
                surface,
                edge,
                size,
            } => {
                handle_set_exclusive_zone(state, &surface, edge, size);
            }
            templar_shell_manager_v1::Request::SetEdgeAlignment {
                surface,
                edge,
                alignment,
            } => {
                handle_set_edge_alignment(state, &surface, edge, alignment);
            }
            templar_shell_manager_v1::Request::RevealHome { reveal } => {
                if let WEnum::Value(reveal) = reveal {
                    let reveal = match reveal {
                        templar_shell_manager_v1::Reveal::Show => RevealHome::Show,
                        templar_shell_manager_v1::Reveal::Hide => RevealHome::Hide,
                        templar_shell_manager_v1::Reveal::Toggle => RevealHome::Toggle,
                    };
                    state.reveal_home(reveal);
                }
            }
            templar_shell_manager_v1::Request::ArrangeWindows { preset } => {
                if let WEnum::Value(preset) = preset {
                    crate::xdg_shell::apply_arrangement(
                        state,
                        WindowCommand::ArrangeWindows {
                            preset: preset_from_wire(preset),
                        },
                    );
                }
            }
            templar_shell_manager_v1::Request::RestoreArrangement => {
                crate::xdg_shell::apply_arrangement(state, WindowCommand::RestoreArrangement);
            }
            templar_shell_manager_v1::Request::SetIdleTimeout { seconds } => {
                // Policy in, enforcement here: the number is `Power1`'s `screen_off_after`, which
                // the shell read and relayed, and this process is the only one that can see the
                // seat go quiet (`src/idle.rs`).
                state.set_idle_timeout(seconds);
            }
            // Which application is "the terminal" is a Providers1 choice the shell relays; what
            // the chord does with it is the compositor's (§4.6, `src/keybindings.rs`).
            templar_shell_manager_v1::Request::SetTerminal { app_id } => {
                state.shell_protocol.terminal_app_id =
                    Some(app_id).filter(|app_id| !app_id.is_empty());
                tracing::info!(
                    terminal = ?state.shell_protocol.terminal_app_id,
                    "the drop-down terminal's application id"
                );
            }
        }
    }

    /// The manager going away is the shell going away (it is bound once, for the process's
    /// lifetime). A reveal it may have left behind ends here, so the windows come back without
    /// anyone having to press anything — the shell is a restartable process, and its death is
    /// not allowed to change what the user can reach.
    fn destroyed(
        &self,
        state: &mut TemplarCompState<B>,
        _client: ClientId,
        resource: &TemplarShellManagerV1,
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

/// The wire enum's values match `templar_window::ArrangementPreset`'s declaration order
/// (`interfaces/wayland/templar-shell-v1.xml`'s `arrangement_preset` doc); a test holds them in
/// agreement.
pub(crate) fn preset_from_wire(
    preset: templar_shell_manager_v1::ArrangementPreset,
) -> templar_window::ArrangementPreset {
    match preset {
        templar_shell_manager_v1::ArrangementPreset::Focus => {
            templar_window::ArrangementPreset::Focus
        }
        templar_shell_manager_v1::ArrangementPreset::Halves => {
            templar_window::ArrangementPreset::Halves
        }
        templar_shell_manager_v1::ArrangementPreset::Thirds => {
            templar_window::ArrangementPreset::Thirds
        }
        templar_shell_manager_v1::ArrangementPreset::Quarters => {
            templar_window::ArrangementPreset::Quarters
        }
        templar_shell_manager_v1::ArrangementPreset::MainAndStack => {
            templar_window::ArrangementPreset::MainAndStack
        }
        templar_shell_manager_v1::ArrangementPreset::Cushion => {
            templar_window::ArrangementPreset::Cushion
        }
    }
}

fn trusted_ctx<B: Backend + 'static>(state: &TemplarCompState<B>) -> CommandContext {
    CommandContext {
        trusted_shell: true,
        now_monotonic_ns: now_monotonic_ns(&state.clock),
    }
}

fn window_id_for_toplevel<B: Backend + 'static>(
    state: &TemplarCompState<B>,
    resource: &TemplarShellToplevelV1,
) -> Option<WindowId> {
    state
        .shell_protocol
        .toplevel_objects
        .iter()
        .find(|(_, object)| *object == resource)
        .map(|(id, _)| *id)
}

fn handle_set_exclusive_zone<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    surface: &WlSurface,
    edge: WEnum<templar_shell_manager_v1::Edge>,
    size: u32,
) {
    let Some(edge) = wire_edge(edge) else {
        return;
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
    // A claim of 0 is kept, not dropped: it reserves nothing (`work_area_for` subtracts 0) but it
    // is still what anchors the surface to its edge (`shell_surface_placement`). The bar collapsed
    // to a hint is the same dock as the bar showing a full row — one that happens to reserve
    // nothing — so shrinking it must not turn it into an unplaced surface that lands wherever it
    // last committed. Only destroying the surface releases the claim.
    //
    // Idempotent alongside `SetLayerSurface`'s identical registration: a surface that only
    // ever calls `set_exclusive_zone` (never `set_layer_surface`) still needs its claim
    // released on destruction.
    smithay::wayland::compositor::add_destruction_hook::<TemplarCompState<B>, _>(
        surface,
        |state, surface| on_shell_surface_destroyed(state, &surface.id()),
    );
    // The alignment may have arrived first (nothing orders the two requests), in which case it
    // is waiting in `edge_alignments` for the edge it named.
    let alignment = match state.shell_protocol.edge_alignments.get(&object_id) {
        Some((aligned_edge, alignment)) if *aligned_edge == edge => *alignment,
        _ => Alignment::Start,
    };
    state.shell_protocol.exclusive_zones.insert(
        object_id,
        (
            output_name,
            Exclusive {
                alignment,
                ..Exclusive::new(edge, size.min(i32::MAX as u32) as i32)
            },
        ),
    );
    // `recompute_outputs` re-places every claiming surface, this one included — a zone changing
    // and a monitor moving are the same event, so they must not have two different repair paths.
    crate::backend::recompute_outputs(state);
}

/// Where along its edge an anchored surface sits (`templar-shell-v1.xml`'s `set_edge_alignment`).
///
/// Placement, never reservation: `work_area_for` reads the claim's size and never its alignment,
/// so a surface sliding along its edge reflows nothing — which is what lets the shell move its
/// island off the bottom-left corner the compositor's hot corner owns
/// (`input::in_home_corner`) without touching a single window.
///
/// The alignment only means anything for a live claim on the same edge; an alignment for an edge
/// the surface does not claim is remembered but not applied, so the two requests may arrive in
/// either order and a later claim on that edge picks it up.
fn handle_set_edge_alignment<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    surface: &WlSurface,
    edge: WEnum<templar_shell_manager_v1::Edge>,
    alignment: WEnum<templar_shell_manager_v1::Alignment>,
) {
    let Some(edge) = wire_edge(edge) else {
        return;
    };
    let WEnum::Value(alignment) = alignment else {
        return;
    };
    let alignment = match alignment {
        templar_shell_manager_v1::Alignment::Start => Alignment::Start,
        templar_shell_manager_v1::Alignment::Centre => Alignment::Centre,
        templar_shell_manager_v1::Alignment::End => Alignment::End,
    };
    let object_id = surface.id();
    state
        .shell_protocol
        .edge_alignments
        .insert(object_id.clone(), (edge, alignment));
    let Some((_, claim)) = state.shell_protocol.exclusive_zones.get_mut(&object_id) else {
        return;
    };
    if claim.edge != edge || claim.alignment == alignment {
        return;
    }
    claim.alignment = alignment;
    // The same repair path a zone change takes: one entry point, so a surface moving along its
    // edge and a monitor moving cannot diverge.
    crate::backend::recompute_outputs(state);
}

/// The protocol's `edge` enum as this crate's. `None` for a value this compositor does not know —
/// a newer shell against an older compositor, which is silently ignored like a refused claim.
fn wire_edge(edge: WEnum<templar_shell_manager_v1::Edge>) -> Option<Edge> {
    let WEnum::Value(edge) = edge else {
        return None;
    };
    Some(match edge {
        templar_shell_manager_v1::Edge::Top => Edge::Top,
        templar_shell_manager_v1::Edge::Bottom => Edge::Bottom,
        templar_shell_manager_v1::Edge::Left => Edge::Left,
        templar_shell_manager_v1::Edge::Right => Edge::Right,
    })
}

/// Apply the trusted shell's claims once the corresponding xdg-toplevel exists in
/// `WindowService`. The protocol requests normally arrive before the first buffer/map commit, so
/// storing them is not enough: mapping must consume them and configure the client to the claimed
/// output geometry. This also handles a claim changed after mapping.
pub fn apply_surface_claims<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
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
    // Placement is idempotent, and applying an unchanged one has to be too. A shell surface
    // commits on every frame it draws, and `xdg_shell::adopt_client_size` routes any commit whose
    // size differs from where the surface sits straight back here — including a size the
    // placement will never grant, because it floors the thickness at the zone and clamps the
    // extent to the output. Re-configuring a client with a size it has already refused, once per
    // frame, is an infinite round trip; so is re-configuring every claiming surface on every
    // topology change (`reapply_surface_claims`) when none of them moved.
    let moved = state
        .window_service
        .window(window_id)
        .is_none_or(|record| record.geometry != geometry);
    state.window_service.set_geometry(window_id, geometry);

    if moved {
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
    }
    // Outside the guard: a shell that restarts and maps its Home Surface at exactly the geometry
    // it will be placed at still has to be given the keyboard.
    if layer == Layer::HomeSurface {
        crate::input::focus_home_if_needed(state, surface);
    }
}

/// Whether the trusted shell placed this surface itself — a live `set_layer_surface` or
/// `set_exclusive_zone` claim. The one definition of "this surface has no frame of its own", so
/// the map path (`xdg_shell::handle_toplevel_commit`, which must not add a tag strip to what such
/// a surface asked for) and the claim path here cannot drift apart about which surfaces those are.
pub fn has_surface_claim<B: Backend + 'static>(
    state: &TemplarCompState<B>,
    surface: &WlSurface,
) -> bool {
    let object_id = surface.id();
    state.shell_protocol.layer_surfaces.contains_key(&object_id)
        || state
            .shell_protocol
            .exclusive_zones
            .contains_key(&object_id)
}

/// Re-place every surface with a live layer or exclusive-zone claim against the topology as it
/// now stands. Called from `backend::recompute_outputs`, which is the one entry point for a
/// topology change (COMPOSITOR.md): an output resized, unplugged or rescaled moves where a bar's
/// edge *is*, and a shell surface's position comes from `shell_surface_placement` alone —
/// `WindowService::on_outputs_changed` deliberately does not repair one, because a surface
/// holding a zone can never satisfy the usable area its own zone creates.
///
/// No recursion: nothing on the `apply_surface_claims` path calls `recompute_outputs`.
pub fn reapply_surface_claims<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    let claimed: HashSet<ObjectId> = state
        .shell_protocol
        .layer_surfaces
        .keys()
        .chain(state.shell_protocol.exclusive_zones.keys())
        .cloned()
        .collect();
    for object_id in claimed {
        let surface = state
            .windows
            .window_id_for(&object_id)
            .and_then(|window_id| state.windows.surface_for(window_id))
            .cloned();
        if let Some(surface) = surface {
            apply_surface_claims_if_mapped(state, &surface);
        }
    }
}

fn apply_surface_claims_if_mapped<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
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
/// `templar_shell_v1` doc's "reservations are per surface" contract.
pub fn on_shell_surface_destroyed<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    surface: &ObjectId,
) {
    state.shell_protocol.layer_surfaces.remove(surface);
    state.shell_protocol.edge_alignments.remove(surface);
    if state
        .shell_protocol
        .exclusive_zones
        .remove(surface)
        .is_some()
    {
        crate::backend::recompute_outputs(state);
    }
}

impl<B: Backend + 'static> Dispatch2<TemplarShellToplevelListV1, TemplarCompState<B>> for () {
    fn request(
        &self,
        _state: &mut TemplarCompState<B>,
        _client: &Client,
        _resource: &TemplarShellToplevelListV1,
        _request: templar_shell_toplevel_list_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
    }

    fn destroyed(
        &self,
        state: &mut TemplarCompState<B>,
        _client: ClientId,
        resource: &TemplarShellToplevelListV1,
    ) {
        state
            .shell_protocol
            .toplevel_lists
            .retain(|list| list != resource);
    }
}

impl<B: Backend + 'static> Dispatch2<TemplarShellToplevelV1, TemplarCompState<B>> for () {
    fn request(
        &self,
        state: &mut TemplarCompState<B>,
        client: &Client,
        resource: &TemplarShellToplevelV1,
        request: templar_shell_toplevel_v1::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        if !client_is_trusted_shell(client) {
            return;
        }
        let Some(window_id) = window_id_for_toplevel(state, resource) else {
            return;
        };
        let ctx = trusted_ctx(state);
        match request {
            templar_shell_toplevel_v1::Request::Activate { activation_token } => {
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
            templar_shell_toplevel_v1::Request::Close => {
                if let Ok(templar_window::CommandEffect::Closed { .. }) = state
                    .window_service
                    .handle_command(WindowCommand::Close { window_id }, ctx)
                {
                    crate::backend::close_window(state, window_id);
                }
            }
            templar_shell_toplevel_v1::Request::MoveToVisibleArea => {
                if state
                    .window_service
                    .handle_command(WindowCommand::MoveToVisibleArea { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            templar_shell_toplevel_v1::Request::Shelve => {
                if state
                    .window_service
                    .handle_command(WindowCommand::Shelve { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            templar_shell_toplevel_v1::Request::Unshelve => {
                if state
                    .window_service
                    .handle_command(WindowCommand::Unshelve { window_id }, ctx)
                    .is_ok()
                {
                    crate::space_sync::sync_after_change(state);
                }
            }
            templar_shell_toplevel_v1::Request::SetMaximized { maximized } => {
                crate::xdg_shell::apply_maximized(state, window_id, maximized != 0);
            }
            // Window groups (version 3). Every one of these is silently ignored for a window the
            // compositor no longer knows or a group that no longer exists: the shell's view of a
            // group arrives by event and can always be one frame stale, and a stale request is
            // not a protocol error.
            templar_shell_toplevel_v1::Request::JoinGroup { other } => {
                // `this.join_group(other)` joins *this* window to `other`'s group, so `other` is
                // the anchor whose group identity survives a merge.
                if let Some(anchor) = window_id_for_toplevel(state, &other) {
                    crate::window_groups::group_windows(state, anchor, window_id);
                }
            }
            templar_shell_toplevel_v1::Request::LeaveGroup => {
                crate::window_groups::ungroup_window(state, window_id);
            }
            templar_shell_toplevel_v1::Request::ActivateGroup { activation_token } => {
                // The same trusted activation `activate` performs — a group is not a way around
                // the focus-stealing checks — and `WindowService::activate` raises the whole
                // group with the named member on top.
                let grant = ActivationGrant {
                    token: activation_token,
                    requesting_instance: None,
                    target_instance: None,
                    expires_at_monotonic_ns: ctx.now_monotonic_ns.saturating_add(5_000_000_000),
                    had_recent_input_evidence: true,
                };
                if state
                    .window_service
                    .handle_command(WindowCommand::ActivateGroup { window_id, grant }, ctx)
                    .is_ok()
                {
                    crate::input::activate_window(state, window_id);
                    crate::space_sync::sync_after_change(state);
                }
            }
            templar_shell_toplevel_v1::Request::ShelveGroup => {
                crate::window_groups::shelve_group(state, window_id);
            }
            templar_shell_toplevel_v1::Request::UnshelveGroup => {
                crate::window_groups::unshelve_group(state, window_id);
            }
            templar_shell_toplevel_v1::Request::SetGroupName { name } => {
                crate::window_groups::set_group_name(state, window_id, name);
            }
        }
    }

    fn destroyed(
        &self,
        state: &mut TemplarCompState<B>,
        _client: ClientId,
        resource: &TemplarShellToplevelV1,
    ) {
        state
            .shell_protocol
            .toplevel_objects
            .retain(|_, object| object != resource);
    }
}

/// One toplevel's window-group membership, sent right after `identity` exactly as
/// `interfaces/wayland/templar-shell-v1.xml` promises: `group_id` 0 for a window in no group, and
/// then an empty name and no accent. A no-op on an object below version 3.
///
/// Sent on every sync rather than only on change, like every other event here — `sync_toplevels`
/// always sends a full state (`src/space_sync.rs`'s module doc), and a group that joined, was
/// renamed or dissolved is exactly the kind of change a diff would be tempted to miss.
fn send_group(object: &TemplarShellToplevelV1, toplevel: &templar_window::ShellToplevel) {
    if object.version() < 3 {
        return;
    }
    match &toplevel.group {
        Some(group) => object.group(
            // A group id is a `u64` in the model and a `uint` on the wire. Saturating rather than
            // wrapping, because wrapping could land on 0 — which means "no group" — and tell the
            // shell the exact opposite of the truth.
            u32::try_from(group.group_id).unwrap_or(u32::MAX),
            group.name.clone(),
            group.accent,
        ),
        None => object.group(0, String::new(), 0),
    }
}

/// Everything a version-2 `templar_shell_toplevel_v1` carries beyond version 1: the foreign-toplevel
/// identifier (once), the geometry and the maximized flag. A no-op on a version-1 object.
fn send_v2_toplevel_events<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    object: &TemplarShellToplevelV1,
    toplevel: &templar_window::ShellToplevel,
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
    state: &mut TemplarCompState<B>,
    list: &TemplarShellToplevelListV1,
    window_id: WindowId,
) {
    let Some(client) = list.client() else { return };
    let Ok(toplevel) = client.create_resource::<TemplarShellToplevelV1, (), TemplarCompState<B>>(
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

/// Push every `templar_shell_toplevel_v1` up to date with `snapshot`: create objects for
/// newly-mapped windows, refresh identity/title/state/layer on every one, and close+drop objects
/// for windows no longer present. Always sends a full state, not a diff — simple and correct at
/// the cost of some redundant wire traffic (`src/space_sync.rs`'s module doc).
pub fn sync_toplevels<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
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
        send_group(&object, toplevel);
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

    // Read live rather than from `snapshot`: the peek is a pointer state the compositor changes
    // between snapshots, and every change publishes another one, so the last sync wins either way.
    let peeked = peeked_output(state);
    for manager in &state.shell_protocol.managers {
        if manager.version() >= 2 {
            manager.home_revealed(u32::from(snapshot.home_revealed));
        }
        if manager.version() >= 7 {
            manager.bar_peeked(peeked.clone());
        }
    }
}

/// The output whose bottom edge the pointer is asking for, as `bar_peeked` sends it: the empty
/// string for none, because a Wayland `string` argument is not nullable here.
fn peeked_output<B: Backend + 'static>(state: &TemplarCompState<B>) -> String {
    state
        .window_service
        .peek_output()
        .unwrap_or_default()
        .to_owned()
}

/// `templar_window::Layer`'s wire mapping is fixed by `templar-shell-v1.xml`'s `layer` enum
/// (`interfaces/wayland/README.md`); `templar_comp_integration::protocol::layer_to_wire` gives the
/// numeric discriminant, matched here against the wayland-scanner-generated enum's own variants
/// once rather than trusted to line up by transmute.
fn wire_layer(layer: Layer) -> templar_shell_manager_v1::Layer {
    match layer {
        Layer::SecureLock => templar_shell_manager_v1::Layer::SecureLock,
        Layer::Recovery => templar_shell_manager_v1::Layer::Recovery,
        Layer::SystemModal => templar_shell_manager_v1::Layer::SystemModal,
        Layer::ShellOverlay => templar_shell_manager_v1::Layer::ShellOverlay,
        Layer::Notification => templar_shell_manager_v1::Layer::Notification,
        Layer::AppOnTop => templar_shell_manager_v1::Layer::AppOnTop,
        Layer::Normal => templar_shell_manager_v1::Layer::Normal,
        Layer::HomeSurface => templar_shell_manager_v1::Layer::HomeSurface,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::WindowClaims;
    use templar_window::{ClientKind, Output, WindowService};

    struct NullCache;
    impl templar_window::IdentityCache for NullCache {
        fn resolve_cached(
            &self,
            _pid: u32,
            _claims: &WindowClaims,
        ) -> Option<templar_window::CachedIdentity> {
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

    /// The protocol enum's wire values follow `templar_window::ArrangementPreset`'s declaration
    /// order, as `interfaces/wayland/templar-shell-v1.xml` promises.
    #[test]
    fn arrangement_wire_values_match_the_window_service_presets() {
        for (index, preset) in templar_window::ArrangementPreset::ALL
            .into_iter()
            .enumerate()
        {
            let wire = templar_shell_manager_v1::ArrangementPreset::try_from(
                u32::try_from(index).unwrap(),
            )
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
                templar_window::WindowCommand::ArrangeWindows {
                    preset: templar_window::ArrangementPreset::Halves,
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
                templar_window::WindowCommand::ArrangeWindows {
                    preset: templar_window::ArrangementPreset::Halves,
                },
                trusted(&service),
            )
            .unwrap();
        let templar_window::CommandEffect::Arranged { moves } = effect else {
            panic!("expected Arranged");
        };
        assert!(moves.iter().any(|(id, _)| *id == third));

        // One closes, and the output shrinks under the baseline: restore skips the departed
        // and repairs the survivors onto what remains.
        service.on_toplevel_destroyed(ids[0]);
        service.on_outputs_changed(vec![Output::new("eDP-1", Rect::new(0, 0, 800, 600), 1000)]);
        let effect = service
            .handle_command(
                templar_window::WindowCommand::RestoreArrangement,
                trusted(&service),
            )
            .unwrap();
        let templar_window::CommandEffect::Arranged { moves } = effect else {
            panic!("expected Arranged");
        };
        assert!(moves.iter().all(|(id, _)| *id != ids[0]));
        let outputs = service.outputs().to_vec();
        for (_, rect) in &moves {
            assert!(
                templar_window::is_recoverable(*rect, &outputs),
                "restored geometry must stay reachable after the topology change: {rect:?}"
            );
        }
    }
}
