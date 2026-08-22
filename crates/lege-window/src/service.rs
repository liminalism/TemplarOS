//! Window records, identity cache boundary, drag handling, layer membership, the Window Shelf
//! and the fullscreen/modal escape route.

use crate::{
    geometry::{
        Output, Point, RecoveryAffordance, Rect, choose_target_output, recovery_affordance,
        repair_window,
    },
    layers::{ClientKind, Layer, LayerError, LayerStack, StackPosition},
    shell::ShellToplevel,
};
use lege_types::{AppId, InstanceId, WindowClaims};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct WindowId(pub u64);

/// What the compositor currently believes about a window's owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIdentity {
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub verified: bool,
    pub claim_mismatch: bool,
}

impl CachedIdentity {
    pub const UNKNOWN: Self = Self {
        app_id: None,
        instance_id: None,
        verified: false,
        claim_mismatch: false,
    };
}

/// Boundary between the render thread and `appd`. Implemented in `lege-comp-integration`.
pub trait IdentityCache: Send + Sync {
    /// Must not perform blocking D-Bus I/O on the compositor render thread.
    fn resolve_cached(&self, pid: u32, claims: &WindowClaims) -> Option<CachedIdentity>;

    /// Schedule an asynchronous refresh from `appd`; the answer arrives through
    /// [`WindowService::update_identity`].
    fn request_resolution(&self, window_id: WindowId, pid: u32, claims: WindowClaims);
}

/// Who draws a window's frame. Every window keeps a compositor-owned recovery band regardless,
/// but client-side-decorated and borderless windows need the compositor to actively paint a
/// temporary tab over it, because the client controls that screen area
/// ([`recovery_affordance`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Decoration {
    /// The compositor draws the title bar and border.
    ServerSide,
    /// The client draws its own frame.
    ClientSide,
    /// No frame at all (fullscreen media, games).
    Borderless,
}

