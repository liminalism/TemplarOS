//! `xdg_shell` and `zxdg_toplevel_decoration_v1`: the compositor-facing half of COMPOSITOR.md's
//! table rows for mapping, title, decoration and fullscreen. `CompositorHandler::commit` lives
//! here too because the moment a toplevel becomes mapped ([`window_bridge::should_map_toplevel`])
//! is detected at commit time, not at `new_toplevel` (which fires before any content exists).
//!
//! Every toplevel is placed in `Space` immediately at `new_toplevel` (before it is mapped into
//! `WindowService` at all) purely so Smithay's own commit/configure machinery — which several
//! `desktop` helpers key off `Space` lookups — has somewhere to find it; `Space` is never
//! consulted for placement or policy (`state.rs`'s doc comment, `src/space_sync.rs`).

use crate::state::{Backend, TemplarCompState};
use crate::window_bridge::{claims_from_wayland, client_kind, now_monotonic_ns};
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
            backend::ObjectId,
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{SERIAL_COUNTER, Serial},
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
use templar_window::{CommandContext, CommandEffect, Decoration, WindowCommand, WindowId};

const ACTIVATION_LIFETIME: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_PENDING_ACTIVATIONS: usize = 64;

/// Only known, live xdg toplevels can enter this queue. Object ids do not retain a surface;
/// destruction removes the entry, and both insertion and mapping prune expired authority.
/// The key is generic so the lifecycle policy can be exercised without a Wayland connection.
#[derive(Debug)]
pub(crate) struct PendingActivations<S = ObjectId> {
    entries: Vec<(S, templar_window::ActivationGrant)>,
}

impl<S> Default for PendingActivations<S> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<S: PartialEq> PendingActivations<S> {
    fn prune(&mut self, now_monotonic_ns: u64) {
        self.entries.retain(|(_, grant)| {
            grant.had_recent_input_evidence && now_monotonic_ns < grant.expires_at_monotonic_ns
        });
    }

    fn remove(&mut self, surface: &S) {
        self.entries.retain(|(candidate, _)| candidate != surface);
    }

    fn insert(
        &mut self,
        surface: S,
        grant: templar_window::ActivationGrant,
        now_monotonic_ns: u64,
    ) {
        self.prune(now_monotonic_ns);
        if !grant.had_recent_input_evidence || now_monotonic_ns >= grant.expires_at_monotonic_ns {
            return;
        }
        if let Some((_, pending)) = self
            .entries
            .iter_mut()
            .find(|(candidate, _)| candidate == &surface)
        {
            *pending = grant;
        } else if self.entries.len() < MAX_PENDING_ACTIVATIONS {
            self.entries.push((surface, grant));
        }
    }

    fn take(
        &mut self,
        surface: &S,
        now_monotonic_ns: u64,
    ) -> Option<templar_window::ActivationGrant> {
        self.prune(now_monotonic_ns);
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == surface)?;
        Some(self.entries.swap_remove(index).1)
    }
}

fn activation_grant(
    token: String,
    age: std::time::Duration,
    now_monotonic_ns: u64,
) -> Option<templar_window::ActivationGrant> {
    let remaining = ACTIVATION_LIFETIME.checked_sub(age)?;
    if remaining.is_zero() {
        return None;
    }
    Some(templar_window::ActivationGrant {
        token,
        requesting_instance: None,
        target_instance: None,
        expires_at_monotonic_ns: now_monotonic_ns.saturating_add(remaining.as_nanos() as u64),
        had_recent_input_evidence: true,
    })
}

fn valid_activation_serial(serial: Serial, last_enter: Serial, upper_bound: Serial) -> bool {
    serial.is_no_older_than(&last_enter) && upper_bound.is_no_older_than(&serial)
}

