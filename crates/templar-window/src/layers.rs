//! Typed overlay layers as a real z-order authority. Instead of arbitrary "always on top", the
//! stacking order is a total order over `(layer, within-layer rank)`, which client may request
//! which layer is fixed by [`may_request_layer`], and any two windows' relative order can be
//! explained rather than merely observed (planning doc 01 §3).

use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap, fmt, hash::Hash};

/// Compositor layers from top-most to bottom-most.
/// The discriminants are the **wire** values (`layer_to_wire`) and are append-only; the
/// **order** is [`Layer::ALL`]'s, read through [`Layer::depth`]. The two were the same number
/// until `Presentation` was added between `SystemModal` and `ShellOverlay`, which no free
/// discriminant could express — so `Ord` is written by hand over `ALL` and every comparison and
/// sort key goes through `depth`, never through `as u8`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Layer {
    /// Secure lock and authentication (`ext-session-lock-v1`).
    SecureLock = 0,
    /// Emergency system recovery (resource overlay, "show every window").
    Recovery = 1,
    /// System-modal requests (polkit, permission prompts).
    SystemModal = 2,
    /// Launcher, switcher and transient shell overlays.
    ShellOverlay = 3,
    /// Toasts admitted by the shell: above fullscreen work, below presentation and secure UI.
    Notification = 4,
    /// Application always-on-top surfaces.
    AppOnTop = 5,
    /// Normal windows.
    Normal = 6,
    /// The desktopless Home Surface — visible wherever no window is.
    HomeSurface = 7,
    /// Taskfield's chrome: the window overview's labels, headers and query field, drawn over the
    /// windows the compositor is presenting. Above `ShellOverlay` — and above the half step a
    /// fullscreen window takes over it (`WindowService::stacking_order`) — because Taskfield is
    /// explicit navigation and a fullscreen window may not cover it. Mapped only while
    /// `Presentation::Taskfield`; it reserves nothing.
    Presentation = 8,
}

impl Layer {
    /// All layers, top-most first. This — not the discriminant — is the stacking order.
    pub const ALL: [Layer; 9] = [
        Self::SecureLock,
        Self::Recovery,
        Self::SystemModal,
        Self::Presentation,
        Self::Notification,
        Self::ShellOverlay,
        Self::AppOnTop,
        Self::Normal,
        Self::HomeSurface,
    ];

    /// How far down the screen this layer sits: 0 is the top-most. The one ordering authority —
    /// [`Ord`] and every sort key are written over it, so inserting a layer is an edit to
    /// [`Self::ALL`] and nothing else.
    #[must_use]
    pub fn depth(self) -> u8 {
        let position = Self::ALL
            .into_iter()
            .position(|layer| layer == self)
            .expect("ALL lists every layer");
        u8::try_from(position).expect("nine layers")
    }

    /// Layers only the compositor core itself may create surfaces on: unreachable by any
    /// client, including the trusted shell.
    pub fn is_core_only(self) -> bool {
        matches!(self, Self::SecureLock | Self::Recovery)
    }

    /// Human name for explanations and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::SecureLock => "SecureLock",
            Self::Recovery => "Recovery",
            Self::SystemModal => "SystemModal",
            Self::ShellOverlay => "ShellOverlay",
            Self::Notification => "Notification",
            Self::AppOnTop => "AppOnTop",
            Self::Normal => "Normal",
            Self::HomeSurface => "HomeSurface",
            Self::Presentation => "Presentation",
        }
    }
}

/// By [`Layer::depth`], so a *lesser* layer draws *above* a greater one — the same direction the
/// discriminants used to give, now stated once where inserting a layer cannot silently reorder
/// the screen.
impl Ord for Layer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.depth().cmp(&other.depth())
    }
}

impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Who is asking to place a surface on a layer. Ordered by trust, least to most: an
/// `Application` connection is the least trusted, `CompositorCore` the most.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ClientKind {
    /// Any ordinary Wayland client.
    Application,
    /// The one client the compositor launched as the shell, bound to `templar_shell_v2`.
    TrustedShell,
    /// The compositor core itself (lock screen, recovery overlay, fallback switcher).
    CompositorCore,
}

impl ClientKind {
    /// Position in the trust order: higher trusts strictly more than lower.
    fn trust_rank(self) -> u8 {
        match self {
            Self::Application => 0,
            Self::TrustedShell => 1,
            Self::CompositorCore => 2,
        }
    }
}

