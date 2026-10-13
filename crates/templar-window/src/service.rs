//! Window records, identity cache boundary, drag handling, layer membership, the Window Shelf
//! and the fullscreen/modal escape route.

use crate::taskfield::{self, TaskFilter, TaskfieldState};
use crate::{
    geometry::{
        Corner, Output, Point, RecoveryAffordance, Rect, ResizeEdges, TabPolicy, WindowLayout,
        bounding_rect, choose_target_output, recovery_affordance, repair_window, resize_rect,
    },
    group::{GroupId, GroupRecord, accent_for},
    layers::{ClientKind, Layer, LayerError, LayerStack, StackPosition},
    shell::{GroupInfo, ShellToplevel},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use templar_types::{AppId, InstanceId, WindowClaims};

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

/// What a toplevel means to window-management policy, independently of where it is drawn.
///
/// A role is deliberately not a [`Layer`]: moving a window in Z-order, promoting fullscreen
/// content over shell chrome, or pinning an application must never change whether Alt-Tab and
/// the taskbar consider it an application.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WindowRole {
    Application,
    Shell,
    Notification,
    Overlay,
    LockScreen,
}

/// What the compositor is currently presenting, and the one thing every "is this on screen"
/// question consults.
///
/// It replaces the reveal flag the service used to carry and the `show_all_windows` slot the
/// compositor carried beside it, so a new mode cannot be added without answering how it relates to
/// the others. The lock screen and the recovery overlay are deliberately *not* here: they are
/// [`crate::Layer`]s, and a layer already outranks everything by construction.
///
/// The invariant that replaces every fullscreen special case:
///
/// > **A fullscreen window covers passive chrome only while [`Presentation::Normal`].** Passive
/// > chrome (the island, Home, tags) yields to fullscreen; explicit navigation (Taskfield, the
/// > lock screen, recovery) yields to nothing — "I asked one application to occupy the display"
/// > and "I asked to see my windows" are different requests, and the second must always be
/// > honoured in one deterministic step.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Presentation {
    #[default]
    Normal,
    /// "Go home": every client window is hidden so the Home Surface shows through. A *view* state
    /// only — no record's geometry, `shelved` or `activated` changes because of it.
    HomeReveal,
    /// The window overview. The windows are drawn at their cards' rectangles instead of their real
    /// ones; no client is reconfigured and no record is touched, so leaving restores the previous
    /// scene exactly. Everything about it is thrown away on exit — Taskfield is a view, not a place.
    Taskfield(TaskfieldState),
}

impl CachedIdentity {
    pub const UNKNOWN: Self = Self {
        app_id: None,
        instance_id: None,
        verified: false,
        claim_mismatch: false,
    };
}

/// Boundary between the render thread and `appd`. Implemented in `templar-comp-integration`.
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

/// User-selected application stacking, independent of protocol layer authorization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowStacking {
    AlwaysOnTop,
    #[default]
    Normal,
    AlwaysOnBottom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRecord {
    pub id: WindowId,
    pub role: WindowRole,
    pub peer_pid: u32,
    pub claims: WindowClaims,
    pub identity: CachedIdentity,
    pub title: String,
    pub geometry: Rect,
    /// Whether the client currently presents this toplevel. Temporary protocol unmaps retain
    /// the record and [`WindowId`]; only destruction removes them.
    pub mapped: bool,
    pub activated: bool,
    /// Monotonic application-activation order. Visual raises have their own counter in
    /// [`LayerStack`] and never alter this value.
    pub last_activated: u64,
    pub fullscreen: bool,
    /// Holds an exclusive input grab (a dialog, a permission prompt). See
    /// [`WindowService::escape`].
    pub modal: bool,
    /// Transient presentation relationship only: never inherits identity, grants or input focus.
    pub transient_parent: Option<WindowId>,
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
    /// The geometry to return to on unfullscreen. A slot of its own rather than sharing
    /// [`Self::restore_geometry`]: a window can be maximized *and* fullscreen, and one slot
    /// would have the fullscreen entry overwrite the floating geometry the unmaximize needs.
    pub fullscreen_restore: Option<Rect>,
    /// The window group this window belongs to, if any (`crate::group`). Membership lives here,
    /// on the window, so no window can be in two groups.
    pub group: Option<GroupId>,
    /// In-session user stacking preference. Never changes identity, focus, or assigned layer.
    pub stacking: WindowStacking,
}

impl WindowRecord {
    /// Whether this window is filling its output — maximized or fullscreen, and on screen to do
    /// it. The one predicate behind every "there is a window over the bottom edge" rule: the
    /// window wears no exterior tabs ([`WindowService::has_frame`]), its own title and controls
    /// are shown overlaid inside it on approach instead, and the shell's machine cluster is not
    /// there at all.
    ///
    /// Maximized counts because the bar reserves nothing (a zone of 0), so a maximized window's
    /// work area *is* the whole output: "maximized" and "fullscreen" differ in what the client
    /// was told, not in how much of the screen is left over.
    #[must_use]
    pub fn covers_screen(&self) -> bool {
        self.mapped && !self.shelved && (self.fullscreen || self.maximized)
    }
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

/// One deadline per transition, preserved across motion until entry or exit completes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TabPending {
    reveal_at: Option<u64>,
    exit_at: Option<u64>,
}