impl<B: Backend + 'static> CompositorHandler for TemplarCompState<B> {
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
                if self
                    .screencopy
                    .pending
                    .iter()
                    .any(|(id, _)| self.windows.surface_for(*id) == Some(&root))
                {
                    B::request_redraw(self);
                }
            }
        }

        self.popups.commit(surface);
        ensure_initial_popup_configure(&mut self.popups, surface);
        self.handle_toplevel_commit(surface);
        crate::video_idle::refresh(self);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.video_idle.forget_surface(surface);
        self.pending_activations.remove(&surface.id());
        crate::xdg_foreign::surface_destroyed(self, surface);
        if let Some(window) = window_for_surface(self, surface) {
            self.space.unmap_elem(&window);
        }
        if let Some(window_id) = self.windows.remove(&surface.id()) {
            tracing::info!(?window_id, "toplevel surface destroyed");
            crate::space_sync::on_window_destroyed(self, window_id);
        }
        crate::video_idle::refresh(self);
    }
}

/// The `Window` wrapping root `WlSurface` `surface` — from `WindowIndex` first, so a window that
/// is currently *unmapped* from `Space` (shelved, or hidden behind a "go home" reveal;
/// `src/space_sync.rs`) keeps its commit/configure bookkeeping running while out of sight and
/// comes back with a current buffer. `Space` is only the fallback for anything the index has not
/// been told about (surface-tree bookkeeping, not placement; see the module doc).
fn window_for_surface<B: Backend + 'static>(
    state: &TemplarCompState<B>,
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
    state: &mut TemplarCompState<B>,
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
        state.reveal_home(templar_window::RevealHome::Hide);
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
    state: &mut TemplarCompState<B>,
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
    // Either direction ends a peek, for the same reason fullscreen does: a maximized window
    // covers the bottom edge too, and a peek that outlives the window that was under it would
    // have the next one open already uncovered, with no dwell ever asked for.
    state.window_service.set_peek_output(None);
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
            // An X11 window is told its whole placement in one request; the tab control is
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

/// `xdg_toplevel.set_fullscreen` / `unset_fullscreen`, from either shell and from the recovery
/// chord: [`templar_window::WindowService::set_fullscreen`] decides the rectangle (the whole output,
/// bar's zone included), and this configures the client with it.
///
/// The size is the point. Before this, fullscreen only flipped a flag and sent a bare configure,
/// so the client kept whatever size it already had and "fullscreen" was a window that had merely
/// lost its frame. The state bit is sent whatever the service decided, so a client repeating a
/// request it is already in still gets its acknowledgement.
pub fn apply_fullscreen<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    window_id: WindowId,
    fullscreen: bool,
) {
    if state.window_service.window(window_id).is_none() {
        return;
    }
    state.window_service.set_fullscreen(window_id, fullscreen);
    // Either direction ends a peek. Leaving fullscreen, because nothing is covering the bar any
    // more; *entering* it, because a peek outlives the window that was under it — closing a
    // fullscreen video with the bar peeked leaves the flag set, and the next thing to go
    // fullscreen would open already uncovered, with no dwell ever asked for.
    state.window_service.set_peek_output(None);
    let client = crate::space_sync::client_area(state, window_id);
    if let Some(window) = crate::space_sync::element_for_window(state, window_id) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|pending| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                if fullscreen {
                    pending.states.set(State::Fullscreen);
                } else {
                    pending.states.unset(State::Fullscreen);
                }
                pending.size = Some((client.width, client.height).into());
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_fullscreen(fullscreen);
            let _ = x11.configure(Some(smithay::utils::Rectangle::new(
                smithay::utils::Point::from((client.x, client.y)),
                smithay::utils::Size::from((client.width, client.height)),
            )));
        }
    }
    crate::space_sync::sync_after_change(state);
}