impl Decoration {
    /// Whether this decoration mode needs the compositor's temporary recovery tab.
    pub fn needs_compositor_recovery_tab(self) -> bool {
        matches!(self, Self::ClientSide | Self::Borderless)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRecord {
    pub id: WindowId,
    pub peer_pid: u32,
    pub claims: WindowClaims,
    pub identity: CachedIdentity,
    pub title: String,
    pub geometry: Rect,
    pub activated: bool,
    pub fullscreen: bool,
    /// Holds an exclusive input grab (a dialog, a permission prompt). See
    /// [`WindowService::escape`].
    pub modal: bool,
    pub decoration: Decoration,
    /// Explicit minimisation: the window goes to the visible Window Shelf, never vanishes.
    pub shelved: bool,
    /// Set when `shelved`; orders the shelf most-recently-shelved first.
    shelf_rank: Option<u64>,
    /// Filling its output's work area (`xdg_toplevel.set_maximized`). Geometry is re-derived
    /// from the output on every topology change while this holds, so a maximized window follows
    /// its output's usable area rather than being repaired as if it were floating.
    pub maximized: bool,
    /// The geometry to return to on unmaximize — what the window had before
    /// [`WindowService::set_maximized`] grew it.
    pub restore_geometry: Option<Rect>,
}

/// Why one command was refused by the compositor before it could touch [`WindowService`] state.
/// A compositor-level escape from a fullscreen or modal window (planning doc 01 §3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EscapeAction {
    ExitedFullscreen { window_id: WindowId },
    DismissedModalGrab { window_id: WindowId },
}

#[derive(Clone, Copy, Debug)]
struct DragState {
    window_id: WindowId,
    anchor_in_window: Point,
}

/// The single-scene window service.
#[derive(Debug)]
pub struct WindowService<I> {
    identity: I,
    outputs: Vec<Output>,
    windows: HashMap<WindowId, WindowRecord>,
    next_window_id: u64,
    next_shelf_rank: u64,
    drag: Option<DragState>,
    layers: LayerStack<WindowId>,
    /// Activation-grant tokens already consumed by a successful `Activate`, keyed to their
    /// expiry so they can be pruned once they could not be replayed anyway. See
    /// `try_spend_activation_token`.
    spent_activation_tokens: HashMap<String, u64>,
    /// "Go home" (`WindowCommand::RevealHome`): while set, every window on `Layer::Normal` /
    /// `Layer::AppOnTop` is hidden so the shell's `Layer::HomeSurface` backdrop shows through.
    /// A *view* flag only — no record's geometry, `shelved` or `activated` changes because of it,
    /// and the governor is never told about it — so a shell that dies mid-reveal, or a stray
    /// press, can never leave a window anywhere but exactly where it was.
    home_revealed: bool,
}

impl<I: IdentityCache> WindowService<I> {
    pub fn new(identity: I, outputs: Vec<Output>) -> Self {
        Self {
            identity,
            outputs,
            windows: HashMap::new(),
            next_window_id: 1,
            next_shelf_rank: 0,
            drag: None,
            layers: LayerStack::new(),
            spent_activation_tokens: HashMap::new(),
            home_revealed: false,
        }
    }

    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    /// A new toplevel appeared. Geometry is validated against the current topology rather than
    /// trusted blindly — this is also the path a restored session window takes: it maps with
    /// its saved geometry as `requested_geometry`, so a session saved under a wider or
    /// differently-scaled topology still lands somewhere recoverable. Identity is resolved from
    /// cache and refreshed asynchronously.
    ///
    /// Every toplevel lands on [`Layer::Normal`] regardless of `client` — an ordinary window
    /// (an about box, a file-conflict dialog) mapped by the trusted shell or, in principle, the
    /// compositor core itself is still just a normal window, not shell chrome. `client` is
    /// still threaded through (rather than assuming [`ClientKind::Application`]) because
    /// `Layer::Normal`'s minimum trust is `Application` — the least any [`ClientKind`] can hold
    /// — so this call is authorized for whichever `client` actually mapped the window, honestly,
    /// not because the caller is lied to about who it was. See [`may_request_layer`] for why
    /// that also means this assignment cannot fail today.
    ///
    /// [`may_request_layer`]: crate::layers::may_request_layer
    pub fn on_toplevel_mapped(
        &mut self,
        peer_pid: u32,
        client: ClientKind,
        claims: WindowClaims,
        title: String,
        requested_geometry: Rect,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self.next_window_id.saturating_add(1);

        let identity = self
            .identity
            .resolve_cached(peer_pid, &claims)
            .unwrap_or(CachedIdentity::UNKNOWN);
        let geometry = repair_window(requested_geometry, &self.outputs);

        self.windows.insert(
            id,
            WindowRecord {
                id,
                peer_pid,
                claims: claims.clone(),
                identity,
                title,
                geometry,
                activated: false,
                fullscreen: false,
                modal: false,
                decoration: Decoration::ServerSide,
                shelved: false,
                shelf_rank: None,
                maximized: false,
                restore_geometry: None,
            },
        );
        // A window the user just opened must be seen: a reveal that hid everything ends the
        // moment something new arrives, rather than hiding the new window too.
        self.home_revealed = false;

        // `Layer::Normal` is reachable by every `ClientKind` (its minimum trust is the lowest
        // there is), so this cannot fail today — but it must not be able to abort the render
        // thread if the authorization table is ever revised. A window that failed to land on a
        // layer is not lost: `layer_of` reports `None` for it and it is simply absent from
        // `stacking_order` until something raises it onto a layer explicitly.
        if let Err(error) = self.layers.assign(id, Layer::Normal, client) {
            debug_assert!(
                false,
                "Layer::Normal should be reachable by every ClientKind: {error}"
            );
        }

        self.identity.request_resolution(id, peer_pid, claims);
        id
    }

