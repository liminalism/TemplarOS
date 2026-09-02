//! `xdg_shell` and `zxdg_toplevel_decoration_v1`: the compositor-facing half of COMPOSITOR.md's
//! table rows for mapping, title, decoration and fullscreen. `CompositorHandler::commit` lives
//! here too because the moment a toplevel becomes mapped ([`window_bridge::should_map_toplevel`])
//! is detected at commit time, not at `new_toplevel` (which fires before any content exists).
//!
//! Every toplevel is placed in `Space` immediately at `new_toplevel` (before it is mapped into
//! `WindowService` at all) purely so Smithay's own commit/configure machinery — which several
//! `desktop` helpers key off `Space` lookups — has somewhere to find it; `Space` is never
//! consulted for placement or policy (`state.rs`'s doc comment, `src/space_sync.rs`).

use crate::state::{Backend, LegeCompState};
use crate::window_bridge::{claims_from_wayland, client_kind, now_monotonic_ns};
use lege_window::{CommandContext, CommandEffect, Decoration, WindowCommand, WindowId};
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::{
        PopupKind, PopupManager, Window, find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::Seat,
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
        wayland_server::{
            Client, Resource,
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        shell::xdg::{
            Configure, PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler,
            XdgShellState, XdgToplevelSurfaceData,
            decoration::XdgDecorationHandler,
            dialog::{ToplevelDialogHint, XdgDialogHandler},
        },
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};

impl<B: Backend + 'static> CompositorHandler for LegeCompState<B> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland's own connection carries Smithay's `XWaylandClientData`, not our `ClientState`
        // (`src/xwayland.rs`'s `XWayland::spawn` inserts it internally) — every other connection
        // goes through `crate::state::init_wayland_listener`/`crate::shell_socket::install`, both
        // of which always insert `ClientState`, so those two cases are exhaustive.
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<crate::state::ClientState>()
            .expect("every non-XWayland client is inserted with ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend_data.early_import(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = window_for_surface(self, &root) {
                window.on_commit();
            }
        }

        self.popups.commit(surface);
        ensure_initial_popup_configure(&mut self.popups, surface);
        self.handle_toplevel_commit(surface);
    }
}

/// The `Window` wrapping root `WlSurface` `surface` — from `WindowIndex` first, so a window that
/// is currently *unmapped* from `Space` (shelved, or hidden behind a "go home" reveal;
/// `src/space_sync.rs`) keeps its commit/configure bookkeeping running while out of sight and
/// comes back with a current buffer. `Space` is only the fallback for anything the index has not
/// been told about (surface-tree bookkeeping, not placement; see the module doc).
fn window_for_surface<B: Backend + 'static>(
    state: &LegeCompState<B>,
    surface: &WlSurface,
) -> Option<Window> {
    if let Some(window) = state.windows.element_for(&surface.id()) {
        return Some(window.clone());
    }
    state
        .space
        .elements()
        .find(|window| window.wl_surface().as_deref() == Some(surface))
        .cloned()
}

/// `xdg_toplevel.set_maximized`/`unset_maximized` (and the same from the shell): let
/// `WindowService` decide the geometry, then tell the client through its pending state and one
/// configure. Nothing to send when the window was already in the requested state.
/// Apply an `ArrangeWindows` / `RestoreArrangement` command and configure every moved window
/// with its new size — a restore also re-asserts the window's restored maximize state.
pub fn apply_arrangement<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    command: WindowCommand,
) {
    let ctx = CommandContext {
        trusted_shell: true,
        now_monotonic_ns: now_monotonic_ns(&state.clock),
    };
    let Ok(CommandEffect::Arranged { moves }) = state.window_service.handle_command(command, ctx)
    else {
        return;
    };
    // Arranging windows is a request to look at them: the controls live on the Home Field's
    // menu, so without this the preset would be applied entirely behind the reveal that is
    // hiding every window it just moved — pressing the button would appear to do nothing.
    if !moves.is_empty() && state.window_service.home_revealed() {
        state.reveal_home(lege_window::RevealHome::Hide);
    }
    for (window_id, _geometry) in moves {
        let maximized = state
            .window_service
            .window(window_id)
            .is_some_and(|window| window.maximized);
        let client = crate::space_sync::client_area(state, window_id);
        if let Some(window) = crate::space_sync::element_for_window(state, window_id)
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.with_pending_state(|pending| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                if maximized {
                    pending.states.set(State::Maximized);
                } else {
                    pending.states.unset(State::Maximized);
                }
                pending.size = Some((client.width, client.height).into());
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }
    }
    crate::space_sync::sync_after_change(state);
}

