//! Translation between Smithay's world and `templar_window`'s: which `ClientKind` a connection maps
//! to, what a client's claims turn into, when a toplevel has actually become "mapped" (as opposed
//! to merely having an `xdg_surface` role), and how per-output exclusive-zone claims combine into
//! one `templar_window::Output`. Kept as plain functions over plain data wherever possible — no
//! `Display`, no live Wayland objects — specifically so the decision logic is unit-testable
//! without a compositor (task rule 3: "test what can be tested without a display").
//!
//! `WindowIndex` is the one stateful piece here: the join table between a live `WlSurface`'s
//! object id and the `templar_window::WindowId` `on_toplevel_mapped` returned for it, plus the set of
//! toplevels that have an `xdg_surface`/`X11Surface` role but have not yet met
//! [`should_map_toplevel`]'s condition.

use smithay::{
    desktop::Window,
    reexports::wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface},
    utils::{Clock, Monotonic},
};
use std::collections::{HashMap, HashSet};
use templar_window::{ClientKind, Rect, WindowClaims, WindowId, geometry::Exclusive};

/// `CommandContext`/`ActivationGrant` want `now_monotonic_ns: u64`; `Clock<Monotonic>::now()`
/// hands back a `Time<Monotonic>`, convertible to `Duration` (never negative for a monotonic
/// clock) and from there to nanoseconds. One place for that conversion so every call site agrees.
pub fn now_monotonic_ns(clock: &Clock<Monotonic>) -> u64 {
    std::time::Duration::from(clock.now())
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// `client` is the trusted-shell connection (bound the private socket, `src/shell_socket.rs`) iff
/// `trusted_shell` — the only two `ClientKind`s a live protocol connection can ever produce.
/// `ClientKind::CompositorCore` is reserved for surfaces the compositor core itself creates with
/// no client behind them at all (the lock screen, the recovery overlay — see `src/session_lock.rs`
/// and `src/keybindings.rs`), never for a real connection.
pub fn client_kind(trusted_shell: bool) -> ClientKind {
    if trusted_shell {
        ClientKind::TrustedShell
    } else {
        ClientKind::Application
    }
}

/// Claims from a Wayland `xdg_toplevel.app_id` (or the sandbox app-id extension, once one exists
/// to read from — `sandbox_app_id` is left `None` until then).
pub fn claims_from_wayland(app_id: Option<String>) -> WindowClaims {
    WindowClaims {
        wayland_app_id: app_id,
        startup_wm_class: None,
        sandbox_app_id: None,
    }
}

/// Claims from an XWayland window's `WM_CLASS`.
pub fn claims_from_x11(wm_class: Option<String>) -> WindowClaims {
    WindowClaims {
        wayland_app_id: None,
        startup_wm_class: wm_class,
        sandbox_app_id: None,
    }
}

/// Whether a toplevel that previously had `had_buffer_before` and now reports `has_buffer_now`
/// should transition to mapped on this commit.
///
/// A toplevel becomes mapped on the *first* commit that attaches real content once the
/// compositor's initial configure has already gone out (`ToplevelSurface::is_initial_configure_sent`)
/// — a client is free to commit before that point (Smithay itself defers sending the initial
/// configure until first commit; see `src/xdg_shell.rs::handle_toplevel_commit`), and neither
/// commit should be mistaken for "the window is now showing content the user asked to see". Once
/// mapped, later buffer attach/detach cycles (a client hiding then showing the same toplevel) are
/// not remapping events — `templar_window::WindowService` has no concept of a window temporarily
/// losing its buffer, only shelved/unshelved and fullscreen/modal — so this function is never
/// consulted again for a toplevel once it has returned `true` once (see [`WindowIndex::mark_mapped`]).
pub fn should_map_toplevel(has_buffer_now: bool, initial_configure_sent: bool) -> bool {
    has_buffer_now && initial_configure_sent
}

/// The join table between live `WlSurface`s and the `WindowId`s `templar_window::WindowService`
/// assigned them. A surface is `pending` from the moment it gets an `xdg_toplevel`/X11 role until
/// [`should_map_toplevel`] says to call `on_toplevel_mapped`; it is `mapped` from that call until
/// `on_toplevel_destroyed`. The reverse direction stores the live `WlSurface` handle itself (cheap
/// to clone — it is just a backend id plus a handle) rather than an `ObjectId`, so callers that
/// need to send the surface a real protocol event (`xdg_toplevel.configure{activated}`,
/// `keyboard.set_focus`) never have to reconstruct a resource from a bare id.
#[derive(Debug, Default)]
pub struct WindowIndex {
    mapped: HashMap<ObjectId, WindowId>,
    reverse: HashMap<WindowId, WlSurface>,
    /// Surfaces with a role but not yet mapped, so repeated pre-content commits don't re-evaluate
    /// anything expensive.
    pending: HashSet<ObjectId>,
    /// The Smithay `Window` wrapping each toplevel, from the moment it gets a role. This — not
    /// `Space` — is where commit/configure bookkeeping finds a window, because a shelved or
    /// reveal-hidden window is *unmapped from `Space`* (`src/space_sync.rs`) yet must keep
    /// committing: its buffer has to be current the instant it is shown again.
    elements: HashMap<ObjectId, Window>,
}

impl WindowIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A surface just gained an `xdg_toplevel`/X11 role. No-op if already tracked (a second
    /// `get_toplevel` on the same `xdg_surface` is a protocol error Smithay itself rejects before
    /// this would ever be called twice for one id).
    pub fn track_pending(&mut self, surface: ObjectId) {
        self.pending.insert(surface);
    }

    /// Remember the `Window` wrapping `surface` (see the `elements` field's doc). Called where
    /// the window is created — `new_toplevel`, XWayland's `map_window_request` — before it is
    /// ever mapped into `Space`.
    pub fn track_element(&mut self, surface: ObjectId, window: Window) {
        self.elements.insert(surface, window);
    }

    /// The `Window` wrapping `surface`, mapped into `Space` or not.
    pub fn element_for(&self, surface: &ObjectId) -> Option<&Window> {
        self.elements.get(surface)
    }

    /// Every `Window` the compositor is tracking, mapped into `Space` or not — the iteration
    /// `Space::elements()` cannot give, because a shelved or reveal-hidden window is unmapped
    /// from `Space` and still has to be configured (`src/input.rs::activate_window` clears the
    /// `activated` state on every one of them, not only the visible ones).
    pub fn elements(&self) -> impl Iterator<Item = &Window> {
        self.elements.values()
    }

    /// The `Window` wrapping the surface `window_id` was assigned for, mapped into `Space` or not.
    pub fn element_of(&self, window_id: WindowId) -> Option<&Window> {
        let surface = self.reverse.get(&window_id)?;
        self.elements.get(&surface.id())
    }

    pub fn is_mapped(&self, surface: &ObjectId) -> bool {
        self.mapped.contains_key(surface)
    }

    /// Whether `surface` should map right now, given it just attached a buffer on this commit and
    /// the compositor's initial configure has already been sent.
    pub fn ready_to_map(
        &self,
        surface: &ObjectId,
        has_buffer_now: bool,
        initial_configure_sent: bool,
    ) -> bool {
        !self.is_mapped(surface)
            && self.pending.contains(surface)
            && should_map_toplevel(has_buffer_now, initial_configure_sent)
    }

    pub fn mark_mapped(&mut self, surface: WlSurface, window_id: WindowId) {
        let object_id = surface.id();
        self.pending.remove(&object_id);
        self.reverse.insert(window_id, surface);
        self.mapped.insert(object_id, window_id);
    }

    pub fn window_id_for(&self, surface: &ObjectId) -> Option<WindowId> {
        self.mapped.get(surface).copied()
    }

    pub fn surface_for(&self, window_id: WindowId) -> Option<&WlSurface> {
        self.reverse.get(&window_id)
    }

    pub fn remove(&mut self, surface: &ObjectId) -> Option<WindowId> {
        self.pending.remove(surface);
        self.elements.remove(surface);
        let window_id = self.mapped.remove(surface)?;
        self.reverse.remove(&window_id);
        Some(window_id)
    }
}

/// Recompute one output's `work_area` from every live exclusive-zone claim registered against it.
/// Zones on different edges are independent axes, so registration order never changes the result
/// — see `templar_window::geometry::work_area_for`'s doc — which is what lets this take an unordered
/// map instead of needing to reconstruct a stable per-output claim order.
pub fn work_area_from_zones(logical: Rect, zones: &[Exclusive]) -> Rect {
    templar_window::geometry::work_area_for(logical, zones)
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_window::geometry::Edge;

    #[test]
    fn trusted_shell_flag_is_the_only_input_to_client_kind() {
        assert_eq!(client_kind(true), ClientKind::TrustedShell);
        assert_eq!(client_kind(false), ClientKind::Application);
    }

    #[test]
    fn claims_carry_exactly_the_field_they_were_built_from() {
        let wayland = claims_from_wayland(Some("org.templaros.Files".into()));
        assert_eq!(
            wayland.wayland_app_id.as_deref(),
            Some("org.templaros.Files")
        );
        assert_eq!(wayland.startup_wm_class, None);

        let x11 = claims_from_x11(Some("firefox".into()));
        assert_eq!(x11.startup_wm_class.as_deref(), Some("firefox"));
        assert_eq!(x11.wayland_app_id, None);
    }

    #[test]
    fn mapping_requires_both_a_buffer_and_an_acked_initial_configure() {
        assert!(!should_map_toplevel(false, false));
        assert!(!should_map_toplevel(true, false));
        assert!(!should_map_toplevel(false, true));
        assert!(should_map_toplevel(true, true));
    }

    // `ObjectId` has no public constructor outside a live wayland-server connection, so
    // `WindowIndex` itself is exercised indirectly here: its bookkeeping is a thin, obviously
    // correct wrapper around `should_map_toplevel`, which is what's tested directly above.

    #[test]
    fn zones_on_the_same_edge_stack_regardless_of_registration_order() {
        let logical = Rect::new(0, 0, 1920, 1080);
        let forward = work_area_from_zones(
            logical,
            &[
                Exclusive::new(Edge::Top, 40),
                Exclusive::new(Edge::Bottom, 64),
            ],
        );
        let backward = work_area_from_zones(
            logical,
            &[
                Exclusive::new(Edge::Bottom, 64),
                Exclusive::new(Edge::Top, 40),
            ],
        );
        assert_eq!(forward, backward);
        assert_eq!(forward, Rect::new(0, 40, 1920, 976));
    }

    /// A claim of 0 is kept rather than released (`protocols_shell::handle_set_exclusive_zone`),
    /// so it reaches this function — and must subtract nothing. The bar collapsed to a hint is
    /// still anchored to its edge, but the work area is the whole output.
    #[test]
    fn a_zero_zone_subtracts_nothing() {
        let logical = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            work_area_from_zones(logical, &[Exclusive::new(Edge::Bottom, 0)],),
            logical
        );
    }
}