/// `xdg_toplevel.resize_edge` in this repository's own vocabulary
/// ([`templar_window::ResizeEdges`]). The protocol enum is a set of corner names rather than flags,
/// so this is a table and not arithmetic; anything unrecognised (the enum is `non_exhaustive`,
/// and `none` is a legal value that asks for nothing) becomes the empty set, which
/// `WindowService::begin_resize` refuses.
fn resize_edges(
    edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
) -> templar_window::ResizeEdges {
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
    templar_window::ResizeEdges {
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

impl<B: Backend + 'static> TemplarCompState<B> {
    fn apply_activation_grant(
        &mut self,
        window_id: WindowId,
        grant: templar_window::ActivationGrant,
    ) -> bool {
        let ctx = CommandContext {
            trusted_shell: true,
            now_monotonic_ns: now_monotonic_ns(&self.clock),
        };
        if self
            .window_service
            .handle_command(WindowCommand::Activate { window_id, grant }, ctx)
            .is_err()
        {
            return false;
        }
        crate::input::activate_window(self, window_id);
        true
    }

    fn apply_pending_activation(&mut self, surface: &WlSurface, window_id: WindowId) {
        let now = now_monotonic_ns(&self.clock);
        if let Some(grant) = self.pending_activations.take(&surface.id(), now) {
            self.apply_activation_grant(window_id, grant);
        }
    }

    fn unmap_existing_toplevel(&mut self, object_id: &ObjectId, window: &Window) {
        let Some(window_id) = self.windows.mark_unmapped(object_id) else {
            return;
        };
        let was_activated = self
            .window_service
            .window(window_id)
            .is_some_and(|record| record.activated);
        self.window_service.set_mapped(window_id, false);
        self.prune_switch_walk(window_id);
        self.space.unmap_elem(window);
        tracing::info!(?window_id, backend = "wayland", "toplevel unmapped");
        if was_activated {
            crate::input::refocus_after_window_hidden(self);
        } else {
            crate::space_sync::sync_after_change(self);
        }
    }

    fn remap_existing_toplevel(
        &mut self,
        surface: &WlSurface,
        object_id: &ObjectId,
        window: &Window,
    ) -> bool {
        let Some(window_id) = self.windows.window_id_for(object_id) else {
            return false;
        };
        self.window_service.set_mapped(window_id, true);
        self.windows.mark_mapped(surface.clone(), window_id);
        let client = crate::space_sync::client_area(self, window_id);
        self.space
            .map_element(window.clone(), (client.x, client.y), false);
        tracing::info!(?window_id, backend = "wayland", "toplevel remapped");
        self.apply_pending_activation(surface, window_id);
        crate::protocols_shell::apply_pending_popup_focus(self, surface);
        crate::space_sync::sync_after_change(self);
        true
    }

    /// The one place a toplevel's mapped-ness is decided (`window_bridge::should_map_toplevel`),
    /// and the one place its title is kept in sync once mapped (`xdg_toplevel.set_title`'s row in
    /// COMPOSITOR.md — Smithay delivers this as [`XdgShellHandler::title_changed`] instead, which
    /// only fires post-map, so the just-mapped title is read here directly from
    /// [`XdgToplevelSurfaceData`] instead of waiting for a redundant first change notification).
    fn handle_toplevel_commit(&mut self, surface: &WlSurface) {
        self.pending_activations
            .prune(now_monotonic_ns(&self.clock));
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
        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                s.buffer().is_some()
            })
            .unwrap_or(false);
        if self.windows.is_mapped(&object_id) {
            if !has_buffer {
                self.unmap_existing_toplevel(&object_id, &window);
                return;
            }
            self.adopt_client_size(surface, &window);
            return;
        }
        self.windows.track_pending(object_id.clone());

        if !self
            .windows
            .ready_to_map(&object_id, has_buffer, toplevel.is_initial_configure_sent())
        {
            return;
        }

        if self.remap_existing_toplevel(surface, &object_id, &window) {
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
        // The client's size, placed and clamped as committed: the client rectangle is
        // authoritative and corner tabs live outside it (or overlaid over it), so what is
        // placed is the whole of what will be on screen with no frame added. The geometry's
        // own location is the window's offset inside its surface (a client-side shadow
        // margin), not a position — an xdg client cannot ask for one — and would otherwise
        // read as a placed window that skips centring.
        let committed = templar_window::Rect::new(0, 0, geometry.size.w, geometry.size.h);
        let requested = committed;

        let window_id = self.window_service.on_toplevel_mapped(
            peer_pid,
            client_kind(trusted_shell),
            claims,
            title.unwrap_or_default(),
            requested,
        );
        self.windows.mark_mapped(surface.clone(), window_id);
        crate::protocols_shell::apply_surface_claims(self, surface, window_id, requested);
        crate::xdg_foreign::mapped(self, surface);

        let repaired = self
            .window_service
            .window(window_id)
            .map_or(requested, |record| record.geometry);
        // The client rectangle is the geometry, whole: mapped at its own origin with no
        // frame offset, for shell surfaces and client windows alike.
        let client = crate::space_sync::client_area(self, window_id);
        self.space
            .map_element(window.clone(), (client.x, client.y), false);
        // Placement may have clamped an oversized request to the output's usable area
        // (`templar_window::place_new_window`); the client only learns a size through a configure.
        if (client.width, client.height) != (geometry.size.w, geometry.size.h) {
            toplevel.with_pending_state(|pending| {
                pending.size = Some((client.width, client.height).into());
            });
            toplevel.send_configure();
        }
        // Launcher1 carries no token, so ordinary launches still use the map fallback.
        // A chooser opened over fullscreen needs its explicit grant instead: parenting is
        // presentation-only and the fallback correctly refuses to steal fullscreen focus.
        if self.window_service.should_activate_on_map(window_id) {
            crate::input::activate_window(self, window_id);
        }
        self.apply_pending_activation(surface, window_id);
        crate::protocols_shell::apply_pending_popup_focus(self, surface);
        tracing::info!(
            ?window_id,
            requested = ?requested,
            geometry = ?repaired,
            layer = ?self.window_service.layer_of(window_id),
            recoverable = templar_window::is_recoverable(repaired, self.window_service.outputs()),
            // Whether the window took the keyboard at map: the one thing the runbook check for
            // "an application opened from the search bar can be typed into" can read from a log.
            activated = self
                .window_service
                .window(window_id)
                .is_some_and(|record| record.activated),
            "toplevel mapped"
        );
        crate::space_sync::sync_after_change(self);
    }
}