    pub fn update_identity(&mut self, window_id: WindowId, identity: CachedIdentity) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.identity = identity;
        }
    }

    pub fn set_title(&mut self, window_id: WindowId, title: String) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.title = title;
        }
    }

    pub fn set_fullscreen(&mut self, window_id: WindowId, fullscreen: bool) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.fullscreen = fullscreen;
        }
    }

    pub fn set_modal(&mut self, window_id: WindowId, modal: bool) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.modal = modal;
        }
    }

    pub fn set_decoration(&mut self, window_id: WindowId, decoration: Decoration) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.decoration = decoration;
        }
    }

    /// Force `window_id`'s geometry directly (no repair pass). Callers that already computed a
    /// validated rectangle — `MoveToVisibleArea`, session restore — use this to apply it.
    pub fn set_geometry(&mut self, window_id: WindowId, geometry: Rect) -> bool {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.geometry = geometry;
            true
        } else {
            false
        }
    }

    pub fn on_toplevel_destroyed(&mut self, window_id: WindowId) {
        self.windows.remove(&window_id);
        self.layers.remove(window_id);
        if self.drag.is_some_and(|drag| drag.window_id == window_id) {
            self.drag = None;
        }
    }

    /// Outputs changed (hotplug, scale, resume). Every window is re-validated except the one
    /// currently being dragged: repairing it here would fight the user's pointer and be
    /// overwritten by the very next `update_drag` anyway, so that invariant is instead enforced
    /// once, in `finish_drag`. The returned list is what must be re-configured on the clients.
    pub fn on_outputs_changed(&mut self, outputs: Vec<Output>) -> Vec<(WindowId, Rect)> {
        self.outputs = outputs;
        let dragging = self.drag.map(|drag| drag.window_id);

        let mut changed = Vec::new();
        for window in self.windows.values_mut() {
            if Some(window.id) == dragging {
                continue;
            }
            let repaired = if window.maximized {
                // A maximized window is not floating: it fills whichever output is now closest
                // to where it was, rather than being nudged until its band is visible.
                maximized_geometry(window.geometry, &self.outputs)
            } else {
                repair_window(window.geometry, &self.outputs)
            };
            if repaired != window.geometry {
                window.geometry = repaired;
                changed.push((window.id, repaired));
            }
        }
        changed
    }

    /// `xdg_toplevel.set_maximized` / `unset_maximized`: fill the usable area of the window's
    /// output, remembering the floating geometry to return to. Returns the geometry to
    /// configure, or `None` for an unknown window or a no-op (already in the requested state).
    /// Both results are recoverable by construction: an output's usable area always has room for
    /// a recovery band (`Output::usable`), and the restore path goes through `repair_window`.
    pub fn set_maximized(&mut self, window_id: WindowId, maximized: bool) -> Option<Rect> {
        let outputs = &self.outputs;
        let window = self.windows.get_mut(&window_id)?;
        if window.maximized == maximized {
            return None;
        }
        window.maximized = maximized;
        if maximized {
            window.restore_geometry = Some(window.geometry);
            window.geometry = maximized_geometry(window.geometry, outputs);
        } else {
            let restore = window.restore_geometry.take().unwrap_or(window.geometry);
            window.geometry = repair_window(restore, outputs);
        }
        Some(window.geometry)
    }

    /// Whether the compositor should render and hit-test `window_id` at all: not shelved, and
    /// not a client window hidden behind a "go home" reveal. Shell surfaces (`Layer::HomeSurface`
    /// and the overlay layers) are never hidden by a reveal — they are what it reveals. The one
    /// predicate `space_sync`, the hit-test and the chrome all consult, so the three cannot
    /// disagree about what is on screen. Unknown windows are not visible.
    pub fn is_visible(&self, window_id: WindowId) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if window.shelved {
            return false;
        }
        if self.home_revealed {
            return !matches!(
                self.layer_of(window_id),
                Some(Layer::Normal | Layer::AppOnTop)
            );
        }
        true
    }

    /// Whether a "go home" reveal is in effect (`WindowCommand::RevealHome`).
    pub fn home_revealed(&self) -> bool {
        self.home_revealed
    }

    /// Start or end a "go home" reveal. Returns whether anything changed. Purely a view flag —
    /// see the field's doc: nothing about any window is touched.
    pub fn set_home_revealed(&mut self, revealed: bool) -> bool {
        let changed = self.home_revealed != revealed;
        self.home_revealed = revealed;
        changed
    }

    pub fn begin_drag(&mut self, window_id: WindowId, pointer_global: Point) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        self.drag = Some(DragState {
            window_id,
            anchor_in_window: Point {
                x: pointer_global.x.saturating_sub(window.geometry.x),
                y: pointer_global.y.saturating_sub(window.geometry.y),
            },
        });
        true
    }

    /// Pointer and window geometry remain in one global logical coordinate space. The anchor is
    /// never recalculated when crossing an output with a different scale factor — `scale_milli`
    /// is never consulted here (see `geometry::logical_to_physical`).
    pub fn update_drag(&mut self, pointer_global: Point) -> Option<(WindowId, Rect)> {
        let drag = self.drag?;
        let window = self.windows.get_mut(&drag.window_id)?;
        window.geometry.x = pointer_global.x.saturating_sub(drag.anchor_in_window.x);
        window.geometry.y = pointer_global.y.saturating_sub(drag.anchor_in_window.y);
        Some((window.id, window.geometry))
    }

    /// Drag ended: the invariant is enforced once, at the end.
    pub fn finish_drag(&mut self) -> Option<(WindowId, Rect)> {
        let drag = self.drag.take()?;
        let window = self.windows.get_mut(&drag.window_id)?;
        window.geometry = repair_window(window.geometry, &self.outputs);
        Some((window.id, window.geometry))
    }

    pub fn is_dragging(&self, window_id: WindowId) -> bool {
        self.drag.is_some_and(|drag| drag.window_id == window_id)
    }

    /// Move `window_id` to the visible Window Shelf: a hidden window cannot sensibly hold
    /// keyboard focus, so this also clears `activated` (the compositor should pick a new focus
    /// target the same way it would for any other deactivation). Returns `false` if the window
    /// does not exist.
    pub fn shelve(&mut self, window_id: WindowId) -> bool {
        let rank = self.next_shelf_rank;
        self.next_shelf_rank = self.next_shelf_rank.saturating_add(1);
        let Some(window) = self.windows.get_mut(&window_id) else {
            return false;
        };
        window.shelved = true;
        window.shelf_rank = Some(rank);
        window.activated = false;
        true
    }

    /// Restore `window_id` from the shelf. Its geometry is re-validated against the *current*
    /// topology before returning — it may have sat shelved through output changes, and while
    /// `on_outputs_changed` already repairs shelved windows too, this makes the invariant
    /// explicit at the one moment the window is about to become visible again. Returns the
    /// (possibly repaired) geometry, or `None` if the window does not exist or was not shelved.
    pub fn unshelve(&mut self, window_id: WindowId) -> Option<Rect> {
        let saved = self.windows.get(&window_id)?.geometry;
        let repaired = repair_window(saved, &self.outputs);
        let window = self.windows.get_mut(&window_id)?;
        if !window.shelved {
            return None;
        }
        window.shelved = false;
        window.shelf_rank = None;
        window.geometry = repaired;
        Some(repaired)
    }

    /// Shelved windows, most-recently-shelved first — the order the Window Shelf / Recent
    /// drawer renders them in.
    pub fn shelf(&self) -> Vec<ShellToplevel> {
        let mut shelved: Vec<&WindowRecord> = self.windows.values().filter(|w| w.shelved).collect();
        shelved.sort_by_key(|window| std::cmp::Reverse(window.shelf_rank.unwrap_or(0)));
        shelved
            .into_iter()
            .map(|window| self.to_shell_toplevel(window))
            .collect()
    }

    /// Place `window_id` on `layer`. Fails if `client` is not permitted to request `layer`
    /// ([`crate::layers::may_request_layer`]).
    pub fn set_layer(
        &mut self,
        window_id: WindowId,
        layer: Layer,
        client: ClientKind,
    ) -> Result<StackPosition, LayerError> {
        self.layers.assign(window_id, layer, client)
    }

    pub fn layer_of(&self, window_id: WindowId) -> Option<Layer> {
        self.layers
            .position(window_id)
            .map(|position| position.layer)
    }

    /// Raise `window_id` to the top of its current layer.
    pub fn raise(&mut self, window_id: WindowId) -> Option<StackPosition> {
        self.layers.raise(window_id)
    }

    /// Every window, top-most first.
    pub fn stacking_order(&self) -> Vec<WindowId> {
        self.layers.order()
    }

    /// One sentence explaining why `a` is stacked above or below `b`.
    pub fn explain_stacking(&self, a: WindowId, b: WindowId) -> Option<String> {
        self.layers.explain(a, b)
    }

    /// Give `window_id` keyboard focus: clears `activated` on every other window and raises it
    /// to the top of its layer. Returns `false` if the window does not exist.
    pub fn activate(&mut self, window_id: WindowId) -> bool {
        if !self.windows.contains_key(&window_id) {
            return false;
        }
        for window in self.windows.values_mut() {
            window.activated = window.id == window_id;
        }
        self.layers.raise(window_id);
        // Activating a window (a click on its tile from the Home Field, say) is the user asking
        // for it: a reveal that was hiding it ends here, for free, with nothing else to undo.
        self.home_revealed = false;
        true
    }

    /// The compositor-level way out of a fullscreen or modal window (planning doc 01 §3),
    /// reachable by a fixed key combination even with no shell (`docs/architecture/
    /// accessibility.md`). `window_id` of `None` targets the currently activated window.
    /// Fullscreen is tried first; a window can be both, and exiting fullscreen alone is usually
    /// enough to reach the recovery band.
    pub fn escape(&mut self, window_id: Option<WindowId>) -> Option<EscapeAction> {
        let target = window_id.or_else(|| {
            self.windows
                .values()
                .find(|window| window.activated && (window.fullscreen || window.modal))
                .map(|window| window.id)
        })?;
        let window = self.windows.get_mut(&target)?;
        if window.fullscreen {
            window.fullscreen = false;
            Some(EscapeAction::ExitedFullscreen { window_id: target })
        } else if window.modal {
            window.modal = false;
            Some(EscapeAction::DismissedModalGrab { window_id: target })
        } else {
            None
        }
    }

    /// Try to consume `token` for an `Activate` (see `commands::ActivationGrant`). Returns
    /// `false`, and consumes nothing, if `token` was already spent and has not yet expired —
    /// xdg-activation tokens are single-use, so a replayed grant must not activate a window a
    /// second time. Also opportunistically prunes tokens whose `expires_at_monotonic_ns` has
    /// passed: once a token has expired the expiry check alone already rejects any replay of it,
    /// so nothing needs to keep remembering it, which is what keeps this bounded over a long
    /// session instead of growing with every activation ever granted.
    pub(crate) fn try_spend_activation_token(
        &mut self,
        token: &str,
        expires_at_monotonic_ns: u64,
        now_monotonic_ns: u64,
    ) -> bool {
        self.spent_activation_tokens
            .retain(|_, expires_at| *expires_at > now_monotonic_ns);
        if self.spent_activation_tokens.contains_key(token) {
            return false;
        }
        self.spent_activation_tokens
            .insert(token.to_owned(), expires_at_monotonic_ns);
        true
    }

    /// The compositor's grab affordance for `window_id`: the recovery band, and whether the
    /// compositor must paint a temporary tab over it (client-side-decorated / borderless).
    pub fn recovery_affordance(&self, window_id: WindowId) -> Option<RecoveryAffordance> {
        let window = self.windows.get(&window_id)?;
        Some(recovery_affordance(
            window.geometry,
            window.decoration.needs_compositor_recovery_tab(),
        ))
    }

    /// Next (or previous) window in stacking order, for the minimal fallback switcher the
    /// compositor core keeps even when the shell is dead. Shelved windows are skipped — they
    /// are reached through the Window Shelf, not the switcher. `current` of `None`, or a window
    /// no longer in the order, starts from the top.
    pub fn cycle_focus(&self, current: Option<WindowId>, forward: bool) -> Option<WindowId> {
        let order: Vec<WindowId> = self
            .layers
            .order()
            .into_iter()
            .filter(|id| self.windows.get(id).is_some_and(|window| !window.shelved))
            .collect();
        if order.is_empty() {
            return None;
        }
        let Some(position) =
            current.and_then(|id| order.iter().position(|candidate| *candidate == id))
        else {
            return Some(order[0]);
        };
        let len = order.len();
        let next = if forward {
            (position + 1) % len
        } else {
            (position + len - 1) % len
        };
        Some(order[next])
    }

    pub fn window(&self, window_id: WindowId) -> Option<&WindowRecord> {
        self.windows.get(&window_id)
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowRecord> {
        self.windows.values()
    }

    /// Sanitized view for the shell (dock, switcher, window shelf): no PIDs, no raw claims.
    pub fn shell_toplevels(&self) -> Vec<ShellToplevel> {
        let mut list: Vec<ShellToplevel> = self
            .windows
            .values()
            .map(|window| self.to_shell_toplevel(window))
            .collect();
        list.sort_by_key(|toplevel| toplevel.window_id);
        list
    }

    fn to_shell_toplevel(&self, window: &WindowRecord) -> ShellToplevel {
        ShellToplevel {
            window_id: window.id,
            canonical_app_id: window.identity.app_id.clone(),
            instance_id: window.identity.instance_id,
            title: window.title.clone(),
            verified_identity: window.identity.verified,
            activated: window.activated,
            fullscreen: window.fullscreen,
            shelved: window.shelved,
            maximized: window.maximized,
            geometry: window.geometry,
            layer: self.layer_of(window.id).unwrap_or(Layer::Normal),
        }
    }
}