pub fn apply_maximized<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    window_id: WindowId,
    maximized: bool,
) {
    let ctx = CommandContext {
        trusted_shell: true,
        now_monotonic_ns: now_monotonic_ns(&state.clock),
    };
    let Ok(CommandEffect::Maximized {
        geometry: Some(_), ..
    }) = state.window_service.handle_command(
        WindowCommand::SetMaximized {
            window_id,
            maximized,
        },
        ctx,
    )
    else {
        return;
    };
    let client = crate::space_sync::client_area(state, window_id);
    if let Some(window) = crate::space_sync::element_for_window(state, window_id) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|pending| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                if maximized {
                    pending.states.set(State::Maximized);
                } else {
                    pending.states.unset(State::Maximized);
                }
                pending.size = Some((client.width, client.height).into());
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        } else if let Some(x11) = window.x11_surface() {
            // An X11 window is told its whole placement in one request; the band button is
            // the only maximize route it has.
            let _ = x11.set_maximized(maximized);
            let _ = x11.configure(Some(smithay::utils::Rectangle::new(
                smithay::utils::Point::from((client.x, client.y)),
                smithay::utils::Size::from((client.width, client.height)),
            )));
        }
    }
    crate::space_sync::sync_after_change(state);
}

/// `xdg_toplevel.resize_edge` in this repository's own vocabulary
/// ([`lege_window::ResizeEdges`]). The protocol enum is a set of corner names rather than flags,
/// so this is a table and not arithmetic; anything unrecognised (the enum is `non_exhaustive`,
/// and `none` is a legal value that asks for nothing) becomes the empty set, which
/// `WindowService::begin_resize` refuses.
fn resize_edges(
    edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
) -> lege_window::ResizeEdges {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as Wire;
    let (left, right, top, bottom) = match edges {
        Wire::Top => (false, false, true, false),
        Wire::Bottom => (false, false, false, true),
        Wire::Left => (true, false, false, false),
        Wire::Right => (false, true, false, false),
        Wire::TopLeft => (true, false, true, false),
        Wire::TopRight => (false, true, true, false),
        Wire::BottomLeft => (true, false, false, true),
        Wire::BottomRight => (false, true, false, true),
        _ => (false, false, false, false),
    };
    lege_window::ResizeEdges {
        left,
        right,
        top,
        bottom,
    }
}

fn ensure_initial_popup_configure(popups: &mut PopupManager, surface: &WlSurface) {
    if let Some(popup) = popups.find_popup(surface)
        && let PopupKind::Xdg(ref xdg) = popup
        && !xdg.is_initial_configure_sent()
    {
        // Smithay only refuses this for a genuinely malformed positioner, which the client
        // already violated protocol to produce — nothing left for the compositor to repair.
        xdg.send_configure().expect("initial popup configure");
    }
}

impl<B: Backend + 'static> LegeCompState<B> {
    /// The one place a toplevel's mapped-ness is decided (`window_bridge::should_map_toplevel`),
    /// and the one place its title is kept in sync once mapped (`xdg_toplevel.set_title`'s row in
    /// COMPOSITOR.md — Smithay delivers this as [`XdgShellHandler::title_changed`] instead, which
    /// only fires post-map, so the just-mapped title is read here directly from
    /// [`XdgToplevelSurfaceData`] instead of waiting for a redundant first change notification).
    fn handle_toplevel_commit(&mut self, surface: &WlSurface) {
        let Some(window) = window_for_surface(self, surface) else {
            return;
        };
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        if toplevel.wl_surface() != surface {
            return;
        }

        if !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
            return;
        }

        let object_id = surface.id();
        if self.windows.is_mapped(&object_id) {
            self.adopt_client_size(surface, &window);
            return;
        }
        self.windows.track_pending(object_id.clone());

        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                s.buffer().is_some()
            })
            .unwrap_or(false);
        if !self
            .windows
            .ready_to_map(&object_id, has_buffer, toplevel.is_initial_configure_sent())
        {
            return;
        }

        // `window.geometry()` (Smithay's `Window::geometry`) itself calls `with_states` on this
        // exact surface — nesting it inside our own `with_states` below would try to re-lock the
        // same non-reentrant per-surface mutex from the same thread and deadlock the whole render
        // loop. Read it first, outside any `with_states` closure.
        let geometry = window.geometry();
        let (title, app_id) = with_states(surface, |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("xdg toplevel surfaces always carry XdgToplevelSurfaceData")
                .lock()
                .unwrap();
            (data.title.clone(), data.app_id.clone())
        });

        let client = surface.client();
        let peer_pid = client
            .clone()
            .and_then(|client| client.get_credentials(&self.display_handle).ok())
            .map_or(0, |credentials| credentials.pid.max(0) as u32);
        let trusted_shell =
            client.is_some_and(|client| crate::state::client_is_trusted_shell(&client));
        let claims = claims_from_wayland(app_id);
        // The client's size, asked for as the frame it needs: the band goes above the client
        // (`lege_window::geometry::client_area`), so what is placed and clamped is the whole of
        // what will be on screen. The geometry's own location is the window's offset inside its
        // surface (a client-side shadow margin), not a position — an xdg client cannot ask for
        // one — and would otherwise read as a placed window that skips centring.
        let requested = lege_window::frame_around(lege_window::Rect::new(
            0,
            0,
            geometry.size.w,
            geometry.size.h,
        ));

        let window_id = self.window_service.on_toplevel_mapped(
            peer_pid,
            client_kind(trusted_shell),
            claims,
            title.unwrap_or_default(),
            requested,
        );
        self.windows.mark_mapped(surface.clone(), window_id);
        crate::protocols_shell::apply_surface_claims(self, surface, window_id, requested);

        let repaired = self
            .window_service
            .window(window_id)
            .map_or(requested, |record| record.geometry);
        // A shell surface has no frame, so this is the whole geometry for one; a client window
        // sits under its band.
        let client = crate::space_sync::client_area(self, window_id);
        self.space
            .map_element(window.clone(), (client.x, client.y), false);
        // Placement may have clamped an oversized request to the output's usable area
        // (`lege_window::place_new_window`); the client only learns a size through a configure.
        if (client.width, client.height) != (geometry.size.w, geometry.size.h) {
            toplevel.with_pending_state(|pending| {
                pending.size = Some((client.width, client.height).into());
            });
            toplevel.send_configure();
        }
        tracing::info!(
            ?window_id,
            requested = ?requested,
            geometry = ?repaired,
            layer = ?self.window_service.layer_of(window_id),
            recoverable = lege_window::is_recoverable(repaired, self.window_service.outputs()),
            "toplevel mapped"
        );
        crate::space_sync::sync_after_change(self);
    }
}

