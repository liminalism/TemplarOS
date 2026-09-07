//! Window records, identity cache boundary, drag handling, layer membership, the Window Shelf
//! and the fullscreen/modal escape route.

use crate::{
    geometry::{
        Output, Point, RecoveryAffordance, Rect, ResizeEdges, bounding_rect, choose_target_output,
        recovery_affordance, repair_window, resize_rect,
    },
    group::{GroupId, GroupRecord, accent_for},
    layers::{ClientKind, Layer, LayerError, LayerStack, StackPosition},
    shell::{GroupInfo, ShellToplevel},
};
use lege_types::{AppId, InstanceId, WindowClaims};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
    /// The window group this window belongs to, if any (`crate::group`). Membership lives here,
    /// on the window, so no window can be in two groups.
    pub group: Option<GroupId>,
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

/// An interactive resize in flight. Unlike a drag, this remembers where the window *started*
/// rather than tracking a delta frame by frame: every update is recomputed from the original
/// rectangle and the total pointer displacement, so a pointer that runs past the minimum size
/// and comes back reproduces the size it had on the way out instead of drifting by however much
/// the clamp swallowed.
#[derive(Clone, Copy, Debug)]
struct ResizeState {
    window_id: WindowId,
    edges: ResizeEdges,
    origin: Rect,
    pointer_origin: Point,
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
    resize: Option<ResizeState>,
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
    /// The geometry and maximize state every eligible window had when the first arrangement
    /// preset was requested (`arrange_windows`): preset switches are auditioned over this one
    /// baseline, and `restore_arrangement` returns surviving windows exactly to it.
    arrangement_baseline: Option<Vec<(WindowId, Rect, bool)>>,
    /// Every live window group. Ordered so that iteration — the switcher's group list, the
    /// snapshot — is deterministic rather than hash-ordered.
    groups: BTreeMap<GroupId, GroupRecord>,
    next_group_id: u64,
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
            resize: None,
            layers: LayerStack::new(),
            spent_activation_tokens: HashMap::new(),
            home_revealed: false,
            arrangement_baseline: None,
            groups: BTreeMap::new(),
            next_group_id: 1,
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
        // Placement, not obedience: an origin-located request has no position intent (xdg
        // clients cannot ask for one) and is centred-and-cascaded; a real position — a restored
        // session window, an X11 client that placed itself — is honoured and repaired. The
        // cascade counts existing *client* windows so shell surfaces (the Home surface maps
        // through here before its layer claim) never push application windows off centre.
        let existing_clients = self
            .windows
            .values()
            .filter(|window| self.is_client_window(window.id))
            .count();
        let geometry =
            crate::geometry::place_new_window(requested_geometry, existing_clients, &self.outputs);

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
                group: None,
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
        let group = self.group_of(window_id);
        self.windows.remove(&window_id);
        self.layers.remove(window_id);
        // Rule 5: a group has at least two members, so the last-but-one departure — a window
        // closing included — dissolves it rather than leaving a group of one.
        if let Some(group) = group {
            self.settle_group(group);
        }
        if self.drag.is_some_and(|drag| drag.window_id == window_id) {
            self.drag = None;
        }
        if self
            .resize
            .is_some_and(|resize| resize.window_id == window_id)
        {
            self.resize = None;
        }
    }

    /// Outputs changed (hotplug, scale, resume). Every *client* window is re-validated except the
    /// one currently being dragged or resized: repairing it here would fight the user's pointer
    /// and be overwritten by the very next `update_drag`/`update_resize` anyway, so that
    /// invariant is instead enforced once, in `finish_drag`/`finish_resize`. The returned list is
    /// what must be re-configured on the clients.
    ///
    /// Shell surfaces are never repaired. Repair measures a window's tag strip against
    /// [`Output::usable`], and a surface holding an exclusive zone is the thing that *creates*
    /// that usable area: a bar on `Exclusive{Bottom, 48}` has its own strip below `usable()` by
    /// construction, is judged unrecoverable, and is pushed a further zone-height off its edge —
    /// where the next `apply_surface_claims` puts it straight back. Two writers, alternating,
    /// which is exactly the flicker. A shell surface's placement is
    /// [`geometry::shell_surface_placement`]'s job and the compositor re-runs it on every
    /// topology change (`backend::recompute_outputs`); recovery is a client-window property.
    pub fn on_outputs_changed(&mut self, outputs: Vec<Output>) -> Vec<(WindowId, Rect)> {
        self.outputs = outputs;
        let dragging = self.drag.map(|drag| drag.window_id);
        let resizing = self.resize.map(|resize| resize.window_id);

        // Ids first: `is_client_window` reads the layer stack, which `values_mut` would have
        // borrowed away. `windows` is a `HashMap`, so this changes no ordering guarantee.
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        let mut changed = Vec::new();
        for id in ids {
            if Some(id) == dragging || Some(id) == resizing || !self.is_client_window(id) {
                continue;
            }
            let Some(window) = self.windows.get_mut(&id) else {
                continue;
            };
            let repaired = if window.maximized {
                // A maximized window is not floating: it fills whichever output is now closest
                // to where it was, rather than being nudged until its band is visible.
                maximized_geometry(window.geometry, &self.outputs)
            } else {
                repair_window(window.geometry, &self.outputs)
            };
            if repaired != window.geometry {
                window.geometry = repaired;
                changed.push((id, repaired));
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

    /// The windows an arrangement preset applies to: mapped client windows that are neither
    /// shelved nor fullscreen — the activated one first (it takes the main slot of Focus and
    /// MainAndStack), then by id, so the result is deterministic in the service state.
    fn arrangement_eligible(&self) -> Vec<WindowId> {
        let mut eligible: Vec<(bool, WindowId)> = self
            .windows
            .values()
            .filter(|window| {
                self.is_client_window(window.id) && !window.shelved && !window.fullscreen
            })
            .map(|window| (window.activated, window.id))
            .collect();
        eligible.sort_by_key(|(activated, id)| (!*activated, *id));
        eligible.into_iter().map(|(_, id)| id).collect()
    }

    /// Arrange every eligible window on its output per `preset`, snapshotting the pre-preset
    /// geometry and maximize state as the baseline on the first request. A later request —
    /// switching presets, or the same preset after windows opened or closed — recomputes from
    /// the current eligible set and the output, never from the previous preset's result, and
    /// keeps the retained baseline. Returns the windows moved with their new geometry.
    pub fn arrange_windows(
        &mut self,
        preset: crate::arrangement::ArrangementPreset,
    ) -> Vec<(WindowId, Rect)> {
        let eligible = self.arrangement_eligible();
        if eligible.is_empty() {
            return Vec::new();
        }
        if self.arrangement_baseline.is_none() {
            self.arrangement_baseline = Some(
                eligible
                    .iter()
                    .filter_map(|id| self.windows.get(id))
                    .map(|window| (window.id, window.geometry, window.maximized))
                    .collect(),
            );
        }
        let baseline = self.arrangement_baseline.clone().unwrap_or_default();
        let baseline_area = |id: WindowId, current: Rect| -> u64 {
            let rect = baseline
                .iter()
                .find(|(entry, _, _)| *entry == id)
                .map_or(current, |(_, rect, _)| *rect);
            u64::from(rect.width.unsigned_abs()) * u64::from(rect.height.unsigned_abs())
        };
        let outputs = self.outputs.clone();
        let mut moves = Vec::with_capacity(eligible.len());
        for output in &outputs {
            let group: Vec<WindowId> = eligible
                .iter()
                .copied()
                .filter(|id| {
                    self.windows.get(id).is_some_and(|window| {
                        choose_target_output(window.geometry, &outputs).name == output.name
                    })
                })
                .collect();
            let weights: Vec<u64> = group
                .iter()
                .map(|id| {
                    let current = self
                        .windows
                        .get(id)
                        .map_or(Rect::new(0, 0, 1, 1), |w| w.geometry);
                    baseline_area(*id, current).max(1)
                })
                .collect();
            let rects = crate::arrangement::preset_rects(preset, &weights, output.usable());
            for (id, rect) in group.into_iter().zip(rects) {
                if let Some(window) = self.windows.get_mut(&id) {
                    window.maximized = false;
                    window.restore_geometry = None;
                    window.geometry = rect;
                    moves.push((id, rect));
                }
            }
        }
        moves
    }

    /// Return every surviving baseline window exactly to its pre-arrangement geometry and
    /// maximize state, repaired only where the output topology no longer reaches the old
    /// rectangle, and spend the baseline. No baseline, or none of its windows surviving,
    /// returns no moves.
    pub fn restore_arrangement(&mut self) -> Vec<(WindowId, Rect)> {
        let Some(baseline) = self.arrangement_baseline.take() else {
            return Vec::new();
        };
        let outputs = self.outputs.clone();
        let mut moves = Vec::with_capacity(baseline.len());
        for (id, rect, maximized) in baseline {
            if !self.is_client_window(id) {
                continue;
            }
            let Some(window) = self.windows.get_mut(&id) else {
                continue;
            };
            if window.shelved {
                continue;
            }
            window.maximized = maximized;
            window.geometry = if maximized {
                window.restore_geometry = Some(repair_window(rect, &outputs));
                maximized_geometry(rect, &outputs)
            } else {
                window.restore_geometry = None;
                repair_window(rect, &outputs)
            };
            moves.push((id, window.geometry));
        }
        moves
    }

    /// Whether a preset baseline is retained — i.e. Restore has something to return to.
    pub fn arrangement_active(&self) -> bool {
        self.arrangement_baseline.is_some()
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
        if self.layer_of(window_id) == Some(Layer::HomeSurface) {
            return self.home_revealed;
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
        // One pointer, one gesture: a move started while a resize is in flight would leave two
        // states both rewriting the same geometry from different anchors.
        if self.resize.is_some() {
            return false;
        }
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

    /// Begin an interactive resize of `window_id` from `pointer_global`, dragging `edges`.
    ///
    /// Refused — `false`, and nothing started — for an unknown window, for a resize that names
    /// no edge, while a move of any window is in flight, and for a maximized window: a maximized
    /// window's geometry is re-derived from its output on every topology change
    /// ([`Self::on_outputs_changed`]), so a size dragged onto it would be silently discarded the
    /// next time anything about the outputs moved. Unmaximize first, which is a route the user
    /// already has.
    pub fn begin_resize(
        &mut self,
        window_id: WindowId,
        pointer_global: Point,
        edges: ResizeEdges,
    ) -> bool {
        if edges.is_empty() || self.drag.is_some() {
            return false;
        }
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if window.maximized {
            return false;
        }
        self.resize = Some(ResizeState {
            window_id,
            edges,
            origin: window.geometry,
            pointer_origin: pointer_global,
        });
        true
    }

    /// The size the window should now have, from the pointer's total displacement since
    /// [`begin_resize`](Self::begin_resize). Clamped to the minimum by
    /// [`resize_rect`](crate::geometry::resize_rect); never repaired mid-gesture, for the same
    /// reason [`update_drag`](Self::update_drag) is not.
    pub fn update_resize(&mut self, pointer_global: Point) -> Option<(WindowId, Rect)> {
        let resize = self.resize?;
        let dx = pointer_global.x.saturating_sub(resize.pointer_origin.x);
        let dy = pointer_global.y.saturating_sub(resize.pointer_origin.y);
        let geometry = resize_rect(resize.origin, resize.edges, dx, dy);
        let window = self.windows.get_mut(&resize.window_id)?;
        window.geometry = geometry;
        Some((window.id, geometry))
    }

    /// Resize ended: the recovery invariant is enforced once, at the end, exactly as it is for a
    /// move. A resize that dragged a top or left edge *does* move the window, so this is not a
    /// formality — a window grown upward past the top of its output is repaired back down.
    pub fn finish_resize(&mut self) -> Option<(WindowId, Rect)> {
        let resize = self.resize.take()?;
        let window = self.windows.get_mut(&resize.window_id)?;
        window.geometry = repair_window(window.geometry, &self.outputs);
        Some((window.id, window.geometry))
    }

    pub fn is_resizing(&self, window_id: WindowId) -> bool {
        self.resize
            .is_some_and(|resize| resize.window_id == window_id)
    }

    /// Whether `window_id` wears the compositor's frame — the tag strip drawn below its client
    /// area rather than over it. Every client window does, with two exceptions. A fullscreen
    /// window is the client's whole output by definition. A maximized window on an output whose
    /// bottom edge a shell surface holds (`Output::reserves_bottom`, the shell's bar) draws no
    /// tags: its controls are the bar's. The moment no such zone is claimed — the shell is dead,
    /// or never claimed one — the tags come back, so controls never vanish.
    pub fn has_frame(&self, window_id: WindowId) -> bool {
        if !self.is_client_window(window_id) {
            return false;
        }
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if window.fullscreen {
            return false;
        }
        if window.maximized && !self.outputs.is_empty() {
            return !crate::geometry::choose_target_output(window.geometry, &self.outputs)
                .reserves_bottom();
        }
        true
    }

    /// The rectangle `window_id`'s client surface occupies: above the tags for a framed window
    /// ([`client_area`](crate::geometry::client_area)), the whole geometry for anything else —
    /// a shell surface, a fullscreen window. What the compositor maps the surface at and
    /// configures the client to; the geometry itself is the frame.
    pub fn client_area(&self, window_id: WindowId) -> Option<Rect> {
        let window = self.windows.get(&window_id)?;
        Some(if self.has_frame(window_id) {
            crate::geometry::client_area(window.geometry)
        } else {
            window.geometry
        })
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
        let mut shelved: Vec<&WindowRecord> = self
            .windows
            .values()
            .filter(|window| self.is_client_window(window.id) && window.shelved)
            .collect();
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
    ///
    /// A grouped window raises its **whole group** contiguously, keeping the members' relative
    /// order with the activated one on top (planning doc §2 rule 3). That is what makes a group
    /// behave as one set of windows rather than as a label — and it is also why it is safe:
    /// raising changes rank, never geometry, so the recovery invariant cannot notice it.
    pub fn activate(&mut self, window_id: WindowId) -> bool {
        if !self.windows.contains_key(&window_id) {
            return false;
        }
        for window in self.windows.values_mut() {
            window.activated = window.id == window_id;
        }
        if let Some(group) = self.group_of(window_id) {
            let mut order: Vec<WindowId> = self
                .layers
                .order()
                .into_iter()
                .filter(|id| *id != window_id && self.group_of(*id) == Some(group))
                .collect();
            order.insert(0, window_id);
            self.layers.raise_many(&order);
            if let Some(record) = self.groups.get_mut(&group) {
                record.last_active = window_id;
            }
        } else {
            self.layers.raise(window_id);
        }
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

    /// The compositor's grab affordance for `window_id`: the tag strip, and whether the
    /// compositor must paint a temporary tab over it (client-side-decorated / borderless).
    /// Answered for every client window; whether the tags are *drawn* is [`Self::has_frame`].
    pub fn recovery_affordance(&self, window_id: WindowId) -> Option<RecoveryAffordance> {
        if !self.is_client_window(window_id) {
            return None;
        }
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
        step_through(&self.switchable_order(), current, forward)
    }

    // ---- Window groups ---------------------------------------------------------------------
    //
    // Explicit, compositor-owned sets of two or more windows that park, return and stack as one
    // (`crate::group`, planning doc §2). Everything below is membership and stacking; the only
    // geometry any of it touches is `place_beside`/`move_group`, and both end in `repair_window`.

    /// Every live group, by id.
    pub fn groups(&self) -> &BTreeMap<GroupId, GroupRecord> {
        &self.groups
    }

    pub fn group_record(&self, group: GroupId) -> Option<&GroupRecord> {
        self.groups.get(&group)
    }

    /// Which group `window_id` belongs to, if any.
    pub fn group_of(&self, window_id: WindowId) -> Option<GroupId> {
        self.windows.get(&window_id).and_then(|window| window.group)
    }

    /// Every member of `group`, by window id — a stable order that does not depend on stacking.
    pub fn group_members(&self, group: GroupId) -> Vec<WindowId> {
        let mut members: Vec<WindowId> = self
            .windows
            .values()
            .filter(|window| window.group == Some(group))
            .map(|window| window.id)
            .collect();
        members.sort_unstable();
        members
    }

    /// `group`'s members in stacking order, top-most first.
    pub fn group_stacking(&self, group: GroupId) -> Vec<WindowId> {
        self.layers
            .order()
            .into_iter()
            .filter(|id| self.group_of(*id) == Some(group))
            .collect()
    }

    /// Put `b` in `a`'s group, making one if neither has one and merging the two if both do.
    ///
    /// `a` is the anchor: when both windows are already grouped, `a`'s group — its id, name and
    /// accent — is the one that survives, and `b`'s members all move into it. When only `b` has a
    /// group, `b`'s group survives and `a` joins it: dissolving a live group (with its name and
    /// its colour) to honour an identity rule would be a worse answer than the one the user can
    /// already see on screen.
    ///
    /// Returns the surviving group, or `None` if either window is unknown, is not a client
    /// window, or the two are the same window. **Placement is not done here** — see
    /// [`Self::place_beside`] and `WindowCommand::Group`, the route that both groups and places
    /// (planning doc §2 rule 6).
    pub fn group(&mut self, a: WindowId, b: WindowId) -> Option<GroupId> {
        if a == b || !self.is_client_window(a) || !self.is_client_window(b) {
            return None;
        }
        let group = match (self.group_of(a), self.group_of(b)) {
            (Some(existing), _) | (None, Some(existing)) => existing,
            (None, None) => {
                let id = GroupId(self.next_group_id);
                self.next_group_id = self.next_group_id.saturating_add(1);
                self.groups.insert(
                    id,
                    GroupRecord {
                        name: None,
                        accent: accent_for(id),
                        last_active: a,
                    },
                );
                id
            }
        };
        if let Some(other) = self.group_of(b)
            && other != group
        {
            for member in self.group_members(other) {
                self.set_group(member, Some(group));
            }
            self.groups.remove(&other);
        }
        self.set_group(a, Some(group));
        self.set_group(b, Some(group));
        if let Some(record) = self.groups.get_mut(&group) {
            record.last_active = a;
        }
        Some(group)
    }

    /// Take `window_id` out of its group, dissolving the group if that leaves it with fewer than
    /// two members. Returns the group it left.
    pub fn ungroup(&mut self, window_id: WindowId) -> Option<GroupId> {
        let group = self.group_of(window_id)?;
        self.set_group(window_id, None);
        self.settle_group(group);
        Some(group)
    }

    /// Name `window_id`'s group. Returns the group, or `None` if the window is in none.
    pub fn set_group_name(&mut self, window_id: WindowId, name: Option<String>) -> Option<GroupId> {
        let group = self.group_of(window_id)?;
        let record = self.groups.get_mut(&group)?;
        record.name = name.filter(|name| !name.is_empty());
        Some(group)
    }

    /// Set one window's membership without touching the group table.
    fn set_group(&mut self, window_id: WindowId, group: Option<GroupId>) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.group = group;
        }
    }

    /// Keep `group` honest after a membership change: dissolve it when fewer than two members are
    /// left (rule 5), and keep `last_active` pointing at a member that still exists.
    fn settle_group(&mut self, group: GroupId) {
        let members = self.group_members(group);
        if members.len() < 2 {
            for member in members {
                self.set_group(member, None);
            }
            self.groups.remove(&group);
            return;
        }
        if let Some(record) = self.groups.get_mut(&group)
            && !members.contains(&record.last_active)
        {
            record.last_active = members[0];
        }
    }

    /// Shelve every member of `window_id`'s group under **one** shelf rank, so the Window Shelf
    /// lists the group as a single entry rather than as scattered windows. A window in no group
    /// is shelved alone. Returns the windows now on the shelf.
    pub fn shelve_group(&mut self, window_id: WindowId) -> Vec<WindowId> {
        let members = self.group_or_window(window_id);
        if members.is_empty() {
            return Vec::new();
        }
        let rank = self.next_shelf_rank;
        self.next_shelf_rank = self.next_shelf_rank.saturating_add(1);
        for id in &members {
            if let Some(window) = self.windows.get_mut(id) {
                window.shelved = true;
                window.shelf_rank = Some(rank);
                window.activated = false;
            }
        }
        members
    }

    /// Bring every shelved member of `window_id`'s group back, each geometry re-validated against
    /// the current topology exactly as [`Self::unshelve`] does. Returns what came back.
    pub fn unshelve_group(&mut self, window_id: WindowId) -> Vec<(WindowId, Rect)> {
        self.group_or_window(window_id)
            .into_iter()
            .filter_map(|id| self.unshelve(id).map(|geometry| (id, geometry)))
            .collect()
    }

    /// `window_id`'s group's members, or just `window_id` when it is in no group. Empty for a
    /// window the service does not know.
    fn group_or_window(&self, window_id: WindowId) -> Vec<WindowId> {
        match self.group_of(window_id) {
            Some(group) => self.group_members(group),
            None if self.windows.contains_key(&window_id) => vec![window_id],
            None => Vec::new(),
        }
    }

    /// Move `anchor` to `geometry` and every other member of its group by the same displacement,
    /// repairing each one. Returns every window that moved, with the geometry to configure.
    ///
    /// This is the discrete "move the group": the Home tile, a finished band drag. An interactive
    /// drag moves the followers as it goes and repairs once at the end, exactly as it does for a
    /// lone window (COMPOSITOR.md's drag rows).
    pub fn move_group(&mut self, anchor: WindowId, geometry: Rect) -> Vec<(WindowId, Rect)> {
        let Some(current) = self.window(anchor).map(|window| window.geometry) else {
            return Vec::new();
        };
        let dx = geometry.x.saturating_sub(current.x);
        let dy = geometry.y.saturating_sub(current.y);
        let mut moves = Vec::new();
        for id in self.group_or_window(anchor) {
            let Some(window) = self.windows.get(&id) else {
                continue;
            };
            let target = if id == anchor {
                geometry
            } else {
                Rect {
                    x: window.geometry.x.saturating_add(dx),
                    y: window.geometry.y.saturating_add(dy),
                    ..window.geometry
                }
            };
            let repaired = repair_window(target, &self.outputs);
            self.set_geometry(id, repaired);
            moves.push((id, repaired));
        }
        moves
    }

    /// Set `window_id` down beside `target` — the nearest free side, edge to edge
    /// (`geometry::place_beside`). A window already beside its target is left exactly where the
    /// user put it. Returns the geometry to configure, or `None` when nothing moved.
    pub fn place_beside(&mut self, window_id: WindowId, target: WindowId) -> Option<Rect> {
        if window_id == target {
            return None;
        }
        let window = self.window(window_id)?.geometry;
        let target_geometry = self.window(target)?.geometry;
        if crate::geometry::is_beside(window, target_geometry) {
            return None;
        }
        let placed = crate::geometry::place_beside(window, target_geometry, &self.outputs);
        let record = self.windows.get_mut(&window_id)?;
        // A maximized window that is set down beside something is no longer maximized: its
        // geometry would otherwise be re-derived from its output on the next topology change,
        // which would silently undo the placement.
        record.maximized = false;
        record.restore_geometry = None;
        record.geometry = placed;
        Some(placed)
    }

    /// The rectangle the compositor draws a group's outline around: the union of its
    /// **unshelved** members' geometries (planning doc §2 rule 7). `None` for an unknown group or
    /// one whose members are all shelved.
    pub fn group_outline(&self, group: GroupId) -> Option<Rect> {
        let rects: Vec<Rect> = self
            .group_members(group)
            .into_iter()
            .filter_map(|id| self.windows.get(&id))
            .filter(|window| !window.shelved)
            .map(|window| window.geometry)
            .collect();
        bounding_rect(&rects)
    }

    /// `Alt+Tab`: the next (or previous) window **inside `current`'s group**, in stacking order,
    /// shelved windows skipped. A window in no group cycles the loose set, which is why this is
    /// the ordinary switcher too — with no group anywhere it behaves exactly like
    /// [`Self::cycle_focus`] (planning doc §2 rule 4).
    pub fn cycle_within_group(&self, current: Option<WindowId>, forward: bool) -> Option<WindowId> {
        let Some(current) = current else {
            // Nothing is focused — nothing to be *inside* of. The switcher must still land
            // somewhere, so it starts from the top of everything, exactly as `cycle_focus` does.
            return self.cycle_focus(None, forward);
        };
        let scope = self.group_of(current);
        let order: Vec<WindowId> = self
            .switchable_order()
            .into_iter()
            .filter(|id| self.group_of(*id) == scope)
            .collect();
        step_through(&order, Some(current), forward)
    }

    /// `Super+Tab`: the window to land on in the next (or previous) *group*.
    ///
    /// One entry per group, ordered by the stacking order of their top-most member, plus one
    /// entry for the loose set (its top-most ungrouped window). A group's entry is its
    /// `last_active` member when that member is still live and unshelved, otherwise its top-most
    /// one — so coming back to a group comes back to the window you left it on.
    pub fn cycle_groups(&self, current: Option<WindowId>, forward: bool) -> Option<WindowId> {
        let targets = self.group_cycle_targets();
        if targets.is_empty() {
            return None;
        }
        let scope = current.and_then(|id| self.group_of(id));
        let here = current.and_then(|current| {
            self.windows.get(&current)?;
            targets
                .iter()
                .position(|target| self.group_of(*target) == scope)
        });
        let Some(position) = here else {
            return Some(targets[0]);
        };
        let len = targets.len();
        let next = if forward {
            (position + 1) % len
        } else {
            (position + len - 1) % len
        };
        Some(targets[next])
    }

    /// One focus target per group, in stacking order, with the loose set counted as one group —
    /// the list `Super+Tab` steps through and the group switcher HUD renders.
    pub fn group_cycle_targets(&self) -> Vec<WindowId> {
        let mut targets = Vec::new();
        let mut seen: std::collections::BTreeSet<GroupId> = std::collections::BTreeSet::new();
        let mut loose_taken = false;
        for id in self.switchable_order() {
            match self.group_of(id) {
                Some(group) => {
                    if !seen.insert(group) {
                        continue;
                    }
                    let landing = self
                        .groups
                        .get(&group)
                        .map(|record| record.last_active)
                        .filter(|member| {
                            self.group_of(*member) == Some(group)
                                && self.windows.get(member).is_some_and(|w| !w.shelved)
                        })
                        .unwrap_or(id);
                    targets.push(landing);
                }
                None => {
                    if !loose_taken {
                        loose_taken = true;
                        targets.push(id);
                    }
                }
            }
        }
        targets
    }

    /// Every window a switcher may land on, top-most first: client windows that are not shelved.
    fn switchable_order(&self) -> Vec<WindowId> {
        self.layers
            .order()
            .into_iter()
            .filter(|id| {
                self.is_client_window(*id)
                    && self.windows.get(id).is_some_and(|window| !window.shelved)
            })
            .collect()
    }

    pub fn window(&self, window_id: WindowId) -> Option<&WindowRecord> {
        self.windows.get(&window_id)
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowRecord> {
        self.windows.values()
    }

    /// Whether `window_id` belongs to the normal application workspace. Trusted-shell dialogs
    /// may deliberately live here too; the distinction is the layer, not which process mapped
    /// it. Shell chrome, notifications, system modals and the Home Surface are excluded.
    pub fn is_client_window(&self, window_id: WindowId) -> bool {
        matches!(
            self.layer_of(window_id),
            Some(Layer::Normal | Layer::AppOnTop)
        )
    }

    /// Sanitized application-window view for the shell (dock, switcher, window shelf): no PIDs,
    /// no raw claims, and no shell-owned surfaces. Feeding the Home Surface or dock back into this
    /// list would make the shell create session tiles and recursive previews of its own windows.
    pub fn shell_toplevels(&self) -> Vec<ShellToplevel> {
        let mut list: Vec<ShellToplevel> = self
            .windows
            .values()
            .filter(|window| self.is_client_window(window.id))
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
            group: window.group.and_then(|id| {
                self.groups.get(&id).map(|record| GroupInfo {
                    group_id: id.0,
                    name: record.display_name().to_owned(),
                    accent: record.accent,
                })
            }),
        }
    }
}

/// One step forward or backward through `order`, wrapping. `current` absent from the list — a
/// window that has just been shelved, or one in another group — starts from the top, which is
/// what makes every switcher land somewhere rather than nowhere.
fn step_through(order: &[WindowId], current: Option<WindowId>, forward: bool) -> Option<WindowId> {
    if order.is_empty() {
        return None;
    }
    let Some(position) = current.and_then(|id| order.iter().position(|candidate| *candidate == id))
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

    fn one_window(geometry: Rect) -> (WindowService<FakeCache>, WindowId) {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            geometry,
        );
        (service, id)
    }

    #[test]
    fn resizing_the_bottom_right_corner_grows_the_window_and_repairs_at_the_end() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        assert!(service.begin_resize(
            id,
            Point { x: 500, y: 400 },
            crate::ResizeEdges::BOTTOM_RIGHT
        ));
        assert!(service.is_resizing(id));
        let (_, during) = service.update_resize(Point { x: 700, y: 550 }).unwrap();
        assert_eq!(during, Rect::new(100, 100, 600, 450));
        let (_, after) = service.finish_resize().unwrap();
        assert!(crate::is_recoverable(after, service.outputs()));
        assert!(!service.is_resizing(id));
        assert_eq!(service.update_resize(Point { x: 0, y: 0 }), None);
    }

    /// The reason the gesture remembers its origin rather than accumulating deltas: a pointer
    /// pushed well past the minimum and brought back must retrace the sizes it passed through,
    /// not be left however much smaller the clamp swallowed on the way out.
    #[test]
    fn a_resize_past_the_minimum_and_back_returns_to_the_size_the_pointer_names() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.begin_resize(
            id,
            Point { x: 500, y: 400 },
            crate::ResizeEdges::BOTTOM_RIGHT,
        );
        let (_, squashed) = service.update_resize(Point { x: -9000, y: -9000 }).unwrap();
        assert_eq!(
            (squashed.width, squashed.height),
            (
                crate::geometry::MIN_WINDOW_WIDTH,
                crate::geometry::MIN_WINDOW_HEIGHT
            )
        );
        let (_, back) = service.update_resize(Point { x: 700, y: 550 }).unwrap();
        assert_eq!(back, Rect::new(100, 100, 600, 450));
    }

    #[test]
    fn one_pointer_runs_one_gesture() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.begin_resize(
            id,
            Point { x: 500, y: 400 },
            crate::ResizeEdges::BOTTOM_RIGHT,
        );
        assert!(
            !service.begin_drag(id, Point { x: 150, y: 110 }),
            "a move must not start on top of a live resize"
        );
        service.finish_resize();
        assert!(service.begin_drag(id, Point { x: 150, y: 110 }));
        assert!(
            !service.begin_resize(
                id,
                Point { x: 500, y: 400 },
                crate::ResizeEdges::BOTTOM_RIGHT
            ),
            "and a resize must not start on top of a live move"
        );
    }

    #[test]
    fn a_resize_is_refused_where_it_could_not_be_honoured() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        assert!(
            !service.begin_resize(id, Point { x: 0, y: 0 }, crate::ResizeEdges::default()),
            "an `xdg_toplevel.resize` naming no edge asks for nothing"
        );
        assert!(!service.begin_resize(
            WindowId(9999),
            Point { x: 0, y: 0 },
            crate::ResizeEdges::BOTTOM_RIGHT
        ));
        // A maximized window's geometry is re-derived from its output; a dragged size would be
        // thrown away the next time anything about the topology moved.
        service.set_maximized(id, true);
        assert!(!service.begin_resize(id, Point { x: 0, y: 0 }, crate::ResizeEdges::BOTTOM_RIGHT));
    }

    /// The output every bar test below runs on: 1920×1080 with `Exclusive{Bottom, 48}` claimed,
    /// so `usable()` ends at 1032 and every window's tag strip below that line is "unreachable".
    fn output_with_a_bottom_bar(size: i32) -> Output {
        let screen = outputs()[0].logical;
        Output::new("eDP-1", screen, 1000).with_work_area(crate::geometry::work_area_for(
            screen,
            &[crate::geometry::Exclusive {
                edge: crate::geometry::Edge::Bottom,
                size,
            }],
        ))
    }

    /// A shell surface mapped by the trusted shell and placed exactly where
    /// `shell_surface_placement` would put it — no repair in between.
    fn shell_surface(
        service: &mut WindowService<FakeCache>,
        layer: Layer,
        geometry: Rect,
    ) -> WindowId {
        let id = service.on_toplevel_mapped(
            2,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            String::new(),
            geometry,
        );
        service
            .set_layer(id, layer, ClientKind::TrustedShell)
            .expect("the trusted shell may claim its own layer");
        service.set_geometry(id, geometry);
        id
    }

    /// The Home Surface fills its output, so its tag strip sits under any bottom bar and
    /// `is_recoverable` is false for it by construction. Repairing it anyway lifted Home by the
    /// bar's height, which `apply_surface_claims` then undid — the flicker. Recovery is a
    /// client-window property; a shell surface is placed, never repaired.
    #[test]
    fn a_bottom_zone_does_not_repair_the_home_surface() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let full = outputs()[0].logical;
        let home = shell_surface(&mut service, Layer::HomeSurface, full);

        let changed = service.on_outputs_changed(vec![output_with_a_bottom_bar(48)]);

        assert!(
            !changed.iter().any(|(id, _)| *id == home),
            "Home was repaired: {changed:?}"
        );
        assert_eq!(service.window(home).unwrap().geometry, full);
    }

    /// The bar holds the very zone that makes its own strip unreachable, so a repair pushed it a
    /// second bar-height off its edge (1032 → 984) and the next claim put it back at 1032.
    #[test]
    fn a_bar_holding_its_own_zone_is_not_pushed_off_its_edge() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let placed = Rect::new(0, 1080 - 48, 1920, 48);
        let bar = shell_surface(&mut service, Layer::ShellOverlay, placed);

        // Twice: a repair loop is only visible once the second pass has something stale to chew.
        for _ in 0..2 {
            let changed = service.on_outputs_changed(vec![output_with_a_bottom_bar(48)]);
            assert!(
                !changed.iter().any(|(id, _)| *id == bar),
                "the bar was repaired: {changed:?}"
            );
        }
        assert_eq!(service.window(bar).unwrap().geometry, placed);
    }

    /// A maximized window draws no tags while a bar holds its output's bottom edge — its
    /// controls are the bar's — and draws them again the moment the zone is gone.
    #[test]
    fn a_maximized_window_has_no_frame_only_while_a_bottom_zone_is_claimed() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        assert!(service.has_frame(id));
        service.set_maximized(id, true);
        assert!(service.has_frame(id), "no zone: the tags stay");
        service.on_outputs_changed(vec![output_with_a_bottom_bar(48)]);
        assert!(!service.has_frame(id), "the bar holds the controls");
        assert_eq!(
            service.client_area(id),
            service.window(id).map(|window| window.geometry),
            "no frame: the client is the whole maximized geometry"
        );
        service.on_outputs_changed(outputs());
        assert!(
            service.has_frame(id),
            "the zone is released: the tags come back"
        );
        service.set_maximized(id, false);
        assert!(service.has_frame(id));
        service.set_fullscreen(id, true);
        assert!(!service.has_frame(id), "fullscreen never has a frame");
    }

    #[test]
    fn destroying_a_window_mid_resize_leaves_nothing_behind() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.begin_resize(
            id,
            Point { x: 500, y: 400 },
            crate::ResizeEdges::BOTTOM_RIGHT,
        );
        service.on_toplevel_destroyed(id);
        assert!(!service.is_resizing(id));
        assert_eq!(service.update_resize(Point { x: 0, y: 0 }), None);
        assert_eq!(service.finish_resize(), None);
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
        assert!(
            !service.is_visible(home),
            "Home cannot blend through client transparency"
        );
        service.set_home_revealed(true);
        assert!(service.is_visible(home));
        service.set_home_revealed(false);
        assert!(!service.is_visible(home));
    }

    #[test]
    fn shell_window_lists_never_feed_shell_surfaces_back_to_the_shell() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let app = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            "Editor".into(),
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

        assert_eq!(service.cycle_focus(None, true), Some(app));
        assert!(service.recovery_affordance(home).is_none());

        let listed: Vec<WindowId> = service
            .shell_toplevels()
            .into_iter()
            .map(|window| window.window_id)
            .collect();
        assert_eq!(listed, vec![app]);
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

        /// Whatever the topology does, a topology change hands back client windows and only
        /// client windows. A shell surface in that list is the bar flicker: its placement comes
        /// from `shell_surface_placement`, and a repair fighting that placement is two writers
        /// on one rectangle.
        #[test]
        fn a_topology_change_returns_client_windows_only(
            first in crate::test_support::arb_topology(),
            second in crate::test_support::arb_topology(),
            window in arb_window(),
        ) {
            let mut service = WindowService::new(FakeCache::default(), first.clone());
            service.on_toplevel_mapped(1, ClientKind::Application, WindowClaims::default(), String::new(), window);
            let full = first.first().map_or(Rect::new(0, 0, 1920, 1080), |output| output.logical);
            shell_surface(&mut service, Layer::HomeSurface, full);
            shell_surface(&mut service, Layer::ShellOverlay, Rect::new(full.x, full.bottom() - 48, full.width, 48));
            shell_surface(&mut service, Layer::Notification, Rect::new(full.x, full.y, 300, 80));

            for topology in [second, first] {
                for (id, _) in service.on_outputs_changed(topology) {
                    prop_assert!(service.is_client_window(id), "{id:?} is a shell surface");
                }
            }
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

    // ---- Window groups -----------------------------------------------------------------

    /// Three windows on one output, none of them grouped yet.
    fn service_with(count: u32) -> (WindowService<FakeCache>, Vec<WindowId>) {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let ids = (0..count)
            .map(|index| {
                let offset = 40 * i32::try_from(index).unwrap_or(0);
                service.on_toplevel_mapped(
                    index + 1,
                    ClientKind::Application,
                    WindowClaims::default(),
                    format!("window {index}"),
                    Rect::new(200 + offset, 200 + offset, 300, 200),
                )
            })
            .collect();
        (service, ids)
    }

    #[test]
    fn grouping_two_loose_windows_makes_one_group_of_exactly_them() {
        let (mut service, ids) = service_with(3);
        let group = service.group(ids[0], ids[1]).expect("two client windows");
        assert_eq!(service.group_members(group), vec![ids[0], ids[1]]);
        assert_eq!(service.group_of(ids[2]), None);
        assert_eq!(service.groups().len(), 1);
        assert_eq!(
            service.group_record(group).unwrap().accent,
            crate::group::accent_for(group)
        );
    }

    /// Merging: `a` is the anchor, so its group's id, name and accent survive and `b`'s members
    /// all move across.
    #[test]
    fn merging_two_groups_keeps_the_anchors_identity() {
        let (mut service, ids) = service_with(4);
        let first = service.group(ids[0], ids[1]).unwrap();
        let second = service.group(ids[2], ids[3]).unwrap();
        service.set_group_name(ids[0], Some("Writing".into()));
        assert_ne!(first, second);

        let survivor = service.group(ids[0], ids[2]).unwrap();
        assert_eq!(survivor, first);
        assert_eq!(service.groups().len(), 1);
        assert_eq!(service.group_members(first), ids);
        assert_eq!(
            service.group_record(first).unwrap().display_name(),
            "Writing"
        );
    }

    /// A window with no group joining one that has a group joins *that* group rather than
    /// dissolving it — the one case where the anchor's identity does not survive, because the
    /// anchor has none.
    #[test]
    fn a_loose_anchor_joins_the_group_the_other_window_is_already_in() {
        let (mut service, ids) = service_with(3);
        let group = service.group(ids[1], ids[2]).unwrap();
        assert_eq!(service.group(ids[0], ids[1]), Some(group));
        assert_eq!(service.group_members(group), ids);
        assert_eq!(service.groups().len(), 1);
    }

    #[test]
    fn a_group_dissolves_when_leaving_would_leave_one_member() {
        let (mut service, ids) = service_with(3);
        let group = service.group(ids[0], ids[1]).unwrap();
        service.group(ids[0], ids[2]).unwrap();
        assert_eq!(service.ungroup(ids[2]), Some(group));
        assert_eq!(service.group_members(group).len(), 2);

        assert_eq!(service.ungroup(ids[1]), Some(group));
        assert!(service.groups().is_empty());
        assert_eq!(service.group_of(ids[0]), None);
    }

    #[test]
    fn closing_the_second_to_last_member_dissolves_the_group() {
        let (mut service, ids) = service_with(2);
        let group = service.group(ids[0], ids[1]).unwrap();
        service.on_toplevel_destroyed(ids[1]);
        assert!(service.groups().is_empty());
        assert_eq!(service.group_of(ids[0]), None);
        assert!(service.group_outline(group).is_none());
    }

    /// Rule 3: activating one member raises every member, contiguously — nothing outside the
    /// group ends up stacked between two of its windows — with the activated one on top.
    #[test]
    fn activating_a_member_raises_the_whole_group_contiguously() {
        let (mut service, ids) = service_with(3);
        service.group(ids[0], ids[2]).unwrap();
        service.activate(ids[1]);
        assert_eq!(service.stacking_order()[0], ids[1]);

        service.activate(ids[2]);
        let order = service.stacking_order();
        assert_eq!(order, vec![ids[2], ids[0], ids[1]]);
    }

    #[test]
    fn shelving_a_group_parks_every_member_under_one_shelf_rank() {
        let (mut service, ids) = service_with(3);
        service.group(ids[0], ids[1]).unwrap();
        let shelved = service.shelve_group(ids[1]);
        assert_eq!(shelved, vec![ids[0], ids[1]]);
        let ranks: Vec<Option<u64>> = shelved
            .iter()
            .map(|id| service.window(*id).unwrap().shelf_rank)
            .collect();
        assert_eq!(ranks[0], ranks[1]);
        assert!(service.window(ids[2]).unwrap().shelf_rank.is_none());

        let back = service.unshelve_group(ids[0]);
        assert_eq!(back.len(), 2);
        assert!(service.windows().all(|window| !window.shelved));
    }

    /// `Alt+Tab` stays inside the group; a loose window cycles the loose set and never reaches a
    /// grouped one.
    #[test]
    fn cycling_within_a_group_never_leaves_it_and_the_loose_set_is_its_own_scope() {
        let (mut service, ids) = service_with(4);
        service.group(ids[0], ids[1]).unwrap();
        assert_eq!(service.cycle_within_group(Some(ids[0]), true), Some(ids[1]));
        assert_eq!(service.cycle_within_group(Some(ids[1]), true), Some(ids[0]));

        let loose = service.cycle_within_group(Some(ids[2]), true);
        assert_eq!(loose, Some(ids[3]));
        assert_eq!(service.cycle_within_group(Some(ids[3]), true), Some(ids[2]));
    }

    /// With nothing focused there is no group to be inside of, and the switcher must still land
    /// somewhere: it starts from the top of everything, exactly as it did before groups existed.
    #[test]
    fn cycling_with_nothing_focused_starts_from_the_top_of_every_window() {
        let (mut service, ids) = service_with(2);
        service.group(ids[0], ids[1]).unwrap();
        assert!(service.windows().all(|window| !window.activated));
        assert_eq!(
            service.cycle_within_group(None, true),
            service.cycle_focus(None, true)
        );
        assert!(service.cycle_within_group(None, true).is_some());
    }

    /// `Super+Tab` moves between groups and lands on each group's last active member; the loose
    /// set counts as one group.
    #[test]
    fn cycling_groups_lands_on_each_groups_last_active_member() {
        let (mut service, ids) = service_with(4);
        service.group(ids[0], ids[1]).unwrap();
        service.activate(ids[1]);
        service.activate(ids[3]);

        let targets = service.group_cycle_targets();
        assert_eq!(
            targets.len(),
            2,
            "one group plus the loose set: {targets:?}"
        );
        let next = service.cycle_groups(Some(ids[3]), true).unwrap();
        assert_eq!(next, ids[1], "back to the group's last active member");
        let back = service.cycle_groups(Some(next), true).unwrap();
        assert_eq!(service.group_of(back), None, "and on to the loose set");
    }

    #[test]
    fn placing_beside_puts_the_window_on_the_targets_free_side_and_leaves_neighbours_alone() {
        let (mut service, ids) = service_with(2);
        let target = service.window(ids[0]).unwrap().geometry;
        let placed = service.place_beside(ids[1], ids[0]).expect("it moved");
        assert_eq!(placed.x, target.right(), "edge to edge on the right");
        assert!(placed.intersection(target).is_none());
        assert!(crate::is_recoverable(placed, service.outputs()));
        // Already beside it: nothing moves a second time.
        assert_eq!(service.place_beside(ids[1], ids[0]), None);
    }

    #[test]
    fn the_group_outline_is_the_union_of_the_unshelved_members() {
        let (mut service, ids) = service_with(2);
        let group = service.group(ids[0], ids[1]).unwrap();
        service.set_geometry(ids[0], Rect::new(100, 100, 200, 200));
        service.set_geometry(ids[1], Rect::new(400, 300, 100, 100));
        assert_eq!(
            service.group_outline(group),
            Some(Rect::new(100, 100, 400, 300))
        );

        service.shelve(ids[1]);
        assert_eq!(
            service.group_outline(group),
            Some(Rect::new(100, 100, 200, 200)),
            "a parked member is not on the workspace and so is not inside the outline"
        );
    }

    /// The shell's only view of a group is this event's payload, so it carries everything a
    /// surface needs to draw the same group the same way: id, name and accent.
    #[test]
    fn the_shell_view_of_a_grouped_window_carries_id_name_and_accent() {
        let (mut service, ids) = service_with(2);
        let group = service.group(ids[0], ids[1]).unwrap();
        service.set_group_name(ids[0], Some("Writing".into()));
        let toplevels = service.shell_toplevels();
        let info = toplevels[0].group.clone().expect("a grouped window");
        assert_eq!(info.group_id, group.0);
        assert_eq!(info.name, "Writing");
        assert_eq!(info.accent, crate::group::accent_for(group));
        assert_eq!(
            toplevels[1].group.as_ref().map(|g| g.group_id),
            Some(group.0)
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// Rule 3's safety argument: raising a group is a stacking change and nothing else, so
        /// no sequence of activations can move a window a single pixel.
        #[test]
        fn raising_a_group_never_changes_geometry(steps in prop::collection::vec(0usize..4, 1..40)) {
            let (mut service, ids) = service_with(4);
            service.group(ids[0], ids[2]).unwrap();
            let before: Vec<Rect> = ids.iter().map(|id| service.window(*id).unwrap().geometry).collect();
            for step in steps {
                service.activate(ids[step]);
            }
            for (id, geometry) in ids.iter().zip(before) {
                prop_assert_eq!(service.window(*id).unwrap().geometry, geometry);
            }
        }

        /// Membership is not geometry: shelving, maximizing, arranging and losing an output
        /// change where windows are, never who they are grouped with. And the two structural
        /// invariants hold throughout: no window is in two groups (membership lives on the
        /// window, so this is by construction) and a live group always has two or more members.
        #[test]
        fn membership_survives_shelving_maximizing_arranging_and_hotplug(
            steps in prop::collection::vec(0u8..7, 1..30),
            topology in crate::test_support::arb_topology(),
        ) {
            let (mut service, ids) = service_with(3);
            let group = service.group(ids[0], ids[1]).unwrap();
            for step in steps {
                match step {
                    0 => { service.shelve(ids[0]); }
                    1 => { service.unshelve(ids[0]); }
                    2 => { service.set_maximized(ids[1], true); }
                    3 => { service.set_maximized(ids[1], false); }
                    4 => { service.arrange_windows(crate::ArrangementPreset::Halves); }
                    5 => { service.restore_arrangement(); }
                    _ => { service.on_outputs_changed(topology.clone()); }
                }
                prop_assert_eq!(service.group_of(ids[0]), Some(group));
                prop_assert_eq!(service.group_of(ids[1]), Some(group));
                prop_assert_eq!(service.group_of(ids[2]), None);
                for (id, record) in service.groups() {
                    prop_assert!(service.group_members(*id).len() >= 2, "{record:?} is a group of one");
                    prop_assert!(service.group_members(*id).contains(&record.last_active));
                }
            }
        }

        /// Whatever a group is asked to do, every member stays reachable: `place_beside` and
        /// `move_group` both end in `repair_window`, on every topology.
        #[test]
        fn placing_and_moving_a_group_keeps_every_member_recoverable(
            topology in crate::test_support::arb_topology(),
            destination in arb_window(),
        ) {
            let mut service = WindowService::new(FakeCache::default(), topology);
            let a = service.on_toplevel_mapped(1, ClientKind::Application, WindowClaims::default(), String::new(), Rect::new(100, 100, 400, 300));
            let b = service.on_toplevel_mapped(2, ClientKind::Application, WindowClaims::default(), String::new(), Rect::new(900, 500, 500, 400));
            service.group(a, b).unwrap();
            service.place_beside(b, a);
            for id in [a, b] {
                prop_assert!(crate::is_recoverable(service.window(id).unwrap().geometry, service.outputs()));
            }
            for (_, geometry) in service.move_group(a, destination) {
                prop_assert!(crate::is_recoverable(geometry, service.outputs()));
            }
        }

        /// Rule 6 is "beside", not "on top of": whenever the target's output has room for the
        /// window on any side, the placed window does not overlap the target.
        #[test]
        fn placing_beside_never_overlaps_the_target(
            window in arb_window(),
            target in arb_window(),
        ) {
            let outputs = outputs();
            let area = outputs[0].usable();
            let horizontal = window.height <= area.height
                && ((area.right() - target.right()).max(0) >= window.width
                    || (target.x - area.x).max(0) >= window.width);
            let vertical = window.width <= area.width
                && ((area.bottom() - target.bottom()).max(0) >= window.height
                    || (target.y - area.y).max(0) >= window.height);
            prop_assume!(horizontal || vertical);

            let placed = crate::place_beside(window, target, &outputs);
            prop_assert_eq!(placed.width, window.width);
            prop_assert_eq!(placed.height, window.height);
            prop_assert!(placed.intersection(target).is_none(), "{placed:?} overlaps {target:?}");
            prop_assert!(crate::is_recoverable(placed, &outputs));
        }
    }
}