/// Whether the client has acknowledged the last size the compositor sent it.
///
/// The one tell that separates "this client wants a different size" from "this client has not
/// caught up yet": a commit made before the ack still carries the size of the configure before
/// the one in flight. Adopting that is an endless round trip, one per frame, because every reply
/// the compositor sends produces another stale commit. Shared by both branches of
/// [`TemplarCompState::adopt_client_size`] so neither can be tightened without the other.
fn has_acked_last_configure(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
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
        // Nothing sent yet is nothing to catch up with.
        sent.is_none() || acked == sent
    })
}

impl<B: Backend + 'static> TemplarCompState<B> {
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
        let (maximized, fullscreen, geometry) =
            (record.maximized, record.fullscreen, record.geometry);
        // Edge-anchored shell surfaces and notifications choose their own extent, but not
        // their anchor. Reapply placement on size changes so a growing/shrinking toast
        // keeps its bottom-right edge rather than extending beyond the output.
        //
        // Only once it has acknowledged the last size it was sent, exactly as the ordinary path
        // below requires. A commit that arrives before that ack still carries the *previous*
        // configure's size, so adopting it re-places the surface at a size the client is already
        // on its way out of, configures it back, and takes the next stale commit the same way —
        // the two chase each other at frame rate, forever. That is what the bar's 80/48
        // oscillation was, and no amount of comparing the placement to the geometry can see it:
        // the placement genuinely does alternate.
        if self
            .shell_protocol
            .exclusive_zones
            .contains_key(&surface.id())
            || record.role == templar_window::WindowRole::Notification
        {
            let committed = window.geometry().size;
            if (committed.w, committed.h) != (geometry.width, geometry.height)
                && has_acked_last_configure(surface)
            {
                let requested =
                    templar_window::Rect::new(geometry.x, geometry.y, committed.w, committed.h);
                crate::protocols_shell::apply_surface_claims(self, surface, window_id, requested);
            }
            return;
        }
        if maximized
            || fullscreen
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
        if !has_acked_last_configure(surface) {
            return;
        }
        // Adopt the committed size as the client rectangle itself: no frame is added,
        // so an oversized commit re-places the same pixels the client drew.
        let geometry = templar_window::Rect::new(client.x, client.y, committed.w, committed.h);
        self.window_service.set_geometry(window_id, geometry);
        crate::space_sync::sync_after_change(self);
    }
}