impl<B: Backend + 'static> LegeCompState<B> {
    /// A mapped client committed a size of its own choosing — a terminal restoring its saved
    /// size, an application growing for a new document — so the frame follows it: the record's
    /// geometry becomes the frame around the committed size, at the position the window had.
    ///
    /// Not while the compositor is the one changing the size: an interactive resize and a
    /// maximize both configure sizes the client has yet to catch up with, and a commit that
    /// still carries the older size is the client catching up, not choosing. The tell is the
    /// acknowledgement: only once the client has acked the size it was last given does a
    /// different committed size mean it wants that size.
    fn adopt_client_size(&mut self, surface: &WlSurface, window: &Window) {
        let Some(window_id) = self.windows.window_id_for(&surface.id()) else {
            return;
        };
        let Some(record) = self.window_service.window(window_id) else {
            return;
        };
        if record.maximized
            || record.fullscreen
            || self.window_service.is_resizing(window_id)
            || !self.window_service.has_frame(window_id)
        {
            return;
        }
        let committed = window.geometry().size;
        let client = crate::space_sync::client_area(self, window_id);
        if (committed.w, committed.h) == (client.width, client.height) {
            return;
        }
        let caught_up = with_states(surface, |states| {
            let attributes = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("xdg toplevel surfaces always carry XdgToplevelSurfaceData")
                .lock()
                .unwrap();
            let sent = attributes.current_server_state().size;
            let acked = attributes
                .last_acked
                .as_ref()
                .and_then(|configure| configure.state.size);
            sent.is_none() || acked == sent
        });
        if !caught_up {
            return;
        }
        let frame = lege_window::frame_around(lege_window::Rect::new(
            client.x,
            client.y,
            committed.w,
            committed.h,
        ));
        self.window_service.set_geometry(window_id, frame);
        crate::space_sync::sync_after_change(self);
    }
}