/// What firing the tab grace deadlines at `now` decided for one window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabDeadlineFired {
    /// The reveal grace elapsed with the pointer still approaching: show the controls.
    Reveal(WindowId),
    /// The exit grace elapsed with the pointer still gone: hide the controls, keep the
    /// title and the last corner.
    Exit(WindowId),
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
    activation_serial: u64,
    drag: Option<DragState>,
    resize: Option<ResizeState>,
    layers: LayerStack<WindowId>,
    /// Activation-grant tokens already consumed by a successful `Activate`, keyed to their
    /// expiry so they can be pruned once they could not be replayed anyway. See
    /// `try_spend_activation_token`.
    spent_activation_tokens: HashMap<String, u64>,
    /// What is being presented ([`Presentation`]): the ordinary scene, a "go home" reveal, or
    /// Taskfield. A *view* state only — no record's geometry, `shelved` or `activated` changes
    /// because of it, and the governor is never told about it — so a shell that dies mid-mode, or
    /// a stray press, can never leave a window anywhere but exactly where it was.
    presentation: Presentation,
    /// The geometry and maximize state every eligible window had when the first arrangement
    /// preset was requested (`arrange_windows`): preset switches are auditioned over this one
    /// baseline, and `restore_arrangement` returns surviving windows exactly to it.
    arrangement_baseline: Option<Vec<(WindowId, Rect, bool)>>,
    /// The one tab policy every window shares: arrangement, follow, visibility, defaults.
    tab_policy: TabPolicy,
    /// Each window's active tab corner. Survives park/restore, maximize/fullscreen and
    /// regrouping — it is per-window state, not derived from transient geometry.
    tab_corners: HashMap<WindowId, Corner>,
    /// Windows whose tabs are frozen: the pointer is aiming at a visible title/control, a
    /// button is down on one, or a move/resize/menu is in flight. Set on press/aim/grab
    /// start, cleared from the grab's own termination path — never by timeout — so Close
    /// cannot flee the pointer mid-reach.
    tab_latched: HashSet<WindowId>,
    /// Windows whose hover controls are currently revealed. Reveal changes nothing else:
    /// no focus, no raise, no action fires.
    tab_revealed: HashSet<WindowId>,
    /// Monotonic entry/exit deadlines, fired by the compositor's shared dwell wakeup.
    /// Further motion never postpones a transition or queues another timer.
    tab_pending: HashMap<WindowId, TabPending>,
    /// The output whose bottom edge the pointer is currently resting on, under a window that
    /// fills it (`set_peek_output`). While set, [`Self::stacking_order`]'s fullscreen exception is
    /// suspended on that output — and the compositor tells the shell either way
    /// (`templar_shell_manager_v2.bar_peeked`), because under a *maximized* window there is no
    /// exception to suspend and the event is the whole of the reveal.
    peek_output: Option<String>,
    /// Whether an `Alt+Tab` / `` Alt+` `` walk is showing (`set_switch_walk`). The walk itself —
    /// its frozen ring and selection — is the compositor's input state; this is only the part
    /// [`Self::is_visible`] needs, because the shell draws that walk on a `Layer::Presentation`
    /// surface exactly as it draws Taskfield, and a surface this predicate hides is never drawn.
    switch_walk: bool,
    /// The one focused shell popup; promotion never changes client activation.
    focused_popup: Option<WindowId>,
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
            activation_serial: 0,
            drag: None,
            resize: None,
            layers: LayerStack::new(),
            spent_activation_tokens: HashMap::new(),
            presentation: Presentation::Normal,
            arrangement_baseline: None,
            tab_policy: TabPolicy::default(),
            tab_corners: HashMap::new(),
            tab_latched: HashSet::new(),
            tab_revealed: HashSet::new(),
            tab_pending: HashMap::new(),
            peek_output: None,
            switch_walk: false,
            focused_popup: None,
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
            .filter(|window| window.mapped && self.is_client_window(window.id))
            .count();
        let geometry =
            crate::geometry::place_new_window(requested_geometry, existing_clients, &self.outputs);

        self.windows.insert(
            id,
            WindowRecord {
                id,
                role: WindowRole::Application,
                peer_pid,
                claims: claims.clone(),
                identity,
                title,
                geometry,
                mapped: true,
                activated: false,
                last_activated: 0,
                fullscreen: false,
                modal: false,
                transient_parent: None,
                decoration: Decoration::ServerSide,
                shelved: false,
                shelf_rank: None,
                maximized: false,
                restore_geometry: None,
                fullscreen_restore: None,
                stacking: WindowStacking::Normal,
                group: None,
            },
        );
        // A window the user just opened must be seen: any presentation hiding or shrinking the
        // scene ends the moment something new arrives, rather than hiding the new window too.
        // Trusted-shell overlays (Taskfield labels, the island, Home) map through here as well
        // and must not cancel the mode they are drawing — their role is assigned after this call.
        if client != ClientKind::TrustedShell {
            self.presentation = Presentation::Normal;
        }

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
        self.tab_corners.insert(id, self.tab_policy.default_corner);
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

    /// Change the semantic role declared for a toplevel. This changes no layer, geometry,
    /// visibility or activation history; callers that also need a visual layer set it through
    /// [`Self::set_layer`].
    pub fn set_role(&mut self, window_id: WindowId, role: WindowRole) -> bool {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return false;
        };
        window.role = role;
        true
    }

    /// Record a protocol map or temporary unmap without changing the toplevel's identity.
    /// Destruction remains [`Self::on_toplevel_destroyed`]'s job.
    pub fn set_mapped(&mut self, window_id: WindowId, mapped: bool) -> bool {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return false;
        };
        if window.mapped == mapped {
            return false;
        }
        window.mapped = mapped;
        if !mapped && self.focused_popup == Some(window_id) {
            self.focused_popup = None;
        }
        if !mapped {
            window.activated = false;
            // Unmapped windows hold no latch, reveal or deadline: nothing stranded across
            // the unmap, and the stored corner is still there on remap.
            self.tab_latched.remove(&window_id);
            self.tab_revealed.remove(&window_id);
            self.tab_pending.remove(&window_id);
        }
        true
    }

    /// `xdg_toplevel.set_fullscreen` / `unset_fullscreen`: fill the *whole* output —
    /// [`Output::logical`], not [`Output::usable`], because a fullscreen window covers the bar's
    /// exclusive zone too — remembering the geometry to return to in
    /// [`WindowRecord::fullscreen_restore`]. Returns the geometry to configure, or `None` for an
    /// unknown window or a no-op.
    ///
    /// The result is *not* recoverable in [`crate::geometry::is_recoverable`]'s sense, and must
    /// not be: that predicate measures a window's recovery band against `usable()`, and a
    /// fullscreen window keeps no band on screen ([`Self::has_frame`]). Its recovery route is
    /// the escape chord ([`Self::escape`]), which restores through this same slot, and
    /// [`Self::on_outputs_changed`] never repairs it.
    pub fn set_fullscreen(&mut self, window_id: WindowId, fullscreen: bool) -> Option<Rect> {
        let outputs = &self.outputs;
        let window = self.windows.get_mut(&window_id)?;
        if window.fullscreen == fullscreen {
            return None;
        }
        window.fullscreen = fullscreen;
        if fullscreen {
            window.fullscreen_restore = Some(window.geometry);
            window.geometry = fullscreen_geometry(window.geometry, outputs);
        } else {
            let restore = window.fullscreen_restore.take().unwrap_or(window.geometry);
            window.geometry = if window.maximized {
                maximized_geometry(restore, outputs)
            } else {
                repair_window(restore, outputs)
            };
        }
        let geometry = window.geometry;
        // Fullscreen owns its complete client area: retire hover state so neither a
        // pending reveal nor an old control press survives entry or restore.
        self.tab_latched.remove(&window_id);
        self.tab_revealed.remove(&window_id);
        self.tab_pending.remove(&window_id);
        Some(geometry)
    }

    pub fn set_modal(&mut self, window_id: WindowId, modal: bool) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.modal = modal;
        }
    }

    /// Set a compositor-validated transient relationship between ordinary application windows.
    /// The adapter checks protocol ownership; this boundary rejects cycles and protected roles.
    pub fn set_transient_parent(&mut self, child: WindowId, parent: Option<WindowId>) -> bool {
        if !self.is_client_window(child)
            || !matches!(self.layer_of(child), Some(Layer::Normal | Layer::AppOnTop))
        {
            return false;
        }
        if let Some(parent) = parent {
            if !self.is_client_window(parent)
                || !matches!(self.layer_of(parent), Some(Layer::Normal | Layer::AppOnTop))
            {
                return false;
            }
            let mut ancestor = Some(parent);
            while let Some(id) = ancestor {
                if id == child {
                    return false;
                }
                ancestor = self
                    .windows
                    .get(&id)
                    .and_then(|window| window.transient_parent);
            }
        }
        self.windows.get_mut(&child).unwrap().transient_parent = parent;
        true
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
        if self.focused_popup == Some(window_id) {
            self.focused_popup = None;
        }
        // Tab state is per-window and dies with it: no latch, reveal or deadline may
        // outlive its window into a grab, a wakeup or a reused id.
        self.tab_corners.remove(&window_id);
        self.tab_latched.remove(&window_id);
        self.tab_revealed.remove(&window_id);
        self.tab_pending.remove(&window_id);
        for window in self.windows.values_mut() {
            if window.transient_parent == Some(window_id) {
                window.transient_parent = None;
            }
        }
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
        // A window that is gone has no card. The rest of the frozen layout stays where it is:
        // relayouting under the pointer is the one thing the mode must not do.
        if let Presentation::Taskfield(state) = &mut self.presentation {
            for section in &mut state.sections {
                section.cards.retain(|card| card.window != window_id);
            }
            state.sections.retain(|section| !section.cards.is_empty());
            state.selected = state.selected.filter(|id| *id != window_id);
            if state.sections.is_empty() {
                self.presentation = Presentation::Normal;
            }
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
        if self.outputs != outputs {
            self.clear_tab_interaction();
        }
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
            let repaired = if window.fullscreen {
                // A fullscreen window *is* its output rather than a rectangle on one: it follows
                // whichever output is now nearest, and is never nudged to keep a recovery band on
                // screen, because it has no band to keep (`has_frame`). Repairing it here is what
                // would pull it off the full output the moment anything changed the topology.
                fullscreen_geometry(window.geometry, &self.outputs)
            } else if window.maximized {
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
        let geometry = window.geometry;
        Some(geometry)
    }

    /// The windows an arrangement preset applies to: mapped client windows that are neither
    /// shelved nor fullscreen — the activated one first (it takes the main slot of Focus and
    /// MainAndStack), then by id, so the result is deterministic in the service state.
    fn arrangement_eligible(&self) -> Vec<WindowId> {
        let mut eligible: Vec<(bool, WindowId)> = self
            .windows
            .values()
            .filter(|window| {
                self.is_client_window(window.id)
                    && window.mapped
                    && !window.shelved
                    && !window.fullscreen
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
    /// not a client window hidden behind a "go home" reveal. The overlay layers are never hidden
    /// by a reveal — they are what it reveals. The one predicate `space_sync`, the hit-test and
    /// the chrome all consult, so the three cannot disagree about what is on screen. Unknown
    /// windows are not visible.
    ///
    /// `Layer::HomeSurface` is the bottom layer and is *visible by default*
    /// ([`Self::home_is_covered`]): it hides only while a visible fullscreen or maximized client
    /// window covers its output, or is shown regardless while a reveal is in effect. Home used to
    /// be hidden unless revealed, which is what made a fresh login a black screen until the user
    /// pressed Super. The accepted consequence is that a translucent client window now blends
    /// with Home rather than with the clear colour.
    pub fn is_visible(&self, window_id: WindowId) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if !window.mapped {
            return false;
        }
        // `Layer::Presentation` exists only while a mode it draws does — Taskfield, or an
        // `Alt+Tab` walk: the shell's surface is configured to the whole output and reserves
        // nothing, so leaving it mapped outside them would put an invisible sheet over the
        // desktop. Here, in the one predicate `space_sync`, the hit-test and the chrome all
        // consult, so the three cannot disagree about it.
        if self.layer_of(window_id) == Some(Layer::Presentation) {
            return self.switch_walk || matches!(self.presentation, Presentation::Taskfield(_));
        }
        if matches!(self.presentation, Presentation::Taskfield(_)) {
            // The overview is a picture of cards, not of the ordinary scene: an application
            // window is on screen only if it has a card (including a mapped shelved one), Home
            // stays the backdrop under the scrim, and everything else — the island, a filtered-
            // out window — is out. Drawing a non-card at its real size is how a filter used to
            // leave full-size windows sitting on the overview.
            if window.role == WindowRole::Application {
                return self.presentation_rect(window_id).is_some();
            }
            return self.layer_of(window_id) == Some(Layer::HomeSurface);
        }
        if window.shelved {
            return false;
        }
        if self.layer_of(window_id) == Some(Layer::HomeSurface) {
            return self.home_revealed() || !self.home_is_covered(window.geometry);
        }
        if self.home_revealed() {
            return !matches!(
                self.layer_of(window_id),
                Some(Layer::Normal | Layer::AppOnTop)
            );
        }
        true
    }

    /// Whether a visible fullscreen or maximized client window covers the output the Home
    /// surface at `home` is on — the one thing that hides Home.
    ///
    /// "Covers" is per output rather than per rectangle: a maximized window fills its output's
    /// *usable* area, which is the output minus whatever the bar reserved, so an intersection
    /// test against Home's rectangle would answer "not quite" for exactly the case this is for.
    /// Which output each is on comes from [`choose_target_output`] on the two geometries, because
    /// a `WindowRecord` does not carry an output; with no outputs at all there is one screen by
    /// definition and the question does not arise.
    ///
    /// A shelved window never counts (it is off the screen), and neither does a shell surface —
    /// only `Layer::Normal`/`Layer::AppOnTop` can cover Home.
    fn home_is_covered(&self, home: Rect) -> bool {
        // Passive chrome yields to a fullscreen window only in the ordinary scene: in Taskfield
        // that window is a card, and nothing it used to cover is covered any more.
        if !matches!(self.presentation, Presentation::Normal) {
            return false;
        }
        let home_output = (!self.outputs.is_empty())
            .then(|| choose_target_output(home, &self.outputs).name.as_str());
        self.windows.values().any(|window| {
            (window.maximized || window.fullscreen)
                && window.mapped
                && !window.shelved
                && self.is_client_window(window.id)
                && home_output.is_none_or(|name| {
                    choose_target_output(window.geometry, &self.outputs).name == name
                })
        })
    }

    /// Whether a "go home" reveal is in effect (`WindowCommand::RevealHome`).
    #[must_use]
    pub fn home_revealed(&self) -> bool {
        matches!(self.presentation, Presentation::HomeReveal)
    }

    /// Start or end a "go home" reveal. Returns whether anything changed. Purely a view state —
    /// see the field's doc: nothing about any window is touched. Ending one returns to the
    /// ordinary scene, which is also how it ends a Taskfield left open behind it: `Super` means
    /// Home, from wherever the user was.
    pub fn set_home_revealed(&mut self, revealed: bool) -> bool {
        let next = if revealed {
            Presentation::HomeReveal
        } else {
            Presentation::Normal
        };
        let changed = self.presentation != next;
        self.presentation = next;
        if changed {
            self.clear_tab_interaction();
        }
        changed
    }

    /// What is being presented.
    #[must_use]
    pub fn presentation(&self) -> &Presentation {
        &self.presentation
    }

    /// Mark an `Alt+Tab` / `` Alt+` `` walk as showing or ended. Returns whether anything changed.
    /// Only `Layer::Presentation`'s visibility depends on it: the shell's switcher surface takes no
    /// input (an empty input region), so showing it moves no pointer or keyboard focus.
    pub fn set_switch_walk(&mut self, active: bool) -> bool {
        let changed = self.switch_walk != active;
        self.switch_walk = active;
        changed
    }

    /// Open Taskfield over `filter`, laying it out once. `false` — and no change — when the filter
    /// selects nothing: Taskfield is a view of windows that exist, so with none there is nothing to
    /// enter and no way to tell that from a black screen.
    pub fn enter_taskfield(&mut self, filter: TaskFilter) -> bool {
        let toplevels = self.shell_toplevels();
        let units = crate::task::units(&toplevels);
        let sections = taskfield::layout(&units, &toplevels, &self.outputs, &filter);
        if sections.is_empty() {
            return false;
        }
        let entered_from = self
            .windows
            .values()
            .find(|window| window.activated && window.role == WindowRole::Application)
            .map(|window| window.id);
        let state = TaskfieldState {
            selected: entered_from.filter(|id| {
                sections
                    .iter()
                    .any(|section| section.cards.iter().any(|card| card.window == *id))
            }),
            entered_from,
            filter,
            query: String::new(),
            sections,
            matching: None,
        };
        let next = Presentation::Taskfield(state);
        let changed = self.presentation != next;
        self.presentation = next;
        if changed {
            self.clear_tab_interaction();
        }
        changed
    }

    /// Leave Taskfield, restoring the ordinary scene exactly. Everything the mode held is dropped:
    /// no card position outlives it.
    pub fn leave_taskfield(&mut self) -> bool {
        if matches!(self.presentation, Presentation::Taskfield(_)) {
            self.presentation = Presentation::Normal;
            return true;
        }
        false
    }

    /// Step the overview's selection, skipping cards the query rejects. Cards do not move.
    pub fn taskfield_step(&mut self, direction: crate::Direction) -> bool {
        let next = match &self.presentation {
            Presentation::Taskfield(state) => {
                taskfield::step_matching(&state.sections, state.selected, direction, |id| {
                    state.matches(id)
                })
            }
            _ => return false,
        };
        self.taskfield_selected(next)
    }

    /// Move Taskfield's selection. Selection highlights; it never moves geometry.
    pub fn taskfield_selected(&mut self, window_id: Option<WindowId>) -> bool {
        let Presentation::Taskfield(state) = &mut self.presentation else {
            return false;
        };
        let changed = state.selected != window_id;
        state.selected = window_id;
        changed
    }

    /// Set Taskfield's query. Matching *dims* non-matches ([`crate::task::matches`]); it never
    /// relayouts, so the card under the pointer does not move as the user types. Selection jumps
    /// to the first match when the current card no longer answers.
    pub fn taskfield_query(&mut self, query: String) -> bool {
        let matching = self.taskfield_matching_set(&query);
        let Presentation::Taskfield(state) = &mut self.presentation else {
            return false;
        };
        let changed = state.query != query || state.matching != matching;
        state.query = query;
        state.matching = matching;
        if let Some(matching) = &state.matching
            && state.selected.is_none_or(|id| !matching.contains(&id))
        {
            let next = state
                .cards()
                .find(|card| matching.contains(&card.window))
                .map(|card| card.window);
            state.selected = next;
        }
        changed
    }

    /// Advance the overflowing section named by `key` by one page. When that section never got
    /// cells (the All layout ran out of the output), this opens the unit as its own filter so
    /// the windows actually fit. `false` when nothing changed.
    pub fn taskfield_overflow(&mut self, key: crate::TaskKey) -> bool {
        let can_page = matches!(
            &self.presentation,
            Presentation::Taskfield(state)
                if state.sections.iter().any(|section| {
                    section.key == key
                        && !section.cells.is_empty()
                        && section.windows.len() > section.cells.len()
                })
        );
        if can_page {
            let geometries: HashMap<WindowId, Rect> = self
                .windows
                .iter()
                .map(|(id, window)| (*id, window.geometry))
                .collect();
            let Presentation::Taskfield(state) = &mut self.presentation else {
                return false;
            };
            let changed = {
                let Some(section) = state.sections.iter_mut().find(|section| section.key == key)
                else {
                    return false;
                };
                taskfield::page(section, |id| geometries.get(&id).copied())
            };
            if changed && state.selected.is_some_and(|id| state.card(id).is_none()) {
                state.selected = state
                    .sections
                    .iter()
                    .find(|section| section.key == key)
                    .and_then(|section| section.cards.first())
                    .map(|card| card.window);
            }
            return changed;
        }
        self.enter_taskfield(TaskFilter::Unit(key))
    }

    /// Page the section the selection is in, or the first overflowing section.
    pub fn taskfield_page_selected(&mut self) -> bool {
        let key = match &self.presentation {
            Presentation::Taskfield(state) => state
                .selected
                .and_then(|id| {
                    state
                        .sections
                        .iter()
                        .find(|section| section.cards.iter().any(|card| card.window == id))
                        .map(|section| section.key.clone())
                })
                .or_else(|| {
                    state
                        .sections
                        .iter()
                        .find(|section| section.overflow > 0)
                        .map(|section| section.key.clone())
                }),
            _ => None,
        };
        key.is_some_and(|key| self.taskfield_overflow(key))
    }

    /// Alpha for a card in the overview: 1.0 when it matches (or the query is empty), dimmed
    /// otherwise. The compositor paints with this; the layout does not.
    #[must_use]
    pub fn taskfield_card_alpha(&self, window_id: WindowId) -> f32 {
        match &self.presentation {
            Presentation::Taskfield(state) if !state.matches(window_id) => 0.35,
            _ => 1.0,
        }
    }

    fn taskfield_matching_set(&self, query: &str) -> Option<HashSet<WindowId>> {
        if query.trim().is_empty() {
            return None;
        }
        let toplevels = self.shell_toplevels();
        let units = crate::task::units(&toplevels);
        let mut matching = std::collections::HashSet::new();
        for toplevel in &toplevels {
            if let Some(unit) = units
                .iter()
                .find(|unit| unit.windows.contains(&toplevel.window_id))
                && crate::task::matches(unit, toplevel, query)
            {
                matching.insert(toplevel.window_id);
            }
        }
        Some(matching)
    }

    /// Where `window_id` is drawn instead of its real geometry, or `None` when it is drawn where it
    /// really is. The renderer *and* the hit-test both ask this, so a card is clicked exactly where
    /// it is seen.
    #[must_use]
    pub fn presentation_rect(&self, window_id: WindowId) -> Option<Rect> {
        match &self.presentation {
            Presentation::Taskfield(state) => state.card(window_id).map(|card| card.rect),
            _ => None,
        }
    }

    /// The windows `Alt+Tab` offers: every protocol-mapped record whose semantic role is
    /// `Application`, **parked (shelved) ones included**, most-recently activated first with the
    /// same tie-break as [`Self::switch_order`]. A parked window keeps its place in the order —
    /// parking is putting a window aside, not forgetting it, so the chord that lists the open
    /// windows must still reach it (activating it unshelves, [`Self::activate`]). No cap: the
    /// HUD scrolls rather than hiding a window that is open.
    #[must_use]
    pub fn alt_tab_order(&self) -> Vec<WindowId> {
        self.application_windows_by_recency(true)
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
        // A gesture owns the tabs for its whole duration: frozen now, released from
        // `finish_drag`, so the title being dragged cannot slide corners mid-move.
        self.tab_latched.insert(window_id);
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

    /// Drag ended: the invariant is enforced once, at the end — and the tab latch taken
    /// for the gesture is released from this same termination path, never by timeout.
    pub fn finish_drag(&mut self) -> Option<(WindowId, Rect)> {
        let drag = self.drag.take()?;
        self.tab_latched.remove(&drag.window_id);
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
        // Same freeze as a drag, released from `finish_resize`.
        self.tab_latched.insert(window_id);
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
        self.tab_latched.remove(&resize.window_id);
        let window = self.windows.get_mut(&resize.window_id)?;
        window.geometry = repair_window(window.geometry, &self.outputs);
        Some((window.id, window.geometry))
    }

    pub fn is_resizing(&self, window_id: WindowId) -> bool {
        self.resize
            .is_some_and(|resize| resize.window_id == window_id)
    }

    /// Whether `window_id` has an exterior resize perimeter. Screen-filling windows
    /// cannot be resized until restored. Maximized windows retain their corner tabs;
    /// fullscreen windows suppress them.
    pub fn has_frame(&self, window_id: WindowId) -> bool {
        if !self.is_client_window(window_id) {
            return false;
        }
        self.windows
            .get(&window_id)
            .is_some_and(|window| !window.covers_screen())
    }

    /// The rectangle `window_id`'s client surface occupies: its geometry, whole. The client
    /// rectangle is authoritative — tabs live outside it, or overlaid over it when there is no
    /// exterior room — so showing, hiding or relocating tabs never changes this rectangle and
    /// never configures the client. What the compositor maps the surface at and configures the
    /// client to.
    pub fn client_area(&self, window_id: WindowId) -> Option<Rect> {
        self.windows.get(&window_id).map(|window| window.geometry)
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
        self.tab_latched.remove(&window_id);
        self.tab_revealed.remove(&window_id);
        self.tab_pending.remove(&window_id);
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
            .filter(|window| self.is_client_window(window.id) && window.mapped && window.shelved)
            .collect();
        shelved.sort_by_key(|window| std::cmp::Reverse(window.shelf_rank.unwrap_or(0)));
        let ranks = self.application_mru_ranks();
        shelved
            .into_iter()
            .map(|window| {
                self.to_shell_toplevel(window, ranks.get(&window.id).copied().unwrap_or(u32::MAX))
            })
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

    /// Raise a window's ordinary transient family, without changing any member's focus.
    pub fn raise(&mut self, window_id: WindowId) -> Option<StackPosition> {
        self.raise_transient_ancestors(window_id);
        self.layers.raise(window_id)
    }

    fn raise_transient_ancestors(&mut self, window_id: WindowId) {
        let mut ancestor = self
            .windows
            .get(&window_id)
            .and_then(|window| window.transient_parent);
        while let Some(id) = ancestor {
            if !matches!(self.layer_of(id), Some(Layer::Normal | Layer::AppOnTop)) {
                break;
            }
            self.layers.raise(id);
            ancestor = self
                .windows
                .get(&id)
                .and_then(|window| window.transient_parent);
        }
    }

    /// Borrowed popup keyboard focus promotes only that overlay over fullscreen work.
    /// Returns whether the render/hit-test mirror needs synchronization.
    pub fn set_focused_popup(&mut self, window: Option<WindowId>) -> bool {
        let window = window.filter(|id| {
            self.is_visible(*id)
                && matches!(
                    self.layer_of(*id),
                    Some(Layer::ShellOverlay | Layer::SystemModal)
                )
        });
        if self.focused_popup == window {
            return false;
        }
        self.focused_popup = window;
        true
    }

    /// Every window, top-most first.
    ///
    /// The one exception to [`LayerStack`]'s pure layer order: while a visible fullscreen client
    /// window's output is not being peeked, that window sorts *above* [`Layer::ShellOverlay`].
    /// The shell admits only critical notifications during fullscreen; [`Layer::Notification`]
    /// stays above this promotion so admitted cards remain visible. The bar still steps behind
    /// fullscreen work. Presentation, system modals, recovery and secure lock remain above both.
    ///
    /// This is the whole of the bottom-edge peek: [`Self::set_peek_output`] suspends the
    /// exception for one output and the bar comes back, with no protocol change, no surface
    /// destroyed and no rectangle touched — so nothing reflows on the way in or out. Every
    /// consumer follows, because this is the one z-order authority — the render order
    /// (`templar_comp::scene`), the hit-test mirror (`templar_comp::space_sync`) and the shell snapshot
    /// all read it.
    pub fn stacking_order(&self) -> Vec<WindowId> {
        let mut order = self.layers.order();
        if !order.iter().any(|id| {
            self.focused_popup == Some(*id)
                || self.outranks_overlay(*id)
                || self.windows.get(id).is_some_and(|window| {
                    window.stacking != WindowStacking::Normal || window.transient_parent.is_some()
                })
        }) {
            return order;
        }
        // A stable second pass applies user stacking and transient-family presentation.
        // Windows with ordinary keys keep LayerStack's order.
        order.sort_by_key(|id| std::cmp::Reverse(self.stack_key(*id)));
        order
    }

    /// The key [`Self::stacking_order`] sorts by, greatest first: the layer doubled so a promoted
    /// fullscreen window can take the half step between `Notification` and `ShellOverlay`, then the
    /// within-layer rank. Doubling rather than renumbering [`Layer`] leaves every discriminant —
    /// and so every [`crate::layers::may_request_layer`] answer — exactly as it was.
    fn stack_key(&self, window_id: WindowId) -> (i32, u64, usize, u64) {
        let Some(position) = self.layers.position(window_id) else {
            return (i32::MIN, 0, 0, 0);
        };
        let layer =
            if self.focused_popup == Some(window_id) && position.layer == Layer::ShellOverlay {
                i32::from(Layer::Notification.depth()) * 2 + 1
            } else if self.outranks_overlay(window_id) {
                i32::from(Layer::ShellOverlay.depth()) * 2 - 1
            } else if self.is_always_on_top(window_id) {
                i32::from(position.layer.depth().min(Layer::AppOnTop.depth())) * 2
            } else if self
                .windows
                .get(&window_id)
                .is_some_and(|window| window.stacking == WindowStacking::AlwaysOnBottom)
                && matches!(position.layer, Layer::Normal | Layer::AppOnTop)
            {
                // Half-step below Normal, still above Home: bottom remains a visible, usable client.
                i32::from(Layer::Normal.depth()) * 2 + 1
            } else {
                i32::from(position.layer.depth()) * 2
            };
        // Negated because `Layer`'s discriminants count *down* the screen: `SecureLock` is 0.
        let mut key = (-layer, position.rank, 0, position.rank);
        if self.focused_popup == Some(window_id) && position.layer == Layer::ShellOverlay {
            // Fullscreen uses the same half-step. Borrowed focus wins that step even
            // if the underlying fullscreen client was raised after this popup mapped.
            key.1 = u64::MAX;
            key.3 = u64::MAX;
        }
        if let Some(parent) = self
            .windows
            .get(&window_id)
            .and_then(|window| window.transient_parent)
            && self.is_visible(parent)
            && matches!(position.layer, Layer::Normal | Layer::AppOnTop)
            && matches!(self.layer_of(parent), Some(Layer::Normal | Layer::AppOnTop))
        {
            let parent_key = self.stack_key(parent);
            // Follow only an ordinary parent's presentation rank (including fullscreen).
            // Neither the assigned layer nor any authorization/activation state changes.
            key = (parent_key.0, parent_key.1, parent_key.2 + 1, position.rank);
        }
        key
    }

    /// Whether `window_id` is the one kind of window that outranks the shell's overlay: a visible
    /// fullscreen client window whose output is not being peeked.
    ///
    /// Fullscreen only, deliberately — not [`WindowRecord::covers_screen`]. A maximized window
    /// covers the bottom edge in every way that matters to the *shell*, but promoting it over
    /// `ShellOverlay` would put the launcher, the switcher and every context menu behind the
    /// window the user is working in, which is the common case rather than the rare one.
    fn outranks_overlay(&self, window_id: WindowId) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if !window.fullscreen
            || window.shelved
            || !self.is_client_window(window_id)
            || window.stacking == WindowStacking::AlwaysOnBottom
        {
            return false;
        }
        // Only the ordinary scene has a fullscreen window to promote: a reveal hides every client
        // window, and in Taskfield the window is a card under the mode's own chrome.
        if !matches!(self.presentation, Presentation::Normal) {
            return false;
        }
        !self.is_peeking(window.geometry)
    }

    /// Whether the output a window at `geometry` is on is the one being peeked. With no outputs
    /// there is one screen by definition, so any peek at all is that screen's.
    fn is_peeking(&self, geometry: Rect) -> bool {
        let Some(peeked) = self.peek_output.as_deref() else {
            return false;
        };
        self.outputs.is_empty()
            || choose_target_output(geometry, &self.outputs).name.as_str() == peeked
    }

    // ---- Corner-following window tabs --------------------------------------------------
    //
    // One [`WindowLayout`] per window, derived here so paint, hit, damage, recovery and
    // configure all share it. Policy is one value for every window; the active corner, the
    // aim/press latch and the hover reveal are per-window state that survives park/restore,
    // maximize/fullscreen and regrouping.

    /// The tab policy in force.
    pub fn tab_policy(&self) -> TabPolicy {
        self.tab_policy
    }

    /// Replace the tab policy. Returns whether anything changed, so the compositor
    /// re-renders only when it must. Stored corners are kept: a policy change rearranges
    /// chrome, it never forgets where a window's tabs were.
    pub fn set_tab_policy(&mut self, policy: TabPolicy) -> bool {
        let changed = self.tab_policy != policy;
        self.tab_policy = policy;
        changed
    }

    /// The corner `window_id`'s tabs currently follow: the stored corner while
    /// `follow_pointer` holds, else the fixed fallback. Unknown windows answer the policy
    /// default rather than failing — there is always a corner.
    pub fn tab_corner(&self, window_id: WindowId) -> Corner {
        if !self.tab_policy.follow_pointer {
            return self.tab_policy.fixed_corner;
        }
        self.tab_corners
            .get(&window_id)
            .copied()
            .unwrap_or(self.tab_policy.default_corner)
    }

    /// Store `window_id`'s active corner. Returns whether it changed. Unknown windows and
    /// non-client windows are refused — there is nothing to follow with.
    pub fn set_tab_corner(&mut self, window_id: WindowId, corner: Corner) -> bool {
        if !self.is_client_window(window_id) || !self.windows.contains_key(&window_id) {
            return false;
        }
        let changed = self.tab_corners.get(&window_id).copied() != Some(corner);
        self.tab_corners.insert(window_id, corner);
        changed
    }

    /// Follow the pointer to the nearest client corner, with deterministic ties and
    /// hysteresis ([`WindowLayout::nearest_corner`]). Returns the previous and the new
    /// corner, or `None` for an unknown or non-client window.
    ///
    /// A latched window ([`Self::set_tab_latched`]) keeps its corner: aiming at a visible
    /// title/control, pressing one, or running a move/resize/menu freezes the tabs until
    /// the grab's own termination path releases them. With `follow_pointer` off the fixed
    /// corner holds and this is a no-op that still reports it.
    pub fn select_tab_corner(
        &mut self,
        window_id: WindowId,
        pointer: Point,
    ) -> Option<(Corner, Corner)> {
        if !self.is_client_window(window_id) {
            return None;
        }
        let geometry = self.windows.get(&window_id)?.geometry;
        let previous = self.tab_corner(window_id);
        if self.tab_latched.contains(&window_id) || !self.tab_policy.follow_pointer {
            return Some((previous, previous));
        }
        let next = WindowLayout::nearest_corner(geometry, pointer, Some(previous));
        self.tab_corners.insert(window_id, next);
        Some((previous, next))
    }

    /// Freeze (`true`) or release (`false`) `window_id`'s tabs. Returns whether anything
    /// changed. Set while the pointer aims at a visible title/control and across
    /// press/release, menu, drag and resize; released from the grab's own termination
    /// path, so a latch can never outlive the gesture that took it.
    pub fn set_tab_latched(&mut self, window_id: WindowId, latched: bool) -> bool {
        if latched {
            self.tab_latched.insert(window_id)
        } else {
            self.tab_latched.remove(&window_id)
        }
    }

    /// Whether `window_id`'s tabs are frozen.
    pub fn is_tab_latched(&self, window_id: WindowId) -> bool {
        self.tab_latched.contains(&window_id)
    }

    /// Release press/aim latches when no grab holds them.
    pub fn clear_tab_latches(&mut self) {
        self.tab_latched.clear();
    }

    /// Cancel transient chrome interaction on lock, topology or presentation changes.
    /// Stored corners and committed policy survive; no delayed reveal may survive.
    pub fn clear_tab_interaction(&mut self) {
        self.tab_latched.clear();
        self.tab_revealed.clear();
        self.tab_pending.clear();
    }

    /// Show (`true`) or hide (`false`) `window_id`'s hover controls. Returns whether
    /// anything changed. Reveal changes nothing else — no focus, no raise, no action —
    /// and a hidden control takes no input.
    pub fn set_controls_revealed(&mut self, window_id: WindowId, revealed: bool) -> bool {
        if revealed && !self.has_window_tabs(window_id) {
            return false;
        }
        if revealed {
            self.tab_revealed.insert(window_id)
        } else {
            self.tab_revealed.remove(&window_id)
        }
    }

    /// Whether `window_id`'s controls are currently drawn and clickable: always, under the
    /// always-controls policy, else only while revealed.
    pub fn controls_revealed(&self, window_id: WindowId) -> bool {
        self.has_window_tabs(window_id)
            && (self.tab_policy.controls_always || self.tab_revealed.contains(&window_id))
    }

    /// Whether `window_id`'s title is currently drawn: always, under the default policy,
    /// else only while revealed.
    pub fn title_revealed(&self, window_id: WindowId) -> bool {
        self.has_window_tabs(window_id)
            && (self.tab_policy.title_always || self.tab_revealed.contains(&window_id))
    }

    /// Fullscreen clients own their entire surface, including every corner. This one
    /// policy governs both tab paint and hit geometry; maximized clients keep their tabs.
    fn has_window_tabs(&self, window_id: WindowId) -> bool {
        self.is_client_window(window_id)
            && self
                .windows
                .get(&window_id)
                .is_some_and(|window| !window.fullscreen)
    }

    /// `window_id`'s complete tab geometry: the stored corner, the shared policy, the
    /// window's own output for the exterior/overlay decision. `None` for unknown or
    /// non-client or fullscreen windows. Every consumer — paint, hit, damage, recovery,
    /// configure — derives its rectangles from this and nothing else.
    pub fn window_layout(&self, window_id: WindowId) -> Option<WindowLayout> {
        if !self.has_window_tabs(window_id) {
            return None;
        }
        let geometry = self.windows.get(&window_id)?.geometry;
        let output = if self.outputs.is_empty() {
            // No outputs: nothing is on screen, so nothing can be off it either — lay
            // out exterior and let the next topology change re-derive.
            Rect::new(
                geometry.x - 4096,
                geometry.y - 4096,
                geometry.width + 8192,
                geometry.height + 8192,
            )
        } else {
            choose_target_output(geometry, &self.outputs).usable()
        };
        Some(WindowLayout::new(
            geometry,
            self.tab_corner(window_id),
            self.tab_policy.placement,
            output,
        ))
    }

    /// Arm once on entry, preserving the original deadline across further motion.
    pub fn note_tab_approach(&mut self, window_id: WindowId, now_ns: u64) {
        if !self.is_visible(window_id) || !self.has_window_tabs(window_id) {
            return;
        }
        if self.tab_revealed.contains(&window_id)
            || (self.tab_policy.controls_always && self.tab_policy.title_always)
        {
            self.tab_pending.remove(&window_id);
            return;
        }
        let pending = self.tab_pending.entry(window_id).or_default();
        pending
            .reveal_at
            .get_or_insert(now_ns.saturating_add(crate::geometry::TAB_REVEAL_GRACE_MS * 1_000_000));
        pending.exit_at = None;
    }

    /// Cancel an unfinished reveal immediately; arm one exit for revealed hover tabs.
    pub fn note_tab_leave(&mut self, window_id: WindowId, now_ns: u64) {
        if !self.tab_revealed.contains(&window_id) {
            self.tab_pending.remove(&window_id);
            return;
        }
        let pending = self.tab_pending.entry(window_id).or_default();
        pending.reveal_at = None;
        pending
            .exit_at
            .get_or_insert(now_ns.saturating_add(crate::geometry::TAB_EXIT_GRACE_MS * 1_000_000));
    }

    /// Fire every tab grace deadline reached by monotonic `now_ns`, oldest first, applying
    /// the reveal/hide to the stored state and returning what changed. Run from the
    /// compositor's existing dwell wakeup: one shared wakeup, never a timer per motion.
    /// A latched window's exit deadline waits — hiding controls mid-aim would be the flee
    /// the latch exists to prevent — while its reveal still fires.
    pub fn fire_tab_deadlines(&mut self, now_ns: u64) -> Vec<TabDeadlineFired> {
        let mut due: Vec<(u64, WindowId, bool)> = Vec::new();
        for (window_id, pending) in &self.tab_pending {
            if pending.reveal_at.is_some_and(|at| at <= now_ns) {
                due.push((pending.reveal_at.unwrap_or(u64::MAX), *window_id, true));
            } else if pending.exit_at.is_some_and(|at| at <= now_ns)
                && !self.tab_latched.contains(window_id)
            {
                due.push((pending.exit_at.unwrap_or(u64::MAX), *window_id, false));
            }
        }
        due.sort();
        let mut fired = Vec::with_capacity(due.len());
        for (_, window_id, reveal) in due {
            if let Some(pending) = self.tab_pending.get_mut(&window_id) {
                if reveal {
                    pending.reveal_at = None;
                    if self.tab_revealed.insert(window_id) {
                        fired.push(TabDeadlineFired::Reveal(window_id));
                    }
                } else {
                    pending.exit_at = None;
                    if self.tab_revealed.remove(&window_id) {
                        fired.push(TabDeadlineFired::Exit(window_id));
                    }
                }
                if pending.reveal_at.is_none() && pending.exit_at.is_none() {
                    self.tab_pending.remove(&window_id);
                }
            }
        }
        fired
    }

    /// The earliest pending tab deadline, if any — what the shared wakeup waits for.
    pub fn next_tab_deadline(&self) -> Option<u64> {
        self.tab_pending
            .iter()
            .filter_map(|(id, pending)| {
                pending
                    .reveal_at
                    .into_iter()
                    .chain(pending.exit_at.filter(|_| !self.tab_latched.contains(id)))
                    .min()
            })
            .min()
    }

    /// Whether `window_id` currently holds any pending tab deadline. The motion path
    /// consults it so leaving an already-settled window notes nothing — otherwise every
    /// motion would arm an exit for every window on screen.
    pub fn has_tab_pending(&self, window_id: WindowId) -> bool {
        self.tab_pending.contains_key(&window_id)
    }

    /// Whether `window_id` is pinned above the other client windows.
    #[must_use]
    pub fn is_always_on_top(&self, window_id: WindowId) -> bool {
        matches!(
            self.layer_of(window_id),
            Some(Layer::Normal | Layer::AppOnTop)
        ) && self
            .windows
            .get(&window_id)
            .is_some_and(|window| window.stacking == WindowStacking::AlwaysOnTop)
    }

    /// Pin or unpin `window_id`, returning its new state. Client windows only — a shell surface
    /// already sits on the layer its trust bought it, and nothing about a pin should be able to
    /// move it.
    pub fn toggle_always_on_top(&mut self, window_id: WindowId) -> bool {
        let stacking = if self.is_always_on_top(window_id) {
            WindowStacking::Normal
        } else {
            WindowStacking::AlwaysOnTop
        };
        self.set_window_stacking(window_id, stacking) && stacking == WindowStacking::AlwaysOnTop
    }

    /// Set mutually-exclusive user stacking without raising, activating, or granting authority.
    /// Protected scene roles/layers cannot be moved by an application title menu.
    pub fn set_window_stacking(&mut self, window_id: WindowId, stacking: WindowStacking) -> bool {
        if !self.is_client_window(window_id)
            || !matches!(
                self.layer_of(window_id),
                Some(Layer::Normal | Layer::AppOnTop)
            )
        {
            return false;
        }
        let Some(window) = self.windows.get_mut(&window_id) else {
            return false;
        };
        window.stacking = stacking;
        true
    }

    /// The output whose overlay surfaces are peeked out from under a fullscreen window, if any.
    pub fn peek_output(&self) -> Option<&str> {
        self.peek_output.as_deref()
    }

    /// Start or end a peek on one output (`None` ends it). Returns whether anything changed, so
    /// the compositor can re-stack only when it must. One output at a time, because there is one
    /// pointer.
    ///
    // ponytail: one peeked output. Per-output peeks would need a set here and a pointer per seat
    // above; nothing on this desktop has two pointers.
    pub fn set_peek_output(&mut self, output: Option<String>) -> bool {
        let changed = self.peek_output != output;
        self.peek_output = output;
        changed
    }

    /// One sentence explaining why `a` is stacked above or below `b`.
    pub fn explain_stacking(&self, a: WindowId, b: WindowId) -> Option<String> {
        self.layers.explain(a, b)
    }

    /// Whether a window that has just mapped should be given the keyboard.
    ///
    /// A window the person just opened is the window they meant to use: nothing else in the map
    /// path activates one, so an application launched from the Home search bar would otherwise
    /// map *under* a Home surface that still owns the keyboard. Client windows only — shell
    /// chrome maps through [`Self::on_toplevel_mapped`] too and must never take focus from it.
    ///
    /// The one exception is a visible, activated **fullscreen** client window on the same output:
    /// a video or a game filling the screen is not worth interrupting for a window that has only
    /// just appeared. Same output, by [`choose_target_output`], for the same reason
    /// [`Self::home_is_covered`] is per output rather than per rectangle.
    pub fn should_activate_on_map(&self, window_id: WindowId) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if !window.mapped || !self.is_client_window(window_id) {
            return false;
        }
        let output = (!self.outputs.is_empty()).then(|| {
            choose_target_output(window.geometry, &self.outputs)
                .name
                .as_str()
        });
        !self.windows.values().any(|other| {
            other.id != window_id
                && other.fullscreen
                && other.activated
                && self.is_visible(other.id)
                && output.is_none_or(|name| {
                    choose_target_output(other.geometry, &self.outputs).name == name
                })
        })
    }

    /// Give `window_id` keyboard focus: clears `activated` on every other window and raises it
    /// to the top of its layer. Returns `false` if the window does not exist.
    ///
    /// A grouped window raises its **whole group** contiguously, keeping the members' relative
    /// order with the activated one on top (planning doc §2 rule 3). That is what makes a group
    /// behave as one set of windows rather than as a label — and it is also why it is safe:
    /// raising changes rank, never geometry, so the recovery invariant cannot notice it.
    pub fn activate(&mut self, window_id: WindowId) -> bool {
        if !self
            .windows
            .get(&window_id)
            .is_some_and(|window| window.mapped)
        {
            return false;
        }
        // Accepted activation is how a shelved card comes back: selection highlights and does
        // not unshelve, a click or Enter does. The shelf is a view of mapped windows, so a
        // mapped-and-shelved record is a valid target.
        if self
            .windows
            .get(&window_id)
            .is_some_and(|window| window.shelved)
        {
            let _ = self.unshelve(window_id);
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
            self.raise_transient_ancestors(window_id);
            if let Some(record) = self.groups.get_mut(&group) {
                record.last_active = window_id;
            }
        } else {
            self.raise(window_id);
        }
        if self
            .windows
            .get(&window_id)
            .is_some_and(|window| window.role == WindowRole::Application)
        {
            self.activation_serial = self.activation_serial.saturating_add(1);
            if let Some(window) = self.windows.get_mut(&window_id) {
                window.last_activated = self.activation_serial;
            }
        }
        // Activating a window (a click on its tile from the Home Field, a card in Taskfield) is
        // the user asking for it: whatever presentation was over it ends here, for free, with
        // nothing else to undo.
        self.presentation = Presentation::Normal;
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
        let window = self.windows.get(&target)?;
        if window.fullscreen {
            // Through `set_fullscreen`, so the escape route returns the window to exactly the
            // geometry the fullscreen entry took it from — the same slot, the same restore.
            self.set_fullscreen(target, false);
            Some(EscapeAction::ExitedFullscreen { window_id: target })
        } else if window.modal {
            self.windows.get_mut(&target)?.modal = false;
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

    /// The compositor's grab affordance for `window_id`: the recovery band, and whether
    /// the compositor must paint a temporary tab over it (client-side-decorated /
    /// borderless). Answered for every client window; whether tabs are *drawn* is
    /// [`Self::has_frame`] plus the tab reveal state.
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
    /// mapped, **unshelved** members' geometries (planning doc §2 rule 7). `None` for an unknown
    /// group or one whose members are all shelved or temporarily unmapped.
    pub fn group_outline(&self, group: GroupId) -> Option<Rect> {
        let rects: Vec<Rect> = self
            .group_members(group)
            .into_iter()
            .filter_map(|id| self.windows.get(&id))
            .filter(|window| window.mapped && !window.shelved)
            .map(|window| window.geometry)
            .collect();
        bounding_rect(&rects)
    }

    /// Every window a focus fallback may land on, most-recently activated first. Semantic role,
    /// protocol mapping and shelving are the only eligibility inputs; visual stacking,
    /// fullscreen, geometry and pinning are intentionally absent. `Alt+Tab` lists
    /// [`Self::alt_tab_order`] instead, which keeps the parked windows this leaves out.
    pub fn switch_order(&self) -> Vec<WindowId> {
        self.application_windows_by_recency(false)
    }

    /// Mapped application windows, newest activation first (ties: newest id first).
    fn application_windows_by_recency(&self, include_shelved: bool) -> Vec<WindowId> {
        let mut windows: Vec<&WindowRecord> = self
            .windows
            .values()
            .filter(|window| {
                window.role == WindowRole::Application
                    && window.mapped
                    && (include_shelved || !window.shelved)
            })
            .collect();
        windows.sort_by_key(|window| {
            (
                std::cmp::Reverse(window.last_activated),
                std::cmp::Reverse(window.id),
            )
        });
        windows.into_iter().map(|window| window.id).collect()
    }

    /// Why a known toplevel is not in [`Self::alt_tab_order`]. `None` means it is listed.
    /// Shelving is deliberately not a reason: a parked window is still an open one.
    pub fn switch_exclusion_reason(&self, window_id: WindowId) -> Option<&'static str> {
        let Some(window) = self.windows.get(&window_id) else {
            return Some("unknown-window");
        };
        if window.role != WindowRole::Application {
            return Some("non-application-role");
        }
        if !window.mapped {
            return Some("unmapped");
        }
        None
    }

    pub fn window(&self, window_id: WindowId) -> Option<&WindowRecord> {
        self.windows.get(&window_id)
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowRecord> {
        self.windows.values()
    }

    /// Whether `window_id` is semantically an application window. This compatibility name is
    /// retained for geometry and input callers; the answer no longer comes from visual layers.
    pub fn is_client_window(&self, window_id: WindowId) -> bool {
        self.windows
            .get(&window_id)
            .is_some_and(|window| window.role == WindowRole::Application)
    }

    /// Sanitized application-window view for the shell (dock, switcher, window shelf): no PIDs,
    /// no raw claims, and no shell-owned surfaces. Feeding the Home Surface or dock back into this
    /// list would make the shell create session tiles and recursive previews of its own windows.
    pub fn shell_toplevels(&self) -> Vec<ShellToplevel> {
        let ranks = self.application_mru_ranks();
        let mut list: Vec<ShellToplevel> = self
            .windows
            .values()
            .filter(|window| window.role == WindowRole::Application && window.mapped)
            .map(|window| {
                self.to_shell_toplevel(window, ranks.get(&window.id).copied().unwrap_or(u32::MAX))
            })
            .collect();
        list.sort_by_key(|toplevel| toplevel.window_id);
        list
    }

    /// Every task unit — the one grouping the island, Taskfield, `` Alt+` `` and the launcher all
    /// read ([`crate::task::units`]). It is deliberately the same pure function the shell runs over
    /// the toplevels it receives, so the two cannot compute different answers.
    #[must_use]
    pub fn task_units(&self) -> Vec<crate::TaskUnit> {
        crate::task::units(&self.shell_toplevels())
    }

    /// One rank per mapped application window: sort once, enumerate. Callers that used to
    /// rescan the map per window (`O(n²)` via [`Self::mru_rank`]) share this snapshot instead.
    fn application_mru_ranks(&self) -> HashMap<WindowId, u32> {
        let mut eligible: Vec<&WindowRecord> = self
            .windows
            .values()
            .filter(|window| window.role == WindowRole::Application && window.mapped)
            .collect();
        eligible.sort_by(|left, right| {
            right
                .last_activated
                .cmp(&left.last_activated)
                .then(right.id.cmp(&left.id))
        });
        eligible
            .into_iter()
            .enumerate()
            .map(|(rank, window)| (window.id, u32::try_from(rank).unwrap_or(u32::MAX)))
            .collect()
    }

    fn to_shell_toplevel(&self, window: &WindowRecord, mru_rank: u32) -> ShellToplevel {
        ShellToplevel {
            window_id: window.id,
            canonical_app_id: window.identity.app_id.clone(),
            claimed_app_id: None,
            instance_id: window.identity.instance_id,
            title: window.title.clone(),
            verified_identity: window.identity.verified,
            activated: window.activated,
            mru_rank,
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
            // Tab counts arrive from sessiond adapters bound to this toplevel; the feed lands
            // with the protocol owner (hook: fill from the compositor's tab-count store).
            tab_count: None,
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

/// Where a fullscreen window goes: the whole [`Output::logical`] rectangle of the output nearest
/// `current` — deliberately not [`Output::usable`], because "genuinely full screen" means over
/// the bar's exclusive zone as well, not politely beside it. With no outputs at all there is
/// nothing to fill, and the window is left as it is.
fn fullscreen_geometry(current: Rect, outputs: &[Output]) -> Rect {
    if outputs.is_empty() {
        return current;
    }
    choose_target_output(current, outputs).logical
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::TabPlacement;
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

    #[test]
    fn a_newly_mapped_client_window_is_the_one_to_focus_and_a_shell_surface_never_is() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let window = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            "kitty".into(),
            Rect::new(0, 0, 400, 300),
        );
        assert!(service.should_activate_on_map(window));
        // A reveal that was hiding everything is over the moment something new arrives.
        assert!(!service.home_revealed());

        let home = service.on_toplevel_mapped(
            2,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "Home".into(),
            Rect::new(0, 0, 1920, 1080),
        );
        service
            .set_layer(home, Layer::HomeSurface, ClientKind::TrustedShell)
            .unwrap();
        service.set_role(home, WindowRole::Shell);
        assert!(!service.should_activate_on_map(home));
        assert!(!service.should_activate_on_map(WindowId(9999)));
    }

    #[test]
    fn a_new_window_never_interrupts_a_fullscreen_window_on_its_own_output() {
        let mut service = WindowService::new(
            FakeCache::default(),
            vec![
                Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000),
                Output::new("HDMI-1", Rect::new(1920, 0, 1920, 1080), 1000),
            ],
        );
        let video = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            "video".into(),
            Rect::new(0, 0, 800, 600),
        );
        service.set_fullscreen(video, true);
        service.activate(video);

        let here = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "notification-ish".into(),
            Rect::new(100, 100, 400, 300),
        );
        assert!(
            !service.should_activate_on_map(here),
            "a new window must not steal the keyboard from a fullscreen window beside it"
        );
        let there = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            "on the other screen".into(),
            Rect::new(2000, 100, 400, 300),
        );
        assert!(
            service.should_activate_on_map(there),
            "the other output is not the fullscreen window's business"
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
            &[crate::geometry::Exclusive::new(
                crate::geometry::Edge::Bottom,
                size,
            )],
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
        service.set_role(
            id,
            match layer {
                Layer::HomeSurface => WindowRole::Shell,
                Layer::Notification => WindowRole::Notification,
                Layer::SecureLock => WindowRole::LockScreen,
                Layer::Recovery
                | Layer::SystemModal
                | Layer::Presentation
                | Layer::ShellOverlay => WindowRole::Overlay,
                Layer::AppOnTop | Layer::Normal => WindowRole::Application,
            },
        );
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

    /// A window that fills the screen wears no exterior tabs, whichever way it got there
    /// and whatever the bar happens to be reserving: there is no exterior to wear them on.
    /// The shared layout overlays its title and hover controls inside the output.
    #[test]
    fn a_window_that_fills_the_screen_has_no_frame() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        let screen = outputs()[0].logical;
        assert!(service.has_frame(id));

        service.set_maximized(id, true);
        assert!(!service.has_frame(id), "maximized: no exterior tabs");
        assert_eq!(
            service.client_area(id),
            service.window(id).map(|window| window.geometry),
            "no frame: the client is the whole maximized geometry"
        );
        assert_eq!(
            service.window(id).unwrap().geometry,
            screen,
            "maximized fills the whole output when nothing is reserved"
        );

        // The bar's zone decides nothing about the frame any more, at either size.
        for zone in [0, 48] {
            service.on_outputs_changed(vec![output_with_a_bottom_bar(zone)]);
            assert!(!service.has_frame(id), "zone {zone}");
        }
        service.on_outputs_changed(outputs());

        service.set_maximized(id, false);
        assert!(service.has_frame(id), "a floating window wears its tags");
        service.set_fullscreen(id, true);
        assert!(!service.has_frame(id), "fullscreen never has a frame");
        service.set_fullscreen(id, false);
        service.set_maximized(id, true);
        service.shelve(id);
        assert!(
            service.has_frame(id),
            "a parked window covers nothing, so it keeps its own tags"
        );
    }

    #[test]
    fn transient_children_follow_fullscreen_without_authority_or_focus() {
        let (mut service, parent) = one_window(Rect::new(100, 100, 400, 300));
        let child = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "chooser".into(),
            Rect::new(120, 120, 200, 100),
        );
        let sibling = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            "other chooser".into(),
            Rect::new(140, 140, 200, 100),
        );
        let identity = service.window(child).unwrap().identity.clone();
        service.set_fullscreen(parent, true);
        service.activate(parent);
        assert!(service.set_transient_parent(child, Some(parent)));
        assert!(service.set_transient_parent(sibling, Some(parent)));
        let order = service.stacking_order();
        assert!(
            order.iter().position(|id| *id == child) < order.iter().position(|id| *id == parent)
        );
        assert!(
            order.iter().position(|id| *id == sibling) < order.iter().position(|id| *id == parent)
        );
        assert_eq!(service.layer_of(child), Some(Layer::Normal));
        assert_eq!(service.window(child).unwrap().identity, identity);
        assert!(!service.window(child).unwrap().activated);
        assert!(!service.set_transient_parent(parent, Some(child)), "cycle");
        assert!(!service.set_transient_parent(child, Some(child)), "self");
        assert!(
            !service.set_transient_parent(child, Some(WindowId(u64::MAX))),
            "dead"
        );
        let protected = shell_surface(&mut service, Layer::SystemModal, Rect::new(0, 0, 100, 100));
        assert!(!service.set_transient_parent(child, Some(protected)));
        assert!(!service.set_transient_parent(protected, Some(parent)));
        service.set_role(protected, WindowRole::Application);
        assert!(
            !service.set_transient_parent(child, Some(protected)),
            "protected layer despite ordinary role"
        );
        assert!(!service.set_transient_parent(protected, Some(parent)));
        assert!(
            service.set_transient_parent(child, Some(sibling)),
            "parent replacement"
        );
        assert_eq!(
            service.window(child).unwrap().transient_parent,
            Some(sibling)
        );
        assert_eq!(
            service.window(sibling).unwrap().transient_parent,
            Some(parent)
        );
        assert!(
            service.set_transient_parent(child, None),
            "property deletion"
        );
        assert_eq!(service.window(child).unwrap().transient_parent, None);
        assert_eq!(
            service.window(sibling).unwrap().transient_parent,
            Some(parent),
            "detaching one child must preserve unrelated siblings"
        );
        assert_eq!(service.window(child).unwrap().identity, identity);
        assert!(!service.window(child).unwrap().activated);
        service.on_toplevel_destroyed(parent);
        assert_eq!(service.window(child).unwrap().transient_parent, None);
        assert_eq!(service.window(sibling).unwrap().transient_parent, None);
        assert!(service.is_visible(child));
    }

    /// A pin sorts a window as if it were on `AppOnTop` without moving it there: `layer_of` still
    /// answers `Normal`, so nothing written in terms of layers changes for a pinned window.
    #[test]
    fn a_pinned_window_sorts_above_the_others_without_changing_layer() {
        let (mut service, lower) = one_window(Rect::new(100, 100, 400, 300));
        let upper = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(200, 200, 400, 300),
        );
        assert_eq!(
            service.stacking_order().first().copied(),
            Some(upper),
            "the newer window is on top to start with"
        );

        assert!(service.toggle_always_on_top(lower));
        assert_eq!(service.layer_of(lower), Some(Layer::Normal));
        assert_eq!(service.stacking_order().first().copied(), Some(lower));

        service.activate(upper);
        assert_eq!(
            service.stacking_order().first().copied(),
            Some(lower),
            "raising another window does not get past a pin"
        );

        assert!(!service.toggle_always_on_top(lower));
        assert_eq!(service.stacking_order().first().copied(), Some(upper));
    }

    #[test]
    fn user_stacking_is_exclusive_persistent_and_keeps_bottom_above_home() {
        let (mut service, first) = one_window(Rect::new(100, 100, 400, 300));
        let second = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(200, 200, 400, 300),
        );
        let home = shell_surface(
            &mut service,
            Layer::HomeSurface,
            Rect::new(0, 0, 1920, 1080),
        );
        let identity = service.window(first).unwrap().identity.clone();
        service.activate(second);
        let mru = service.switch_order();
        for preference in [WindowStacking::AlwaysOnTop, WindowStacking::AlwaysOnBottom] {
            assert!(service.set_window_stacking(first, preference));
            service.raise(second);
            let order = service.stacking_order();
            let rank = |id| order.iter().position(|candidate| *candidate == id).unwrap();
            assert_eq!(
                rank(first) < rank(second),
                preference == WindowStacking::AlwaysOnTop
            );
            assert!(rank(first) < rank(home));
            assert!(service.is_visible(first));
            assert_eq!(service.layer_of(first), Some(Layer::Normal));
            assert_eq!(service.window(first).unwrap().identity, identity);
            assert!(!service.window(first).unwrap().activated);
            assert_eq!(service.switch_order(), mru);
            assert!(service.set_window_stacking(first, WindowStacking::Normal));
            assert_eq!(service.stacking_order().first().copied(), Some(second));
        }
        service.set_window_stacking(first, WindowStacking::AlwaysOnBottom);
        service.activate(first);
        assert_eq!(
            service.stacking_order().first().copied(),
            Some(second),
            "activation and raises do not erase the bottom preference"
        );
        assert!(service.toggle_always_on_top(first));
        assert_eq!(
            service.window(first).unwrap().stacking,
            WindowStacking::AlwaysOnTop
        );
        assert!(!service.toggle_always_on_top(first));
        assert_eq!(
            service.window(first).unwrap().stacking,
            WindowStacking::Normal
        );
        assert!(!service.set_window_stacking(home, WindowStacking::AlwaysOnTop));
        let protected = shell_surface(&mut service, Layer::SystemModal, Rect::new(0, 0, 100, 100));
        service.set_role(protected, WindowRole::Application);
        assert!(!service.set_window_stacking(protected, WindowStacking::AlwaysOnBottom));
        assert!(!service.set_window_stacking(WindowId(u64::MAX), WindowStacking::Normal));
    }

    #[test]
    fn transient_family_follows_user_stacking_without_focus_or_identity_changes() {
        let (mut service, parent) = one_window(Rect::new(100, 100, 400, 300));
        let child = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "dialog".into(),
            Rect::new(200, 200, 200, 100),
        );
        let other = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            "other".into(),
            Rect::new(200, 200, 400, 300),
        );
        let home = shell_surface(
            &mut service,
            Layer::HomeSurface,
            Rect::new(0, 0, 1920, 1080),
        );
        assert!(service.set_transient_parent(child, Some(parent)));
        let identity = service.window(child).unwrap().identity.clone();
        service.activate(other);
        for preference in [WindowStacking::AlwaysOnTop, WindowStacking::AlwaysOnBottom] {
            service.set_window_stacking(parent, preference);
            service.raise(other);
            let order = service.stacking_order();
            let rank = |id| order.iter().position(|candidate| *candidate == id).unwrap();
            assert!(
                rank(child) < rank(parent),
                "dialogs stay usable above their parent"
            );
            assert_eq!(
                rank(child) < rank(other),
                preference == WindowStacking::AlwaysOnTop
            );
            assert!(rank(parent) < rank(home));
            assert!(!service.window(child).unwrap().activated);
            assert_eq!(service.window(child).unwrap().identity, identity);
            assert_eq!(service.layer_of(child), Some(Layer::Normal));
        }
        service.set_fullscreen(parent, true);
        assert_eq!(
            service.stacking_order().first().copied(),
            Some(other),
            "explicit bottom overrides fullscreen promotion, not visibility"
        );
        service.set_window_stacking(parent, WindowStacking::Normal);
        assert_eq!(
            service.stacking_order().first().copied(),
            Some(child),
            "Normal restores fullscreen's family presentation precedence"
        );
        service.on_toplevel_destroyed(parent);
        assert_eq!(service.window(child).unwrap().transient_parent, None);
        assert!(service.is_visible(child));
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
        assert_eq!(service.switch_order(), vec![c, a]);
        let mut ring = crate::SwitchRing::new(service.switch_order(), None).unwrap();
        assert_eq!(ring.step(true), c);
        assert_eq!(ring.step(true), a);
        assert_eq!(ring.step(false), c);
    }

    #[test]
    fn switching_uses_semantic_mru_not_visual_stacking() {
        let (mut service, ids) = service_with(3);
        service.activate(ids[0]);
        service.activate(ids[1]);
        assert_eq!(service.switch_order(), vec![ids[1], ids[0], ids[2]]);

        service.toggle_always_on_top(ids[2]);
        service.raise(ids[0]);
        service.set_fullscreen(ids[1], true);
        assert_eq!(
            service.switch_order(),
            vec![ids[1], ids[0], ids[2]],
            "fullscreen, pinning and raises are visual state, not switching policy"
        );
    }

    #[test]
    fn temporary_unmap_retains_identity_but_excludes_the_window() {
        let (mut service, ids) = service_with(2);
        service.activate(ids[0]);
        let serial = service.window(ids[0]).unwrap().last_activated;

        assert!(service.set_mapped(ids[0], false));
        assert_eq!(service.switch_exclusion_reason(ids[0]), Some("unmapped"));
        assert!(!service.switch_order().contains(&ids[0]));
        assert!(
            !service
                .shell_toplevels()
                .iter()
                .any(|window| window.window_id == ids[0])
        );

        assert!(service.set_mapped(ids[0], true));
        assert_eq!(service.window(ids[0]).unwrap().last_activated, serial);
        assert!(service.switch_order().contains(&ids[0]));
    }

    #[test]
    fn a_shell_role_is_not_an_application_even_on_the_normal_layer() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "shell".into(),
            Rect::new(0, 0, 100, 100),
        );
        service.set_role(id, WindowRole::Shell);
        assert_eq!(service.layer_of(id), Some(Layer::Normal));
        assert!(!service.is_client_window(id));
        assert_eq!(
            service.switch_exclusion_reason(id),
            Some("non-application-role")
        );
        assert!(service.shell_toplevels().is_empty());
    }

    /// Activation can change semantic MRU while the chord is held — a client's xdg-activation, a
    /// window mapping. A frozen ring must keep walking the list the hold began with rather than
    /// observing those mutations midway through the gesture.
    #[test]
    fn a_frozen_list_reaches_the_third_window_where_a_rebuilt_one_never_could() {
        let (mut service, ids) = service_with(3);
        service.activate(ids[2]);

        // Rebuilding the order after each activation: ids[1], then straight back to ids[2].
        let rebuilt_first = service.switch_order()[1];
        service.activate(rebuilt_first);
        assert_eq!(service.switch_order()[1], ids[2], "it can only oscillate");

        service.activate(ids[2]);
        let mut ring = crate::SwitchRing::new(service.switch_order(), Some(ids[2])).unwrap();
        let first = ring.step(true);
        service.activate(first);
        let second = ring.step(true);
        service.activate(second);
        assert_ne!(first, second);
        assert_ne!(second, ids[2], "the third window is reachable");
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
        service.set_role(home, WindowRole::Shell);
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
            service.is_visible(home),
            "no window here is maximized or fullscreen, so Home stays the backdrop"
        );
        service.set_home_revealed(true);
        assert!(service.is_visible(home));
        service.set_home_revealed(false);
        assert!(service.is_visible(home));
    }

    /// Home is the bottom layer and is on screen unless something actually covers it. The whole
    /// table, because the previous rule — hidden unless revealed — is what made a fresh login a
    /// black screen the user had to press Super to escape.
    #[test]
    fn home_is_visible_unless_a_maximized_or_fullscreen_window_covers_its_output() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let home = shell_surface(
            &mut service,
            Layer::HomeSurface,
            Rect::new(0, 0, 1920, 1080),
        );
        assert!(
            service.is_visible(home),
            "an empty desktop is the Home Field"
        );

        let app = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(10, 10, 400, 300),
        );
        assert!(service.is_visible(home), "a floating window covers nothing");

        service.set_maximized(app, true);
        assert!(!service.is_visible(home), "a maximized window covers Home");

        service.set_maximized(app, false);
        assert!(service.is_visible(home));

        service.set_fullscreen(app, true);
        assert!(!service.is_visible(home), "a fullscreen window covers Home");

        // Shelved is off the screen, so it covers nothing however it is sized.
        service.shelve(app);
        assert!(
            service.is_visible(home),
            "a shelved window is not on the screen to cover anything"
        );
        service.unshelve(app);
        assert!(!service.is_visible(home));

        // A reveal shows Home regardless — that is what a reveal is for.
        service.set_home_revealed(true);
        assert!(service.is_visible(home));
        service.set_home_revealed(false);
        assert!(!service.is_visible(home));

        // The shell's own bar is not a client window and never covers Home, whatever it claims.
        service.set_fullscreen(app, false);
        let bar = shell_surface(
            &mut service,
            Layer::ShellOverlay,
            Rect::new(0, 0, 1920, 1080),
        );
        service.set_maximized(bar, true);
        assert!(service.is_visible(home));
    }

    /// Per output: a maximized window on the second screen leaves the first screen's Home alone.
    #[test]
    fn a_maximized_window_on_another_output_does_not_cover_home() {
        let outputs = vec![
            Output::new("primary", Rect::new(0, 0, 1920, 1080), 1000),
            Output::new("second", Rect::new(1920, 0, 1920, 1080), 1000),
        ];
        let mut service = WindowService::new(FakeCache::default(), outputs);
        let home = shell_surface(
            &mut service,
            Layer::HomeSurface,
            Rect::new(0, 0, 1920, 1080),
        );
        let app = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(1930, 10, 400, 300),
        );
        service.set_maximized(app, true);
        assert!(
            service.is_visible(home),
            "a window filling the second screen must not blank the first"
        );
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
        service.set_role(home, WindowRole::Shell);

        assert_eq!(service.switch_order(), vec![app]);
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

    /// The one invariant that replaces every fullscreen special case: passive chrome yields to a
    /// fullscreen window only in the ordinary scene. In Taskfield the window is a card, so Home is
    /// not covered by it and it does not outrank the shell's overlay.
    #[test]
    fn fullscreen_covers_chrome_only_while_normal() {
        let (mut service, ids) = service_with(1);
        let home = service.on_toplevel_mapped(
            9,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "home".into(),
            Rect::new(0, 0, 1920, 1080),
        );
        service
            .set_layer(home, Layer::HomeSurface, ClientKind::TrustedShell)
            .unwrap();
        service.set_role(home, WindowRole::Shell);
        service.set_fullscreen(ids[0], true);
        service.activate(ids[0]);

        assert!(!service.is_visible(home), "fullscreen covers Home");
        assert!(
            service.stacking_order().first() == Some(&ids[0]),
            "and outranks the shell's overlay"
        );

        assert!(service.enter_taskfield(TaskFilter::All));
        assert!(
            service.is_visible(home),
            "in Taskfield the fullscreen window is a card and covers nothing"
        );
        assert!(!service.stacking_order().is_empty());

        assert!(service.leave_taskfield());
        assert!(
            !service.is_visible(home),
            "and the scene comes back exactly"
        );
    }

    #[test]
    fn entering_taskfield_lays_out_once_and_leaving_throws_it_away() {
        let (mut service, ids) = service_with(3);
        service.activate(ids[1]);

        assert!(service.enter_taskfield(TaskFilter::All));
        let Presentation::Taskfield(state) = service.presentation() else {
            panic!("in the mode");
        };
        assert_eq!(state.entered_from, Some(ids[1]));
        assert_eq!(
            state.selected,
            Some(ids[1]),
            "selection starts where you were"
        );
        assert_eq!(state.cards().count(), 3);

        let rect = service.presentation_rect(ids[0]).expect("a card");
        assert_ne!(rect, service.window(ids[0]).unwrap().geometry);

        assert!(service.leave_taskfield());
        assert_eq!(service.presentation(), &Presentation::Normal);
        assert_eq!(
            service.presentation_rect(ids[0]),
            None,
            "no card position outlives the mode"
        );
        assert!(!service.leave_taskfield(), "leaving twice changes nothing");
    }

    #[test]
    fn taskfield_needs_windows_to_show() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        assert!(!service.enter_taskfield(TaskFilter::All));
        assert_eq!(service.presentation(), &Presentation::Normal);
    }

    #[test]
    fn mapping_a_shell_overlay_does_not_cancel_taskfield() {
        let (mut service, ids) = service_with(2);
        assert!(service.enter_taskfield(TaskFilter::All));
        let overlay = service.on_toplevel_mapped(
            9,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "taskfield".into(),
            Rect::new(0, 0, 1920, 1080),
        );
        service
            .set_layer(overlay, Layer::Presentation, ClientKind::TrustedShell)
            .unwrap();
        service.set_role(overlay, WindowRole::Overlay);
        assert!(
            matches!(service.presentation(), Presentation::Taskfield(_)),
            "the overlay that draws the mode must not cancel it"
        );
        assert!(service.is_visible(overlay));
        assert!(service.presentation_rect(ids[0]).is_some());
    }

    #[test]
    fn the_shell_switcher_surface_is_visible_only_during_a_walk() {
        let (mut service, ids) = service_with(2);
        let overlay = service.on_toplevel_mapped(
            9,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "switcher".into(),
            Rect::new(0, 0, 1920, 1080),
        );
        service
            .set_layer(overlay, Layer::Presentation, ClientKind::TrustedShell)
            .unwrap();
        service.set_role(overlay, WindowRole::Overlay);
        assert!(
            !service.is_visible(overlay),
            "no mode, no sheet over the desktop"
        );
        assert!(service.set_switch_walk(true));
        assert!(
            service.is_visible(overlay),
            "the walk is drawn by this surface"
        );
        assert!(
            ids.iter().all(|id| service.is_visible(*id)),
            "a walk is not the overview: windows stay on screen under it"
        );
        assert!(!service.set_switch_walk(true), "already showing");
        assert!(service.set_switch_walk(false));
        assert!(!service.is_visible(overlay));
    }

    #[test]
    fn mapping_an_application_leaves_taskfield() {
        let (mut service, _ids) = service_with(1);
        assert!(service.enter_taskfield(TaskFilter::All));
        let _fresh = service.on_toplevel_mapped(
            4,
            ClientKind::Application,
            WindowClaims::default(),
            "fresh".into(),
            Rect::new(40, 40, 400, 300),
        );
        assert_eq!(service.presentation(), &Presentation::Normal);
    }

    #[test]
    fn taskfield_shows_shelved_cards_and_hides_the_rest() {
        let (mut service, ids) = service_with(3);
        assert!(service.shelve(ids[2]));
        assert!(service.enter_taskfield(TaskFilter::Shelved));
        assert!(
            service.is_visible(ids[2]),
            "a mapped shelved card is a presentation-only view"
        );
        assert!(
            !service.is_visible(ids[0]),
            "a window without a card is not drawn at its real size"
        );
        assert!(
            !service.is_visible(ids[1]),
            "filtered-out live windows stay off the overview"
        );
        assert!(service.presentation_rect(ids[2]).is_some());
        assert_eq!(service.presentation_rect(ids[0]), None);
    }

    #[test]
    fn paging_overflow_reaches_every_window_of_an_oversized_unit() {
        let mut service = WindowService::new(
            FakeCache::default(),
            vec![Output::new("eDP-1", Rect::new(0, 0, 1366, 768), 1000)],
        );
        let ids: Vec<WindowId> = (0..40)
            .map(|index| {
                service.on_toplevel_mapped(
                    index + 1,
                    ClientKind::Application,
                    WindowClaims::default(),
                    format!("files {index}"),
                    Rect::new(40, 40, 800, 600),
                )
            })
            .collect();
        for id in &ids {
            service.update_identity(
                *id,
                CachedIdentity {
                    app_id: Some("org.templaros.Files".parse().unwrap()),
                    instance_id: None,
                    verified: true,
                    claim_mismatch: false,
                },
            );
        }
        let key = crate::TaskKey::App("org.templaros.Files".parse().unwrap());
        assert!(service.enter_taskfield(TaskFilter::Unit(key.clone())));
        let mut seen = HashSet::new();
        let pages = {
            let Presentation::Taskfield(state) = service.presentation() else {
                panic!("in the mode");
            };
            state.sections[0]
                .windows
                .len()
                .div_ceil(state.sections[0].cells.len().max(1))
        };
        for _ in 0..pages {
            let Presentation::Taskfield(state) = service.presentation() else {
                panic!("in the mode");
            };
            for card in state.cards() {
                seen.insert(card.window);
            }
            if seen.len() == ids.len() {
                break;
            }
            assert!(service.taskfield_overflow(key.clone()));
        }
        assert_eq!(seen.len(), ids.len());
        assert_eq!(
            service.window(ids[0]).unwrap().geometry,
            Rect::new(40, 40, 800, 600)
        );
    }

    #[test]
    fn a_query_dims_without_moving_cards_and_steers_selection() {
        let (mut service, ids) = service_with(3);
        service.windows.get_mut(&ids[0]).unwrap().title = "alpha".into();
        service.windows.get_mut(&ids[1]).unwrap().title = "beta".into();
        service.windows.get_mut(&ids[2]).unwrap().title = "gamma".into();
        assert!(service.enter_taskfield(TaskFilter::All));
        let before = service.presentation_rect(ids[0]);
        assert!(service.taskfield_query("beta".into()));
        assert_eq!(service.presentation_rect(ids[0]), before);
        assert!((service.taskfield_card_alpha(ids[1]) - 1.0).abs() < f32::EPSILON);
        assert!(service.taskfield_card_alpha(ids[0]) < 1.0);
        let Presentation::Taskfield(state) = service.presentation() else {
            panic!("in the mode");
        };
        assert_eq!(state.selected, Some(ids[1]));
    }

    #[test]
    fn enter_and_leave_do_not_change_window_geometry() {
        let (mut service, ids) = service_with(3);
        let before: Vec<_> = ids
            .iter()
            .map(|id| service.window(*id).unwrap().geometry)
            .collect();
        assert!(service.enter_taskfield(TaskFilter::All));
        assert!(service.leave_taskfield());
        let after: Vec<_> = ids
            .iter()
            .map(|id| service.window(*id).unwrap().geometry)
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn activating_a_shelved_card_unshelves_it() {
        let (mut service, ids) = service_with(2);
        assert!(service.shelve(ids[0]));
        assert!(service.enter_taskfield(TaskFilter::Shelved));
        assert!(service.activate(ids[0]));
        assert!(!service.window(ids[0]).unwrap().shelved);
        assert_eq!(service.presentation(), &Presentation::Normal);
        assert!(service.is_visible(ids[0]));
    }

    #[test]
    fn activating_a_card_leaves_the_mode_and_a_closed_window_loses_its_card() {
        let (mut service, ids) = service_with(2);
        assert!(service.enter_taskfield(TaskFilter::All));
        service.on_toplevel_destroyed(ids[0]);
        assert_eq!(service.presentation_rect(ids[0]), None);
        assert!(
            service.presentation_rect(ids[1]).is_some(),
            "the rest stays put"
        );

        assert!(service.activate(ids[1]));
        assert_eq!(service.presentation(), &Presentation::Normal);
    }

    #[test]
    fn the_query_and_the_selection_are_the_only_things_that_move() {
        let (mut service, ids) = service_with(2);
        assert!(service.enter_taskfield(TaskFilter::All));
        let before = service.presentation_rect(ids[0]);

        assert!(service.taskfield_query("smith".into()));
        assert!(!service.taskfield_query("smith".into()));
        assert!(service.taskfield_selected(Some(ids[1])));
        assert_eq!(service.presentation_rect(ids[0]), before);

        service.leave_taskfield();
        assert!(!service.taskfield_query("x".into()), "not in the mode");
        assert!(!service.taskfield_selected(None));
    }

    #[test]
    fn alt_tab_order_lists_every_open_window_parked_included_in_recency_order() {
        let (mut service, ids) = service_with(9);
        for id in &ids {
            service.activate(*id);
        }
        service.shelve(ids[4]);
        service.shelve(ids[0]);

        let order = service.alt_tab_order();
        let newest_first: Vec<WindowId> = ids.iter().rev().copied().collect();
        assert_eq!(
            order, newest_first,
            "all nine, parked ones in their recency position, no cap"
        );
        assert_eq!(service.switch_exclusion_reason(ids[4]), None);
        assert!(
            !service.switch_order().contains(&ids[4]),
            "the focus fallback still skips parked windows"
        );
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

    // ---- fullscreen, and the bar it covers -----------------------------------------------

    /// A machine with the shell's bar on it: the usable area stops 48px short of the bottom.
    fn service_with_a_bar() -> (WindowService<FakeCache>, WindowId, WindowId) {
        let mut service =
            WindowService::new(FakeCache::default(), vec![output_with_a_bottom_bar(48)]);
        let app = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(100, 100, 400, 300),
        );
        let bar = shell_surface(
            &mut service,
            Layer::ShellOverlay,
            Rect::new(0, 1080 - 48, 1920, 48),
        );
        (service, app, bar)
    }

    #[test]
    fn focused_popup_alone_outranks_fullscreen_and_keeps_client_state() {
        let (mut service, app, bar) = service_with_a_bar();
        let popup = shell_surface(
            &mut service,
            Layer::ShellOverlay,
            Rect::new(600, 300, 700, 480),
        );
        service.set_fullscreen(app, true);
        service.activate(app);
        let before = service.shell_toplevels();
        let order = service.stacking_order();
        assert!(order.iter().position(|id| *id == app) < order.iter().position(|id| *id == popup));
        assert!(service.set_focused_popup(Some(popup)));
        service.raise(app);
        let order = service.stacking_order();
        assert!(order.iter().position(|id| *id == popup) < order.iter().position(|id| *id == app));
        assert!(order.iter().position(|id| *id == app) < order.iter().position(|id| *id == bar));
        assert_eq!(service.shell_toplevels(), before);
        assert!(!service.home_revealed());
        service.set_mapped(popup, false);
        service.set_mapped(popup, true);
        let order = service.stacking_order();
        assert!(order.iter().position(|id| *id == app) < order.iter().position(|id| *id == popup));
    }

    /// "Genuinely full screen" is over the bar, not politely beside it: `usable()` is 1032 tall
    /// here and the window must be 1080. The window is deliberately *not* `is_recoverable` while
    /// it holds that rectangle — it has no tag strip to keep reachable, and the escape chord is
    /// its way out.
    #[test]
    fn a_fullscreen_window_fills_the_output_over_the_bars_zone() {
        let (mut service, app, _) = service_with_a_bar();
        let screen = service.outputs()[0].logical;

        let geometry = service.set_fullscreen(app, true).expect("a real change");

        assert_eq!(geometry, screen);
        assert_ne!(geometry, service.outputs()[0].usable());
        assert!(
            !service.has_frame(app),
            "a fullscreen window wears no frame"
        );
        assert_eq!(
            service.set_fullscreen(app, true),
            None,
            "already fullscreen"
        );
    }

    /// The restore slot is fullscreen's own: a window that was maximized first must come back to
    /// the maximized area, and the *floating* geometry the unmaximize needs must still be there.
    #[test]
    fn unfullscreen_restores_through_a_slot_maximize_never_touches() {
        let (mut service, app, _) = service_with_a_bar();
        let floating = service.window(app).unwrap().geometry;
        let maximized = service.set_maximized(app, true).expect("maximized");

        service.set_fullscreen(app, true);
        assert_eq!(service.set_fullscreen(app, false), Some(maximized));
        assert_eq!(service.set_maximized(app, false), Some(floating));
    }

    /// A hotplug or a scale change must not pull a fullscreen window off the full output, which
    /// is exactly what `repair_window` would do: its tag strip is below `usable()`.
    #[test]
    fn an_output_change_never_repairs_a_fullscreen_window() {
        let (mut service, app, _) = service_with_a_bar();
        let screen = service.set_fullscreen(app, true).expect("fullscreen");
        assert!(!crate::is_recoverable(screen, service.outputs()));

        let changed = service.on_outputs_changed(vec![output_with_a_bottom_bar(48)]);

        assert!(!changed.iter().any(|(id, _)| *id == app), "{changed:?}");
        assert_eq!(service.window(app).unwrap().geometry, screen);
    }

    /// The recovery route, and the reason a fullscreen window needs no recovery band.
    #[test]
    fn escape_returns_the_window_to_where_fullscreen_found_it() {
        let (mut service, app, _) = service_with_a_bar();
        let before = service.window(app).unwrap().geometry;
        service.set_fullscreen(app, true);

        assert_eq!(
            service.escape(Some(app)),
            Some(EscapeAction::ExitedFullscreen { window_id: app })
        );

        assert_eq!(service.window(app).unwrap().geometry, before);
        assert!(crate::is_recoverable(before, service.outputs()));
    }

    /// Fullscreen hides the shell bar, not an admitted critical notification or secure UI.
    #[test]
    fn a_fullscreen_window_outranks_the_overlay_and_nothing_above_it() {
        for (layer, fullscreen_wins) in [
            (Layer::SecureLock, false),
            (Layer::Recovery, false),
            (Layer::SystemModal, false),
            (Layer::ShellOverlay, true),
            (Layer::Notification, false),
        ] {
            let (mut service, app, _) = service_with_a_bar();
            // `SecureLock` and `Recovery` are compositor-owned; the rest are the shell's.
            let client = if layer.is_core_only() {
                ClientKind::CompositorCore
            } else {
                ClientKind::TrustedShell
            };
            let other = service.on_toplevel_mapped(
                2,
                client,
                WindowClaims::default(),
                String::new(),
                Rect::new(0, 0, 200, 200),
            );
            service
                .set_layer(other, layer, client)
                .expect("authorized for this layer");
            service.set_fullscreen(app, true);

            let order = service.stacking_order();
            let app_at = order.iter().position(|id| *id == app).unwrap();
            let other_at = order.iter().position(|id| *id == other).unwrap();

            assert_eq!(
                app_at < other_at,
                fullscreen_wins,
                "fullscreen against {layer}: {order:?}"
            );
        }
    }

    #[test]
    fn admitted_notifications_cover_fullscreen_without_changing_application_state() {
        let (mut service, app, _) = service_with_a_bar();
        service.activate(app);
        service.set_fullscreen(app, true);
        let before = service.window(app).unwrap().geometry;
        let work_before = service.shell_toplevels();
        let toast = shell_surface(
            &mut service,
            Layer::Notification,
            Rect::new(1500, 800, 380, 80),
        );
        let order = service.stacking_order();
        assert!(order.iter().position(|id| *id == toast) < order.iter().position(|id| *id == app));
        assert!(!service.should_activate_on_map(toast));
        assert!(service.window(app).unwrap().activated);
        assert_eq!(service.window(app).unwrap().geometry, before);
        assert_eq!(service.shell_toplevels(), work_before);
    }

    /// The whole of the peek: one flag, and the bar is back over the fullscreen window. No
    /// surface is destroyed, no zone is released and no rectangle moves, so nothing reflows
    /// either way — the compositor never runs `recompute_outputs` for a peek because there is
    /// nothing for it to recompute.
    #[test]
    fn a_peek_puts_the_overlay_back_on_top_and_nothing_else_moves() {
        let (mut service, app, bar) = service_with_a_bar();
        let screen = service.set_fullscreen(app, true).expect("fullscreen");
        let bar_geometry = service.window(bar).unwrap().geometry;
        let usable = service.outputs()[0].usable();
        let above = |service: &WindowService<FakeCache>| {
            let order = service.stacking_order();
            order.iter().position(|id| *id == bar).unwrap()
                < order.iter().position(|id| *id == app).unwrap()
        };
        assert!(!above(&service), "the fullscreen window covers the bar");

        assert!(service.set_peek_output(Some("eDP-1".to_owned())));
        assert!(above(&service), "the peek raises the bar");
        assert!(
            !service.set_peek_output(Some("eDP-1".to_owned())),
            "no change"
        );

        assert!(service.set_peek_output(None));
        assert!(!above(&service));
        // Neither the peek nor the fullscreen entry touched a rectangle.
        assert_eq!(service.window(app).unwrap().geometry, screen);
        assert_eq!(service.window(bar).unwrap().geometry, bar_geometry);
        assert_eq!(
            service.outputs()[0].usable(),
            usable,
            "no zone changed, so no window has anywhere new to be"
        );
        assert!(
            service.is_visible(bar),
            "the bar is never unmapped for this"
        );
    }

    /// A peek on the other screen leaves this one's fullscreen window on top.
    #[test]
    fn a_peek_is_one_output_at_a_time() {
        let (mut service, app, _) = service_with_a_bar();
        let bar = shell_surface(
            &mut service,
            Layer::ShellOverlay,
            Rect::new(0, 1080 - 48, 1920, 48),
        );
        service.set_fullscreen(app, true);
        service.set_peek_output(Some("HDMI-1".to_owned()));

        let order = service.stacking_order();
        assert!(
            order.iter().position(|id| *id == app).unwrap()
                < order.iter().position(|id| *id == bar).unwrap()
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

    /// Child 2 (`no-client-reconfigure`): the client rectangle is the geometry, whole, and
    /// pure tab moves — corner changes, policy changes, reveal — never touch it.
    #[test]
    fn pure_tab_moves_never_resize_the_client() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.set_tab_policy(TabPolicy {
            follow_pointer: true,
            ..TabPolicy::default()
        });
        let client = service.client_area(id).unwrap();
        assert_eq!(client, Rect::new(100, 100, 400, 300));

        for corner in Corner::ALL {
            service.set_tab_corner(id, corner);
            assert_eq!(service.client_area(id).unwrap(), client);
            assert_eq!(service.window_layout(id).unwrap().client, client);
        }
        let policy = TabPolicy {
            placement: TabPlacement::OppositeCorners,
            ..TabPolicy::default()
        };
        assert!(service.set_tab_policy(policy));
        assert!(!service.set_tab_policy(policy));
        assert_eq!(service.client_area(id).unwrap(), client);
        service.set_controls_revealed(id, true);
        assert_eq!(service.client_area(id).unwrap(), client);
        // Maximized geometry is still the whole client: no strip is subtracted anywhere.
        service.set_maximized(id, true);
        assert_eq!(
            service.client_area(id),
            service.window(id).map(|window| window.geometry)
        );
    }

    /// Child 2 (`recovery-and-consumers`): one layout for every consumer — paint and hit
    /// derive from `window_layout`, and the stored corner survives park/restore,
    /// maximize/fullscreen and regrouping.
    #[test]
    fn one_shared_layout_survives_transitions() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.set_tab_policy(TabPolicy {
            follow_pointer: true,
            ..TabPolicy::default()
        });
        assert_eq!(service.tab_corner(id), Corner::BottomRight);
        assert!(service.set_tab_corner(id, Corner::BottomLeft));
        assert!(!service.set_tab_corner(id, Corner::BottomLeft));
        let layout = service.window_layout(id).unwrap();
        assert_eq!(layout.corner, Corner::BottomLeft);
        assert_eq!(layout.placement, TabPlacement::SameCorner);

        service.shelve(id);
        service.unshelve(id);
        assert_eq!(service.tab_corner(id), Corner::BottomLeft);
        service.set_maximized(id, true);
        assert_eq!(service.tab_corner(id), Corner::BottomLeft);
        assert!(service.window_layout(id).unwrap().overlay);
        service.set_maximized(id, false);
        service.set_fullscreen(id, true);
        assert_eq!(service.tab_corner(id), Corner::BottomLeft);
        service.set_fullscreen(id, false);

        let other = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(600, 100, 400, 300),
        );
        service.group(id, other);
        assert_eq!(service.tab_corner(id), Corner::BottomLeft);
        assert_eq!(service.tab_corner(other), Corner::BottomRight);
        // Output loss re-derives the layout against the surviving topology without
        // forgetting the corner.
        service.on_outputs_changed(vec![Output::new("eDP-1", Rect::new(0, 0, 800, 600), 1000)]);
        assert_eq!(service.tab_corner(id), Corner::BottomLeft);
        assert!(service.window_layout(id).is_some());
        // Unknown windows answer the default corner and no layout.
        assert_eq!(service.tab_corner(WindowId(9999)), Corner::BottomRight);
        assert!(service.window_layout(WindowId(9999)).is_none());
    }

    /// Child 3 (`follow-and-latch`): selection follows the pointer until latched — aiming,
    /// pressing, dragging and resizing freeze the corner, and the latch releases from the
    /// grab's own termination path. Close never flees because the layout cannot move under it.
    #[test]
    fn latched_tabs_hold_their_corner_until_the_grab_ends() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.set_tab_policy(TabPolicy {
            follow_pointer: true,
            ..TabPolicy::default()
        });
        service.set_tab_corner(id, Corner::TopLeft);
        // Unlatched, the far corner wins.
        assert_eq!(
            service.select_tab_corner(id, Point { x: 900, y: 700 }),
            Some((Corner::TopLeft, Corner::BottomRight))
        );
        // Latched, nothing moves.
        assert!(service.set_tab_latched(id, true));
        assert!(!service.set_tab_latched(id, true));
        assert_eq!(
            service.select_tab_corner(id, Point { x: 0, y: 0 }),
            Some((Corner::BottomRight, Corner::BottomRight))
        );
        assert!(service.is_tab_latched(id));
        assert!(!service.is_tab_latched(WindowId(9999)));
        // A drag latches on begin and releases on finish — the real termination path.
        assert!(service.set_tab_latched(id, false));
        assert!(service.begin_drag(id, Point { x: 150, y: 150 }));
        assert!(service.is_tab_latched(id));
        assert_eq!(
            service.select_tab_corner(id, Point { x: 0, y: 0 }),
            Some((Corner::BottomRight, Corner::BottomRight))
        );
        service.finish_drag();
        assert!(!service.is_tab_latched(id));
        assert_eq!(
            service.select_tab_corner(id, Point { x: 0, y: 0 }),
            Some((Corner::BottomRight, Corner::TopLeft))
        );
        // Same for resize.
        assert!(service.begin_resize(
            id,
            Point { x: 500, y: 400 },
            crate::ResizeEdges::BOTTOM_RIGHT
        ));
        assert!(service.is_tab_latched(id));
        service.finish_resize();
        assert!(!service.is_tab_latched(id));
        // Destroying the window drops every scrap of tab state with it.
        service.set_tab_latched(id, true);
        service.set_controls_revealed(id, true);
        service.note_tab_approach(id, 0);
        service.on_toplevel_destroyed(id);
        assert!(!service.is_tab_latched(id));
        assert!(!service.has_tab_pending(id));
    }

    /// One pending deadline per purpose; reveal fires, exit hides controls but keeps
    /// the title and last corner, and a latched exit waits.
    #[test]
    fn tab_grace_deadlines_fire_once_and_never_pile() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.set_tab_corner(id, Corner::BottomRight);
        // Ten approaches note one deadline, and firing it reveals exactly once.
        for now in 0..10u64 {
            service.note_tab_approach(id, now);
        }
        assert!(service.has_tab_pending(id));
        assert!(service.fire_tab_deadlines(0).is_empty());
        let fired = service.fire_tab_deadlines(10_000_000_000);
        assert_eq!(fired, vec![TabDeadlineFired::Reveal(id)]);
        assert!(service.controls_revealed(id));
        assert!(service.fire_tab_deadlines(20_000_000_000).is_empty());
        assert!(!service.has_tab_pending(id));
        // Leaving arms the exit; firing hides the controls, keeps the title and the corner.
        service.note_tab_leave(id, 0);
        assert_eq!(
            service.next_tab_deadline(),
            Some(crate::geometry::TAB_EXIT_GRACE_MS * 1_000_000)
        );
        let fired = service.fire_tab_deadlines(u64::MAX);
        assert_eq!(fired, vec![TabDeadlineFired::Exit(id)]);
        assert!(!service.controls_revealed(id));
        assert!(service.title_revealed(id));
        assert_eq!(service.tab_corner(id), Corner::BottomRight);
        // A latched window's exit waits for the release, then fires.
        service.set_controls_revealed(id, true);
        service.set_tab_latched(id, true);
        service.note_tab_leave(id, 0);
        assert!(service.fire_tab_deadlines(u64::MAX).is_empty());
        assert!(service.controls_revealed(id));
        service.set_tab_latched(id, false);
        assert_eq!(
            service.fire_tab_deadlines(u64::MAX),
            vec![TabDeadlineFired::Exit(id)]
        );
        assert!(service.next_tab_deadline().is_none());
    }

    /// Child 3: reveal changes nothing but the reveal — no focus, no raise, no activation —
    /// and the fixed-corner fallback holds every window still when following is off.
    #[test]
    fn reveal_changes_nothing_and_fixed_corner_holds_still() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        assert!(!service.window(id).unwrap().activated);
        service.set_controls_revealed(id, true);
        assert!(!service.window(id).unwrap().activated);
        assert!(!service.set_controls_revealed(id, true));

        let policy = TabPolicy {
            follow_pointer: false,
            fixed_corner: Corner::BottomLeft,
            ..TabPolicy::default()
        };
        service.set_tab_policy(policy);
        assert_eq!(service.tab_corner(id), Corner::BottomLeft);
        assert_eq!(
            service.select_tab_corner(id, Point { x: 900, y: 0 }),
            Some((Corner::BottomLeft, Corner::BottomLeft))
        );
        assert_eq!(
            service.window_layout(id).unwrap().corner,
            Corner::BottomLeft
        );
    }

    /// Child 4 (`options-and-transitions`): both arrangements lay out from every corner, and
    /// the placement survives the same transitions the corner does.
    #[test]
    fn both_arrangements_serve_every_corner() {
        let (mut service, id) = one_window(Rect::new(300, 200, 640, 480));
        for placement in [TabPlacement::SameCorner, TabPlacement::OppositeCorners] {
            let policy = TabPolicy {
                placement,
                follow_pointer: true,
                ..TabPolicy::default()
            };
            service.set_tab_policy(policy);
            for corner in Corner::ALL {
                service.set_tab_corner(id, corner);
                let layout = service.window_layout(id).unwrap();
                assert_eq!(layout.corner, corner);
                assert_eq!(layout.placement, placement);
                assert_eq!(layout.controls.len(), 3);
                assert!(layout.title.width >= layout.title.height);
            }
        }
        service.set_maximized(id, true);
        assert!(service.window_layout(id).unwrap().overlay);
        service.set_maximized(id, false);
        assert!(!service.window_layout(id).unwrap().overlay);
    }

    #[test]
    fn continued_motion_does_not_postpone_or_rearm_tab_deadlines() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        let reveal = crate::TAB_REVEAL_GRACE_MS * 1_000_000;
        service.note_tab_approach(id, 0);
        service.note_tab_approach(id, reveal - 1);
        assert_eq!(service.next_tab_deadline(), Some(reveal));
        assert_eq!(
            service.fire_tab_deadlines(reveal),
            vec![TabDeadlineFired::Reveal(id)]
        );
        service.note_tab_approach(id, reveal + 1);
        assert_eq!(service.next_tab_deadline(), None);
        service.note_tab_leave(id, reveal);
        let exit = reveal + crate::TAB_EXIT_GRACE_MS * 1_000_000;
        service.note_tab_leave(id, exit - 1);
        assert_eq!(service.next_tab_deadline(), Some(exit));
        assert_eq!(
            service.fire_tab_deadlines(exit),
            vec![TabDeadlineFired::Exit(id)]
        );
    }

    #[test]
    fn privacy_and_visibility_transitions_retire_pending_tabs() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.note_tab_approach(id, 0);
        service.note_tab_leave(id, 1);
        assert_eq!(
            service.next_tab_deadline(),
            None,
            "an unrevealed leave needs no timer"
        );
        for transition in 0..5 {
            service.note_tab_approach(id, 0);
            service.set_controls_revealed(id, true);
            service.set_tab_latched(id, true);
            match transition {
                0 => service.clear_tab_interaction(),
                1 => {
                    service.shelve(id);
                }
                2 => {
                    service.set_mapped(id, false);
                }
                3 => {
                    service.set_home_revealed(true);
                }
                _ => {
                    service.on_outputs_changed(vec![Output::new(
                        "other",
                        Rect::new(0, 0, 800, 600),
                        1000,
                    )]);
                }
            }
            assert!(service.fire_tab_deadlines(u64::MAX).is_empty());
            assert!(!service.is_tab_latched(id));
            assert!(!service.controls_revealed(id));
            assert_eq!(service.next_tab_deadline(), None);
            service.set_mapped(id, true);
            service.unshelve(id);
            service.set_home_revealed(false);
        }
    }

    #[test]
    fn maximized_windows_keep_title_and_bounded_controls() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.set_tab_policy(TabPolicy {
            follow_pointer: true,
            ..TabPolicy::default()
        });
        service.set_maximized(id, true);
        assert!(service.title_revealed(id));
        assert!(!service.controls_revealed(id));
        for corner in Corner::ALL {
            service.set_tab_corner(id, corner);
            let layout = service.window_layout(id).unwrap();
            assert!(layout.overlay);
            assert!(layout.title.y >= 16);
            assert!(layout.title.bottom() <= service.outputs()[0].usable().bottom() - 16);
        }
    }

    #[test]
    fn default_tabs_stay_bottom_right_on_approach_and_after_restore() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        for pos in [Point { x: 0, y: 0 }, Point { x: 900, y: 0 }] {
            assert_eq!(
                service.select_tab_corner(id, pos),
                Some((Corner::BottomRight, Corner::BottomRight))
            );
            assert_eq!(
                service.window_layout(id).unwrap().corner,
                Corner::BottomRight
            );
        }
        service.set_fullscreen(id, true);
        service.set_fullscreen(id, false);
        assert_eq!(
            service.window_layout(id).unwrap().corner,
            Corner::BottomRight
        );
    }

    #[test]
    fn fullscreen_retires_tabs_and_blocks_hover_until_restored() {
        let (mut service, id) = one_window(Rect::new(100, 100, 400, 300));
        service.note_tab_approach(id, 0);
        service.set_controls_revealed(id, true);
        service.set_tab_latched(id, true);
        service.set_fullscreen(id, true);

        assert!(!service.title_revealed(id));
        assert!(!service.controls_revealed(id));
        assert!(
            service.window_layout(id).is_none(),
            "no clickable tab rectangles"
        );
        assert!(!service.is_tab_latched(id));
        assert_eq!(service.next_tab_deadline(), None);
        assert!(service.fire_tab_deadlines(u64::MAX).is_empty());
        service.note_tab_approach(id, 1);
        assert!(!service.set_controls_revealed(id, true));
        assert_eq!(service.next_tab_deadline(), None);

        service.set_tab_policy(TabPolicy {
            controls_always: true,
            ..TabPolicy::default()
        });
        assert!(!service.title_revealed(id));
        assert!(
            !service.controls_revealed(id),
            "fullscreen overrides always-controls"
        );

        service.set_tab_policy(TabPolicy::default());
        service.set_fullscreen(id, false);
        assert!(service.title_revealed(id));
        assert!(
            !service.controls_revealed(id),
            "old hover does not survive restore"
        );
        service.note_tab_approach(id, 0);
        assert_eq!(
            service.fire_tab_deadlines(u64::MAX),
            vec![TabDeadlineFired::Reveal(id)]
        );
    }
}