impl<B: Backend + 'static> XdgShellHandler for TemplarCompState<B> {
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
        CompositorHandler::destroyed(self, surface.wl_surface());
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

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        crate::xdg_foreign::sync_parent(self, surface.wl_surface());
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
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            apply_fullscreen(self, window_id, true);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(window_id) = self.windows.window_id_for(&surface.wl_surface().id()) {
            apply_fullscreen(self, window_id, false);
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

impl<B: Backend + 'static> TemplarCompState<B> {
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
        let Some(output_geo) = crate::viewport::output_geometry(&self.space, output) else {
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

/// The decoration a client request resolves to. The compositor draws every frame — the tags
/// along each window's bottom edge are its name and its park, maximize and close controls — so
/// a client asking for client-side still gets server-side: one set of controls, the
/// compositor's, on every window. No application themes its own chrome on this system.
fn granted_mode(_requested: Mode) -> Mode {
    Mode::ServerSide
}

impl<B: Backend + 'static> XdgDecorationHandler for TemplarCompState<B> {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // The compositor draws the frame: the tags along every client window's bottom edge are
        // its name and its park, maximize and close controls (`chrome::collect_window_chrome`,
        // `chrome::layout::control_tags`), and the window is movable by its name tag and
        // resizable from its outside edges and corners (`templar_window::geometry::resize_edges_at`).
        // A client that also drew its own title bar would be showing two sets of controls, so
        // server-side is the answer, including when the client asks for client-side below.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        self.apply_decoration(&toplevel, Mode::ServerSide);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: Mode) {
        let granted = granted_mode(mode);
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(granted);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
        self.apply_decoration(&toplevel, granted);
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

impl<B: Backend + 'static> TemplarCompState<B> {
    fn apply_decoration(&mut self, toplevel: &ToplevelSurface, mode: Mode) {
        let Some(window_id) = self.windows.window_id_for(&toplevel.wl_surface().id()) else {
            return;
        };
        // Belt and braces: even a future caller passing `ClientSide` lands on server-side.
        let decoration = if granted_mode(mode) == Mode::ServerSide {
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
impl<B: Backend + 'static> XdgDialogHandler for TemplarCompState<B> {
    fn dialog_hint_changed(&mut self, toplevel: ToplevelSurface, hint: ToplevelDialogHint) {
        let Some(window_id) = self.windows.window_id_for(&toplevel.wl_surface().id()) else {
            return;
        };
        self.window_service
            .set_modal(window_id, hint == ToplevelDialogHint::Modal);
        crate::space_sync::sync_after_change(self);
    }
}

impl<B: Backend + 'static> XdgActivationHandler for TemplarCompState<B> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Validate the issuer now, while it still has focus. A portal or another process may
        // legitimately redeem the token later, with no source surface of its own.
        let Some((serial, seat_resource)) = data.serial.as_ref() else {
            return false;
        };
        if Seat::<Self>::from_resource(seat_resource).as_ref() != Some(&self.seat) {
            return false;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let Some(focused_client) = keyboard
            .current_focus()
            .and_then(|surface| surface.client())
        else {
            return false;
        };
        if data.client_id.as_ref() != Some(&focused_client.id()) {
            return false;
        }
        if let Some(source) = data.surface.as_ref()
            && source.client().as_ref().map(Client::id).as_ref() != data.client_id.as_ref()
        {
            return false;
        }
        keyboard.last_enter().is_some_and(|last_enter| {
            valid_activation_serial(*serial, last_enter, SERIAL_COUNTER.next_serial())
        })
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Smithay only dispatches known tokens, but keeps them in its pool until we remove
        // them. Consume before either immediate activation or queueing so a replay cannot
        // redirect queued authority to a different surface, even after destruction/expiry.
        if !self.xdg_activation_state.remove_token(&token) {
            return;
        }
        let now = now_monotonic_ns(&self.clock);
        self.pending_activations.prune(now);
        let Some(grant) = activation_grant(token.into(), token_data.timestamp.elapsed(), now)
        else {
            return;
        };
        if !surface.is_alive()
            || !self
                .windows
                .element_for(&surface.id())
                .is_some_and(|window| window.toplevel().is_some())
        {
            return;
        }
        if self.windows.is_mapped(&surface.id()) {
            if let Some(window_id) = self.windows.window_id_for(&surface.id())
                && self.apply_activation_grant(window_id, grant)
            {
                crate::space_sync::sync_after_change(self);
            }
        } else {
            self.pending_activations.insert(surface.id(), grant, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as Wire;
    use super::{ACTIVATION_LIFETIME, MAX_PENDING_ACTIVATIONS, PendingActivations, activation_grant};
    use templar_window::{ClientKind, CommandContext, Output, Rect, WindowClaims, WindowCommand, WindowId, WindowService};

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

    fn fullscreen_browser() -> (WindowService<NullCache>, WindowId) {
        let mut service = WindowService::new(
            NullCache,
            vec![Output::new("test", Rect::new(0, 0, 1920, 1080), 1000)],
        );
        let browser = map_test_window(&mut service, "browser");
        service.activate(browser);
        service.set_fullscreen(browser, true);
        (service, browser)
    }

    fn map_test_window(service: &mut WindowService<NullCache>, title: &str) -> WindowId {
        service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            title.into(),
            Rect::new(0, 0, 400, 300),
        )
    }

    fn grant(token: &str, now: u64) -> templar_window::ActivationGrant {
        activation_grant(token.into(), std::time::Duration::ZERO, now).unwrap()
    }

    #[test]
    fn pending_chooser_grant_overrides_fullscreen_map_refusal_once() {
        let (mut service, browser) = fullscreen_browser();
        let mut pending = PendingActivations::default();
        pending.insert(7, grant("chooser-input", 0), 0);
        let chooser = map_test_window(&mut service, "chooser");
        assert!(!service.should_activate_on_map(chooser));
        assert!(service.window(browser).unwrap().activated);
        let grant = pending.take(&7, 1).unwrap();
        service
            .handle_command(
                WindowCommand::Activate {
                    window_id: chooser,
                    grant: grant.clone(),
                },
                CommandContext {
                    trusted_shell: true,
                    now_monotonic_ns: 1,
                },
            )
            .unwrap();
        assert!(service.window(chooser).unwrap().activated);
        assert!(!service.window(browser).unwrap().activated);
        assert!(pending.take(&7, 1).is_none());

        // Neither a second map nor a replay of the retained token may take focus again.
        service.activate(browser);
        let replay = service.handle_command(
            WindowCommand::Activate {
                window_id: chooser,
                grant,
            },
            CommandContext {
                trusted_shell: true,
                now_monotonic_ns: 2,
            },
        );
        assert_eq!(replay, Err(templar_window::CommandError::GrantAlreadyUsed));
        assert!(service.window(browser).unwrap().activated);
        assert!(!service.should_activate_on_map(chooser));
    }

    #[test]
    fn expired_untrusted_and_destroyed_pending_choosers_do_not_interrupt_fullscreen() {
        for invalidation in ["expired", "untrusted", "destroyed"] {
            let (mut service, browser) = fullscreen_browser();
            let mut pending = PendingActivations::default();
            let mut request = grant(invalidation, 0);
            if invalidation == "untrusted" {
                request.had_recent_input_evidence = false;
            }
            let expiry = request.expires_at_monotonic_ns;
            pending.insert(7, request, 0);
            if invalidation == "destroyed" {
                pending.remove(&7);
            }
            let chooser = map_test_window(&mut service, "chooser");
            let now = if invalidation == "expired" { expiry } else { 1 };
            assert!(pending.take(&7, now).is_none(), "{invalidation}");
            assert!(!service.should_activate_on_map(chooser));
            assert!(service.window(browser).unwrap().activated);
            assert!(!service.window(chooser).unwrap().activated);
        }
    }

    #[test]
    fn activation_deadline_is_anchored_to_mint_not_map_or_redemption() {
        let now = 10_000_000_000;
        let age = std::time::Duration::from_secs(4);
        let request = activation_grant("recent".into(), age, now).unwrap();
        assert_eq!(request.expires_at_monotonic_ns, now + 1_000_000_000);
        let mut pending = PendingActivations::default();
        pending.insert(7, request, now);
        assert!(pending.take(&7, now + 1_000_000_000).is_none());
        assert!(activation_grant("expired".into(), ACTIVATION_LIFETIME, now).is_none());
        assert!(activation_grant("older".into(), ACTIVATION_LIFETIME + age, now).is_none());
    }

    #[test]
    fn pending_capacity_does_not_evict_valid_authority_and_reclaims_expiry() {
        let mut pending = PendingActivations::default();
        for surface in 0..MAX_PENDING_ACTIVATIONS {
            pending.insert(surface, grant(&format!("token-{surface}"), 0), 0);
        }
        pending.insert(MAX_PENDING_ACTIVATIONS, grant("overflow", 0), 0);
        assert!(pending.take(&MAX_PENDING_ACTIVATIONS, 1).is_none());
        assert!(pending.take(&0, 1).is_some());
        pending.insert(MAX_PENDING_ACTIVATIONS, grant("replacement", 1), 1);
        assert!(pending.take(&MAX_PENDING_ACTIVATIONS, 2).is_some());
        let expiry = ACTIVATION_LIFETIME.as_nanos() as u64;
        pending.insert(
            MAX_PENDING_ACTIVATIONS + 1,
            grant("after-expiry", expiry),
            expiry,
        );
        assert_eq!(pending.entries.len(), 1);
        assert!(
            pending
                .take(&(MAX_PENDING_ACTIVATIONS + 1), expiry)
                .is_some()
        );
    }

    #[test]
    fn one_surface_retains_only_its_latest_requested_grant() {
        let mut pending = PendingActivations::default();
        pending.insert(7, grant("first", 0), 0);
        pending.insert(7, grant("second", 1), 1);
        assert_eq!(pending.take(&7, 2).unwrap().token, "second");
        assert!(pending.take(&7, 2).is_none());
    }

    #[test]
    fn activation_serial_must_be_within_the_focused_issuers_serial_interval() {
        let valid = |serial: u32, enter: u32, upper: u32| {
            super::valid_activation_serial(serial.into(), enter.into(), upper.into())
        };
        assert!(valid(12u32, 10u32, 15u32));
        assert!(valid(10, 10, 15));
        assert!(!valid(9, 10, 15));
        assert!(!valid(16, 10, 15));
        assert!(valid(1, u32::MAX - 1, 3));
        assert!(!valid(u32::MAX - 2, u32::MAX - 1, 3));
        assert!(!valid(4, u32::MAX - 1, 3));
    }

    /// The compositor draws every frame, so decoration negotiation is not a negotiation: a
    /// client asking for client-side still gets server-side, and no application themes its
    /// own chrome. (`request_mode` and `apply_decoration` both resolve through this.)
    #[test]
    fn client_side_requests_are_granted_server_side() {
        assert_eq!(super::granted_mode(Mode::ClientSide), Mode::ServerSide);
        assert_eq!(super::granted_mode(Mode::ServerSide), Mode::ServerSide);
    }

    /// A client asking for a resize means what the protocol says it means. The compositor's own
    /// perimeter turns every press into a corner (`templar_window::resize_edges_at`), but that rule
    /// belongs to the compositor's affordance and must not leak into `xdg_toplevel.resize`: a
    /// client that asks to drag one edge gets one edge.
    #[test]
    fn a_client_requested_resize_keeps_the_edges_the_protocol_named() {
        for (wire, expected) in [
            (Wire::Left, (true, false, false, false)),
            (Wire::Right, (false, true, false, false)),
            (Wire::Top, (false, false, true, false)),
            (Wire::Bottom, (false, false, false, true)),
        ] {
            let edges = super::resize_edges(wire);
            assert_eq!(
                (edges.left, edges.right, edges.top, edges.bottom),
                expected,
                "{wire:?} was widened to a corner"
            );
        }
        // A corner request is still a corner, and `none` still asks for nothing —
        // `WindowService::begin_resize` refuses the empty set.
        let corner = super::resize_edges(Wire::BottomLeft);
        assert_eq!(
            (corner.left, corner.right, corner.top, corner.bottom),
            (true, false, false, true)
        );
        assert!(super::resize_edges(Wire::None).is_empty());
    }
}