impl<B: Backend + 'static> XdgShellHandler for LegeCompState<B> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let object_id = surface.wl_surface().id();
        let window = Window::new_wayland_window(surface);
        self.windows.track_element(object_id, window.clone());
        // Placed off-screen at the origin until `handle_toplevel_commit` maps it through
        // `WindowService::on_toplevel_mapped` and repairs its geometry — nothing reads this
        // location as policy before that point.
        self.space.map_element(window, (0, 0), false);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let object_id = surface.wl_surface().id();
        if let Some(window) = window_for_surface(self, surface.wl_surface()) {
            self.space.unmap_elem(&window);
        }
        if let Some(window_id) = self.windows.remove(&object_id) {
            tracing::info!(?window_id, "toplevel destroyed");
            crate::space_sync::on_window_destroyed(self, window_id);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let object_id = surface.wl_surface().id();
        let Some(window_id) = self.windows.window_id_for(&object_id) else {
            return;
        };
        let title = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|data| data.lock().unwrap().title.clone().unwrap_or_default())
        })
        .unwrap_or_default();
        self.window_service.set_title(window_id, title);
        crate::space_sync::sync_after_change(self);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // No popup-dismiss-on-outside-click grab this milestone: popups still work (they map,
        // render and receive input while focused), they just don't auto-close on an outside
        // click/tap yet. Tracked as a follow-up, not a correctness gap in the geometry contract
        // this crate exists to uphold.
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) else {
            return;
        };
        crate::input::start_drag(self, &seat, serial, window_id);
    }

    /// `xdg_toplevel.resize`: the client's own resize border was dragged. The gesture itself is
    /// the compositor's to run — the client only says which edges the user grabbed — so this
    /// installs the same [`crate::input::start_resize`] grab the compositor's own corner grip
    /// uses, and `WindowService` stays the one authority for the resulting geometry.
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    ) {
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) else {
            return;
        };
        crate::input::start_resize(self, &seat, serial, window_id, resize_edges(edges));
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        surface.with_pending_state(|state| {
            state
                .states
                .set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
        });
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            self.window_service.set_fullscreen(window_id, true);
            crate::space_sync::sync_after_change(self);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state
                .states
                .unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
        });
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            self.window_service.set_fullscreen(window_id, false);
            crate::space_sync::sync_after_change(self);
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            self.window_service.shelve(window_id);
            crate::space_sync::sync_after_change(self);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            apply_maximized(self, window_id, true);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            apply_maximized(self, window_id, false);
        }
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: Configure) {}
}

impl<B: Backend + 'static> LegeCompState<B> {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = window_for_surface(self, &root) else {
            return;
        };
        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(&window) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

impl<B: Backend + 'static> XdgDecorationHandler for LegeCompState<B> {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // The compositor draws the frame: the recovery band above every client window is a
        // title bar with park, maximize and close controls (`chrome::collect_bands`,
        // `chrome::layout::band_buttons`), and the window is movable by it, resizable by its
        // corner grip and its outside edges (`lege_window::geometry::resize_edges_at`). A client
        // that also drew its own title bar would be showing two, so server-side is the answer
        // unless the client insists on client-side in `request_mode`.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        self.apply_decoration(&toplevel, Mode::ServerSide);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: Mode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        self.apply_decoration(&toplevel, mode);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        self.apply_decoration(&toplevel, Mode::ServerSide);
    }
}

impl<B: Backend + 'static> LegeCompState<B> {
    fn apply_decoration(&mut self, toplevel: &ToplevelSurface, mode: Mode) {
        let Some(window_id) = self.windows.window_id_for(&toplevel.wl_surface().id()) else {
            return;
        };
        let decoration = if mode == Mode::ServerSide {
            Decoration::ServerSide
        } else {
            Decoration::ClientSide
        };
        self.window_service.set_decoration(window_id, decoration);
        crate::space_sync::sync_after_change(self);
    }
}

/// `xdg_dialog_v1`: native Wayland modal-dialog tracking, the counterpart to
/// `src/xwayland.rs`'s `modal_request`/`unmodal_request` for XWayland windows — COMPOSITOR.md's
/// "compositor's own modal-dialog tracking (`xdg_dialog_v1`, or transient-for heuristics for
/// XWayland)" row.
impl<B: Backend + 'static> XdgDialogHandler for LegeCompState<B> {
    fn dialog_hint_changed(&mut self, toplevel: ToplevelSurface, hint: ToplevelDialogHint) {
        let Some(window_id) = self.windows.window_id_for(&toplevel.wl_surface().id()) else {
            return;
        };
        self.window_service
            .set_modal(window_id, hint == ToplevelDialogHint::Modal);
        crate::space_sync::sync_after_change(self);
    }
}

impl<B: Backend + 'static> XdgActivationHandler for LegeCompState<B> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let Some(window_id) = self.windows.window_id_for(&surface.id()) else {
            return;
        };
        let had_recent_input_evidence = token_data
            .serial
            .as_ref()
            .and_then(|(serial, _seat_resource)| {
                self.seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.last_enter())
                    .map(|last_enter| serial.is_no_older_than(&last_enter))
            })
            .unwrap_or(false);

        let now_monotonic_ns = now_monotonic_ns(&self.clock);
        let grant = lege_window::ActivationGrant {
            token: token.as_str().to_owned(),
            requesting_instance: None,
            target_instance: None,
            expires_at_monotonic_ns: now_monotonic_ns.saturating_add(5_000_000_000),
            had_recent_input_evidence,
        };
        let ctx = lege_window::CommandContext {
            trusted_shell: true,
            now_monotonic_ns,
        };
        if self
            .window_service
            .handle_command(
                lege_window::WindowCommand::Activate { window_id, grant },
                ctx,
            )
            .is_ok()
        {
            crate::input::activate_window(self, window_id);
            crate::space_sync::sync_after_change(self);
        }
    }
}