/// The least-trusted [`ClientKind`] allowed to place a surface on `layer`.
///
/// `SecureLock` and `Recovery` are compositor-owned. `SystemModal`, `ShellOverlay`,
/// `Notification` and `HomeSurface` need at least the trusted shell — `SystemModal` covers
/// polkit/permission prompts, which the shell renders on the compositor's behalf. `AppOnTop` and
/// `Normal` are where ordinary toplevels live and need no elevated trust at all: the shell (an
/// about box, a file-conflict dialog) and even the compositor core map windows there too, the
/// same as any other client.
fn minimum_trust(layer: Layer) -> ClientKind {
    match layer {
        Layer::SecureLock | Layer::Recovery => ClientKind::CompositorCore,
        Layer::SystemModal
        | Layer::Presentation
        | Layer::ShellOverlay
        | Layer::Notification
        | Layer::HomeSurface => ClientKind::TrustedShell,
        Layer::AppOnTop | Layer::Normal => ClientKind::Application,
    }
}

/// Fixed authorization table: which [`ClientKind`] may place a surface on which [`Layer`].
///
/// Monotone by construction: `may_request_layer` is `client.trust_rank() >=
/// minimum_trust(layer).trust_rank()`, so a client trusted enough for one layer is trusted
/// enough for every layer with a lower [`minimum_trust`] — a `TrustedShell` or `CompositorCore`
/// connection is never refused `Layer::Normal` just because it is not literally an
/// `Application`. Without this property, the *most* trusted callers would be the ones most
/// likely to be refused the *least* sensitive layer, which is a trap: `on_toplevel_mapped`
/// passes through whatever `ClientKind` actually mapped the window, and a refusal there must
/// never be able to reach an unwrap/expect on the render thread (see its doc comment).
pub fn may_request_layer(client: ClientKind, layer: Layer) -> bool {
    client.trust_rank() >= minimum_trust(layer).trust_rank()
}

/// A window's exact position in the total stacking order: its layer, and how recently it was
/// raised within that layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct StackPosition {
    pub layer: Layer,
    /// Assigned from a monotonically increasing counter; a higher rank was raised more
    /// recently and therefore draws above a lower rank in the same layer.
    pub rank: u64,
}

impl StackPosition {
    /// True when `self` draws above `other`. A total order: a lower [`Layer`] discriminant
    /// always wins, and within one layer a higher rank always wins.
    pub fn is_above(self, other: Self) -> bool {
        self > other
    }

    /// One sentence explaining the relative order of `self` and `other`.
    pub fn explain(self, other: Self) -> String {
        if self.layer != other.layer {
            let (top, bottom) = if self.layer < other.layer {
                (self.layer, other.layer)
            } else {
                (other.layer, self.layer)
            };
            format!("{top} sits above {bottom} in the fixed layer order")
        } else if self.rank == other.rank {
            format!("both are in {} at the same rank", self.layer)
        } else {
            let (later, earlier) = if self.rank > other.rank {
                (self.rank, other.rank)
            } else {
                (other.rank, self.rank)
            };
            format!(
                "both are in {}; rank {later} was raised after rank {earlier}",
                self.layer
            )
        }
    }
}

/// Ordered so that a *greater* `StackPosition` draws *above* a lesser one: `Layer`'s
/// top-most-first discriminant order is reversed here, then within a layer a higher rank wins.
impl Ord for StackPosition {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .layer
            .cmp(&self.layer)
            .then(self.rank.cmp(&other.rank))
    }
}

impl PartialOrd for StackPosition {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Why a layer request was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayerError {
    pub layer: Layer,
    pub client: ClientKind,
}

impl fmt::Display for LayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} may not request layer {}",
            self.client, self.layer
        )
    }
}

/// The z-order authority: every surface's [`StackPosition`], keyed by an opaque `Id` (in
/// practice `WindowId`). Kept generic and dependency-free so it can be tested and reused without
/// pulling in window/session state.
#[derive(Debug)]
pub struct LayerStack<Id> {
    positions: HashMap<Id, StackPosition>,
    next_rank: u64,
}

impl<Id> Default for LayerStack<Id> {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            next_rank: 0,
        }
    }
}

impl<Id: Copy + Eq + Hash> LayerStack<Id> {
    pub fn new() -> Self {
        Self::default()
    }

    fn fresh_rank(&mut self) -> u64 {
        let rank = self.next_rank;
        self.next_rank = self.next_rank.saturating_add(1);
        rank
    }

    /// Place `id` on `layer`, at the top of that layer. Fails if `client` may not request
    /// `layer` ([`may_request_layer`]); `id`'s previous position, if any, is left untouched.
    pub fn assign(
        &mut self,
        id: Id,
        layer: Layer,
        client: ClientKind,
    ) -> Result<StackPosition, LayerError> {
        if !may_request_layer(client, layer) {
            return Err(LayerError { layer, client });
        }
        let position = StackPosition {
            layer,
            rank: self.fresh_rank(),
        };
        self.positions.insert(id, position);
        Ok(position)
    }