/// Where a maximized window goes: the usable area of the output nearest `current`. Always
/// recoverable, because `Output::usable` refuses a work area too small for a recovery band. With
/// no outputs at all there is nothing to fill, and the window is left as it is.
fn maximized_geometry(current: Rect, outputs: &[Output]) -> Rect {
    if outputs.is_empty() {
        return current;
    }
    choose_target_output(current, outputs).usable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{arb_hotplug_sequence, arb_window, load_topology_sequence};
    use proptest::prelude::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeCache {
        requests: Mutex<Vec<(WindowId, u32)>>,
    }

    impl IdentityCache for FakeCache {
        fn resolve_cached(&self, _pid: u32, _claims: &WindowClaims) -> Option<CachedIdentity> {
            None
        }
        fn request_resolution(&self, window_id: WindowId, pid: u32, _claims: WindowClaims) {
            self.requests.lock().unwrap().push((window_id, pid));
        }
    }

    fn outputs() -> Vec<Output> {
        vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)]
    }

    #[test]
    fn mapping_repairs_geometry_and_requests_identity() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            4242,
            ClientKind::Application,
            WindowClaims::default(),
            "x".into(),
            Rect::new(9000, 9000, 400, 300),
        );
        let window = service.window(id).unwrap();
        assert!(crate::is_recoverable(window.geometry, service.outputs()));
        assert_eq!(window.identity, CachedIdentity::UNKNOWN);
        assert_eq!(service.layer_of(id), Some(Layer::Normal));
        assert_eq!(
            service.identity.requests.lock().unwrap().as_slice(),
            &[(id, 4242)]
        );
    }

    /// Regression for the "most trusted caller refused the least sensitive layer" trap: an
    /// ordinary toplevel mapped by the trusted shell itself (an about box, a file-conflict
    /// dialog) must land on `Layer::Normal` exactly like any other window, not be refused
    /// because `Normal`'s table entry only literally names `ClientKind::Application`.
    #[test]
    fn a_toplevel_mapped_by_the_trusted_shell_still_lands_on_normal() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            99,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "About".into(),
            Rect::new(0, 0, 300, 200),
        );
        assert_eq!(service.layer_of(id), Some(Layer::Normal));
    }

    #[test]
    fn restoring_a_session_validates_against_the_current_topology() {
        // A window saved while a wide external display was attached...
        let saved_geometry = Rect::new(2200, 300, 800, 600);
        // ...restores after the machine reboots with only the laptop panel attached.
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            saved_geometry,
        );
        assert!(crate::is_recoverable(
            service.window(id).unwrap().geometry,
            service.outputs()
        ));
    }

    #[test]
    fn drag_keeps_anchor_and_repairs_on_finish() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(100, 100, 400, 300),
        );
        assert!(service.begin_drag(id, Point { x: 150, y: 120 }));
        let (_, during) = service.update_drag(Point { x: 5000, y: 5000 }).unwrap();
        assert_eq!((during.x, during.y), (4950, 4980)); // anchor preserved, no clamping mid-drag
        let (_, after) = service.finish_drag().unwrap();
        assert!(crate::is_recoverable(after, service.outputs()));
    }

    #[test]
    fn output_change_mid_drag_does_not_fight_the_pointer() {
        let two = vec![
            Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000),
            Output::new("HDMI-1", Rect::new(1920, 0, 2560, 1440), 1000),
        ];
        let mut service = WindowService::new(FakeCache::default(), two.clone());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(100, 100, 400, 300),
        );
        assert!(service.begin_drag(id, Point { x: 150, y: 120 }));
        service.update_drag(Point { x: 2200, y: 300 });

        // The external output disappears mid-drag: the dragged window is left alone (its
        // reported geometry list should not include it), but a second window is still repaired.
        let other = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(2500, 300, 400, 300),
        );
        let changed = service.on_outputs_changed(vec![two[0].clone()]);
        assert!(changed.iter().all(|(changed_id, _)| *changed_id != id));
        assert!(changed.iter().any(|(changed_id, _)| *changed_id == other));

        // The drag continues to track the pointer exactly, unaffected by the repair pass.
        let (_, during) = service.update_drag(Point { x: 2300, y: 400 }).unwrap();
        assert_eq!((during.x, during.y), (2250, 380));

        // Only at drag end is the invariant enforced for the dragged window too.
        let (_, after) = service.finish_drag().unwrap();
        assert!(crate::is_recoverable(after, service.outputs()));
    }

    #[test]
    fn output_removal_migrates_windows() {
        let two = vec![
            Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000),
            Output::new("HDMI-1", Rect::new(1920, 0, 2560, 1440), 1000),
        ];
        let mut service = WindowService::new(FakeCache::default(), two.clone());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(3000, 200, 800, 600),
        );
        let changed = service.on_outputs_changed(vec![two[0].clone()]);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, id);
        assert!(crate::is_recoverable(changed[0].1, service.outputs()));
    }

    #[test]
    fn shelve_and_unshelve_round_trip_stays_recoverable() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            "shelf-me".into(),
            Rect::new(100, 100, 400, 300),
        );
        assert!(service.shelve(id));
        assert!(service.window(id).unwrap().shelved);
        assert_eq!(service.shelf().len(), 1);

        // Topology shrinks while the window sits on the shelf.
        service.on_outputs_changed(vec![Output::new("eDP-1", Rect::new(0, 0, 640, 480), 1000)]);

        let restored = service.unshelve(id).unwrap();
        assert!(!service.window(id).unwrap().shelved);
        assert!(crate::is_recoverable(restored, service.outputs()));
    }

    #[test]
    fn unshelve_of_a_non_shelved_window_is_none() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        assert!(service.unshelve(id).is_none());
    }

    #[test]
    fn shelving_the_activated_window_clears_activation() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        service.activate(id);
        assert!(service.window(id).unwrap().activated);
        service.shelve(id);
        assert!(!service.window(id).unwrap().activated);
    }

    #[test]
    fn activating_a_window_deactivates_the_others_and_raises_it() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let a = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        let b = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        assert!(service.activate(a));
        assert!(service.window(a).unwrap().activated);
        assert!(!service.window(b).unwrap().activated);
        assert_eq!(service.stacking_order().first(), Some(&a));
    }

    #[test]
    fn escape_exits_fullscreen_before_dismissing_modal() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        service.activate(id);
        service.set_fullscreen(id, true);
        service.set_modal(id, true);

        assert_eq!(
            service.escape(None),
            Some(EscapeAction::ExitedFullscreen { window_id: id })
        );
        assert!(!service.window(id).unwrap().fullscreen);
        assert!(service.window(id).unwrap().modal);

        assert_eq!(
            service.escape(None),
            Some(EscapeAction::DismissedModalGrab { window_id: id })
        );
        assert!(!service.window(id).unwrap().modal);

        assert_eq!(service.escape(None), None);
    }

    #[test]
    fn recovery_affordance_flags_client_side_and_borderless_windows() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 400, 300),
        );
        assert!(
            !service
                .recovery_affordance(id)
                .unwrap()
                .requires_compositor_paint
        );

        service.set_decoration(id, Decoration::ClientSide);
        assert!(
            service
                .recovery_affordance(id)
                .unwrap()
                .requires_compositor_paint
        );

        service.set_decoration(id, Decoration::Borderless);
        assert!(
            service
                .recovery_affordance(id)
                .unwrap()
                .requires_compositor_paint
        );
    }

    #[test]
    fn cycle_focus_wraps_and_skips_shelved_windows() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let a = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        let b = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        let c = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        service.shelve(b);

        // Stacking order top-first is c, b, a; b is shelved and must be skipped entirely.
        let first = service.cycle_focus(None, true).unwrap();
        assert_eq!(first, c);
        let second = service.cycle_focus(Some(first), true).unwrap();
        assert_eq!(second, a);
        let back = service.cycle_focus(Some(second), false).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn activation_token_cannot_be_spent_twice_within_its_validity_window() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        assert!(service.try_spend_activation_token("tok", 1_000, 0));
        assert!(!service.try_spend_activation_token("tok", 1_000, 500));
    }

    #[test]
    fn a_spent_token_is_pruned_once_it_would_have_expired_anyway() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        assert!(service.try_spend_activation_token("tok", 100, 0));
        assert!(!service.try_spend_activation_token("tok", 100, 50)); // still within window: blocked
        // By `now = 150` the original grant is long past `expires_at_monotonic_ns = 100`, so it
        // is pruned; the same token string can be spent again as a fresh, later-expiring grant.
        assert!(service.try_spend_activation_token("tok", 300, 150));
    }

    /// Recorded regression: laptop alone → external plugged in → laptop lid closed (external
    /// renumbered to the origin) → external unplugged again. A window opened on the external
    /// output must stay recoverable through every step, including the two output removals.
    #[test]
    fn recorded_hotplug_sequence_keeps_a_window_recoverable() {
        let steps = load_topology_sequence("hotplug-sequence");
        let mut service = WindowService::new(FakeCache::default(), steps[0].clone());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(2200, 300, 800, 600),
        );

        for step in &steps[1..] {
            service.on_outputs_changed(step.clone());
            assert!(
                crate::is_recoverable(service.window(id).unwrap().geometry, service.outputs()),
                "lost after hotplug step {step:?}"
            );
        }
    }

    #[test]
    fn shelving_hides_and_unshelving_shows() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(10, 10, 400, 300),
        );
        assert!(service.is_visible(id));
        assert!(service.shelve(id));
        assert!(!service.is_visible(id));
        assert!(service.unshelve(id).is_some());
        assert!(service.is_visible(id));
        assert!(
            !service.is_visible(WindowId(999)),
            "unknown windows are not visible"
        );
    }

    #[test]
    fn a_reveal_hides_client_windows_but_never_shell_surfaces() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let app = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(10, 10, 400, 300),
        );
        let home = service.on_toplevel_mapped(
            2,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "Home Surface".into(),
            Rect::new(0, 0, 1920, 1080),
        );
        service
            .set_layer(home, Layer::HomeSurface, ClientKind::TrustedShell)
            .unwrap();
        let on_top = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(50, 50, 100, 100),
        );
        service
            .set_layer(on_top, Layer::AppOnTop, ClientKind::Application)
            .unwrap();

        assert!(service.set_home_revealed(true));
        assert!(!service.set_home_revealed(true), "no change reported twice");
        assert!(!service.is_visible(app));
        assert!(!service.is_visible(on_top));
        assert!(
            service.is_visible(home),
            "the Home Surface is what a reveal reveals"
        );

        // A new window ends the reveal: the user must be able to see what just opened.
        let fresh = service.on_toplevel_mapped(
            4,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        assert!(!service.home_revealed());
        assert!(service.is_visible(fresh));
        assert!(service.is_visible(app));
    }

    #[test]
    fn a_maximized_window_follows_its_output_through_hotplug() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(100, 100, 400, 300),
        );
        assert_eq!(
            service.set_maximized(id, true),
            Some(Rect::new(0, 0, 1920, 1080))
        );
        let smaller = vec![Output::new("eDP-1", Rect::new(0, 0, 1280, 720), 1000)];
        let changed = service.on_outputs_changed(smaller);
        assert_eq!(changed, vec![(id, Rect::new(0, 0, 1280, 720))]);
        assert!(service.window(id).unwrap().maximized);
        assert_eq!(
            service.set_maximized(id, false),
            Some(Rect::new(100, 100, 400, 300))
        );
        assert_eq!(service.set_maximized(id, false), None, "already floating");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]

        /// Maximizing fills exactly one output's usable area — which `Output::usable` guarantees
        /// has room for a recovery band — and unmaximizing lands somewhere recoverable too, for
        /// any topology and any starting rectangle.
        #[test]
        fn maximize_and_unmaximize_are_always_recoverable(
            topology in crate::test_support::arb_topology(),
            window in arb_window(),
        ) {
            let mut service = WindowService::new(FakeCache::default(), topology.clone());
            let id = service.on_toplevel_mapped(1, ClientKind::Application, WindowClaims::default(), String::new(), window);
            let maximized = service.set_maximized(id, true).expect("first maximize always changes state");
            prop_assert!(crate::is_recoverable(maximized, service.outputs()));
            prop_assert!(topology.iter().any(|output| output.usable() == maximized));
            let restored = service.set_maximized(id, false).expect("unmaximize always changes state");
            prop_assert!(crate::is_recoverable(restored, service.outputs()));
            prop_assert_eq!(restored.width, window.width);
            prop_assert_eq!(restored.height, window.height);
        }

        /// A reveal is a view: whatever sequence of reveals, shelves and activations happens, a
        /// hidden window is hidden because it is shelved or because a reveal is in effect —
        /// never because its record was changed — and shelved windows never hold activation.
        #[test]
        fn hidden_windows_are_explained_by_shelf_or_reveal_alone(
            steps in proptest::collection::vec(0u8..5, 1..30),
        ) {
            let mut service = WindowService::new(FakeCache::default(), outputs());
            let a = service.on_toplevel_mapped(1, ClientKind::Application, WindowClaims::default(), String::new(), Rect::new(0, 0, 300, 200));
            let b = service.on_toplevel_mapped(2, ClientKind::Application, WindowClaims::default(), String::new(), Rect::new(50, 50, 300, 200));
            for step in steps {
                match step {
                    0 => { service.shelve(a); }
                    1 => { service.unshelve(a); }
                    2 => { service.activate(b); }
                    3 => { service.set_home_revealed(true); }
                    _ => { service.set_home_revealed(false); }
                }
                for id in [a, b] {
                    let record = service.window(id).unwrap();
                    prop_assert_eq!(
                        service.is_visible(id),
                        !record.shelved && !service.home_revealed(),
                    );
                    if record.shelved {
                        prop_assert!(!record.activated);
                    }
                }
            }
        }

        /// Across an arbitrary hotplug sequence (outputs connecting and disconnecting in turn),
        /// a mapped window stays recoverable after every step, and replaying the identical
        /// sequence from the identical starting geometry always ends at the identical geometry
        /// — output removal migrates windows deterministically, not by hash-map iteration luck.
        #[test]
        fn hotplug_sequences_keep_windows_recoverable_and_migrate_deterministically(
            sequence in arb_hotplug_sequence(),
            window in arb_window(),
        ) {
            let mut first = WindowService::new(FakeCache::default(), sequence[0].clone());
            let first_id = first.on_toplevel_mapped(1, ClientKind::Application, WindowClaims::default(), String::new(), window);
            let mut second = WindowService::new(FakeCache::default(), sequence[0].clone());
            let second_id = second.on_toplevel_mapped(1, ClientKind::Application, WindowClaims::default(), String::new(), window);

            for step in &sequence[1..] {
                first.on_outputs_changed(step.clone());
                second.on_outputs_changed(step.clone());

                prop_assert!(crate::is_recoverable(first.window(first_id).unwrap().geometry, first.outputs()));
                prop_assert_eq!(
                    first.window(first_id).unwrap().geometry,
                    second.window(second_id).unwrap().geometry,
                );
            }
        }
    }
}