    /// Re-rank `id` to the top of its current layer (no layer change, no authorization check:
    /// raising an already-placed window is always allowed). `None` if `id` has no position.
    pub fn raise(&mut self, id: Id) -> Option<StackPosition> {
        let layer = self.positions.get(&id)?.layer;
        let position = StackPosition {
            layer,
            rank: self.fresh_rank(),
        };
        self.positions.insert(id, position);
        Some(position)
    }

    /// Raise several ids at once, keeping the order given: `ids[0]` ends up top-most, each of the
    /// rest immediately below the one before it. Ids with no position are skipped.
    ///
    /// This is how a window group is raised as one thing ([`crate::WindowService::activate`]):
    /// each member gets a consecutive fresh rank, so nothing outside the group can end up
    /// stacked between two of its members — a *contiguous* raise, which "raise each of them in
    /// turn" only accidentally is. Like [`Self::raise`], it changes rank and nothing else: no
    /// layer changes, no geometry changes, no authorization question.
    pub fn raise_many(&mut self, ids: &[Id]) {
        for id in ids.iter().rev() {
            self.raise(*id);
        }
    }

    pub fn remove(&mut self, id: Id) {
        self.positions.remove(&id);
    }

    pub fn position(&self, id: Id) -> Option<StackPosition> {
        self.positions.get(&id).copied()
    }

    /// Every placed id, top-most first.
    pub fn order(&self) -> Vec<Id> {
        let mut entries: Vec<(Id, StackPosition)> =
            self.positions.iter().map(|(id, pos)| (*id, *pos)).collect();
        entries.sort_by_key(|(_, position)| std::cmp::Reverse(*position));
        entries.into_iter().map(|(id, _)| id).collect()
    }

    /// One sentence explaining why `a` is above or below `b`. `None` if either has no position.
    pub fn explain(&self, a: Id, b: Id) -> Option<String> {
        Some(self.position(a)?.explain(self.position(b)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_compositor_core_reaches_secure_lock_and_recovery() {
        for layer in [Layer::SecureLock, Layer::Recovery] {
            assert!(may_request_layer(ClientKind::CompositorCore, layer));
            assert!(!may_request_layer(ClientKind::TrustedShell, layer));
            assert!(!may_request_layer(ClientKind::Application, layer));
        }
    }

    #[test]
    fn shell_layers_need_at_least_the_trusted_shell() {
        for layer in [Layer::ShellOverlay, Layer::Notification, Layer::HomeSurface] {
            assert!(may_request_layer(ClientKind::TrustedShell, layer));
            // Monotone: the compositor core is trusted at least as much as the shell.
            assert!(may_request_layer(ClientKind::CompositorCore, layer));
            assert!(!may_request_layer(ClientKind::Application, layer));
        }
    }

    /// The discriminants are the wire encoding and the depths are the screen order, and since
    /// `Presentation` they are different numbers. Every comparison must use the depth.
    #[test]
    fn presentation_sits_between_system_modal_and_the_shell_overlay() {
        assert!(Layer::SystemModal < Layer::Presentation);
        assert!(Layer::Presentation < Layer::ShellOverlay);
        assert!(
            Layer::Presentation as u8 > Layer::HomeSurface as u8,
            "not by discriminant"
        );

        let above = StackPosition {
            layer: Layer::Presentation,
            rank: 0,
        };
        let below = StackPosition {
            layer: Layer::ShellOverlay,
            rank: u64::MAX,
        };
        assert!(above.is_above(below), "no rank buys a lower layer the top");
    }

    #[test]
    fn depths_are_the_order_all_lists_them_in() {
        for (index, layer) in Layer::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(layer.depth()), index, "{layer}");
        }
    }

    #[test]
    fn taskfields_chrome_is_the_trusted_shells_to_claim() {
        assert!(may_request_layer(
            ClientKind::TrustedShell,
            Layer::Presentation
        ));
        assert!(!may_request_layer(
            ClientKind::Application,
            Layer::Presentation
        ));
    }

    #[test]
    fn applications_reach_only_normal_and_app_on_top() {
        for layer in Layer::ALL {
            let allowed = may_request_layer(ClientKind::Application, layer);
            assert_eq!(allowed, matches!(layer, Layer::Normal | Layer::AppOnTop));
        }
    }

    #[test]
    fn normal_and_app_on_top_are_reachable_by_every_client_kind() {
        // The trap this guards against: the *most* trusted callers being refused the *least*
        // sensitive layers just because they are not literally an `Application`.
        for layer in [Layer::Normal, Layer::AppOnTop] {
            assert!(may_request_layer(ClientKind::Application, layer));
            assert!(may_request_layer(ClientKind::TrustedShell, layer));
            assert!(may_request_layer(ClientKind::CompositorCore, layer));
        }
    }

    #[test]
    fn trust_is_monotone() {
        let order = [
            ClientKind::Application,
            ClientKind::TrustedShell,
            ClientKind::CompositorCore,
        ];
        for layer in Layer::ALL {
            let allowed: Vec<bool> = order
                .iter()
                .map(|client| may_request_layer(*client, layer))
                .collect();
            // Once a client in trust order is allowed, every more-trusted client after it must
            // also be allowed — there is no layer a more-trusted `ClientKind` is refused that a
            // less-trusted one is granted.
            for window in allowed.windows(2) {
                assert!(!window[0] || window[1], "trust is not monotone for {layer}");
            }
        }
    }

    #[test]
    fn stack_position_orders_by_layer_before_rank() {
        let top = StackPosition {
            layer: Layer::SecureLock,
            rank: 0,
        };
        let bottom = StackPosition {
            layer: Layer::HomeSurface,
            rank: 1_000_000,
        };
        assert!(top.is_above(bottom));
        assert!(top > bottom);
    }

    #[test]
    fn later_rank_wins_within_a_layer() {
        let early = StackPosition {
            layer: Layer::Normal,
            rank: 1,
        };
        let late = StackPosition {
            layer: Layer::Normal,
            rank: 2,
        };
        assert!(late.is_above(early));
    }

    #[test]
    fn explain_names_the_layer_rule() {
        let lock = StackPosition {
            layer: Layer::SecureLock,
            rank: 0,
        };
        let normal = StackPosition {
            layer: Layer::Normal,
            rank: 5,
        };
        assert_eq!(
            lock.explain(normal),
            "SecureLock sits above Normal in the fixed layer order"
        );

        let raised = StackPosition {
            layer: Layer::Normal,
            rank: 9,
        };
        assert_eq!(
            raised.explain(normal),
            "both are in Normal; rank 9 was raised after rank 5"
        );
    }

    #[test]
    fn layer_stack_assign_enforces_authorization() {
        let mut stack: LayerStack<u32> = LayerStack::new();
        assert!(
            stack
                .assign(1, Layer::SecureLock, ClientKind::TrustedShell)
                .is_err()
        );
        assert!(
            stack
                .assign(1, Layer::HomeSurface, ClientKind::TrustedShell)
                .is_ok()
        );
    }

    #[test]
    fn raising_keeps_layer_and_moves_to_top_of_it() {
        let mut stack: LayerStack<u32> = LayerStack::new();
        stack
            .assign(1, Layer::Normal, ClientKind::Application)
            .unwrap();
        stack
            .assign(2, Layer::Normal, ClientKind::Application)
            .unwrap();
        // 2 was assigned after 1, so 2 starts above 1.
        assert_eq!(stack.order(), vec![2, 1]);

        stack.raise(1);
        assert_eq!(stack.order(), vec![1, 2]);
        assert_eq!(stack.position(1).unwrap().layer, Layer::Normal);
    }

    #[test]
    fn a_higher_layer_always_beats_a_lower_layer_regardless_of_rank() {
        let mut stack: LayerStack<u32> = LayerStack::new();
        stack
            .assign(1, Layer::HomeSurface, ClientKind::TrustedShell)
            .unwrap();
        for _ in 0..10 {
            stack.raise(1);
        }
        stack
            .assign(2, Layer::Normal, ClientKind::Application)
            .unwrap();
        assert_eq!(stack.order(), vec![2, 1]);
        assert_eq!(
            stack.explain(2, 1).unwrap(),
            "Normal sits above HomeSurface in the fixed layer order"
        );
    }

    /// A window group is raised as one thing: the members end up on top in the order given, with
    /// nothing from outside the group stacked between any two of them.
    #[test]
    fn raise_many_lifts_a_set_contiguously_in_the_order_given() {
        let mut stack: LayerStack<u32> = LayerStack::new();
        for id in 1..=4 {
            stack
                .assign(id, Layer::Normal, ClientKind::Application)
                .unwrap();
        }
        // 3 and 1 are the group; 4 is an unrelated window that was on top.
        stack.raise_many(&[3, 1]);
        assert_eq!(stack.order(), vec![3, 1, 4, 2]);
    }

    #[test]
    fn raise_many_ignores_ids_with_no_position() {
        let mut stack: LayerStack<u32> = LayerStack::new();
        stack
            .assign(1, Layer::Normal, ClientKind::Application)
            .unwrap();
        stack.raise_many(&[9, 1]);
        assert_eq!(stack.order(), vec![1]);
    }

    #[test]
    fn removed_windows_leave_the_stack() {
        let mut stack: LayerStack<u32> = LayerStack::new();
        stack
            .assign(1, Layer::Normal, ClientKind::Application)
            .unwrap();
        stack.remove(1);
        assert!(stack.position(1).is_none());
        assert!(stack.order().is_empty());
    }
}
