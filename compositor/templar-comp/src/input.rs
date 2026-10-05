//! Input: pointer motion/button/axis, keyboard, and the drag grab. COMPOSITOR.md's rows for
//! "pointer button down on a window's decoration/recovery band", "pointer motion during a drag"
//! and "pointer button up ending a drag" all live here, plus the fixed recovery keybindings
//! (`src/keybindings.rs`) intercepted before a key ever reaches a client.
//!
//! Coordinates are converted from device/physical space to the one global logical scene exactly
//! once, at `AbsolutePositionEvent::position_transformed` — the same boundary
//! `templar_window::geometry::physical_to_logical` exists for — and never again: everything from
//! `MotionEvent` onward (`WindowService::update_drag`, hit-testing, focus) stays in that one
//! logical space, matching `crates/templar-window`'s own invariant.

use crate::chrome::layout::{self, ControlTag};
use crate::state::{Backend, TemplarCompState};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
        InputBackend, InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent,
    },
    desktop::WindowSurfaceType,
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::FilterResult,
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GestureHoldBeginEvent,
            GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent,
            PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::calloop::timer::{TimeoutAction, Timer},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        seat::WaylandFocus,
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
        },
    },
};
use templar_window::{Layer, Point as TemplarPoint, RevealHome, WindowId};

use std::time::Duration;

impl<B: Backend + 'static> SeatHandler for TemplarCompState<B> {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // While the compositor is showing a resize cursor of its own, a client's choice is
        // remembered rather than displayed: it is what the pointer goes back to on the way out,
        // and letting it win here would flicker the perimeter cursor away on every frame a
        // client re-sets its cursor.
        if let Some(saved) = self.cursor_before_perimeter.as_mut() {
            *saved = image;
        } else {
            self.cursor_status = image;
        }
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|surface| dh.get_client(surface.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

impl<B: Backend + 'static> SelectionHandler for TemplarCompState<B> {
    type SelectionUserData = ();
}

impl<B: Backend + 'static> DataDeviceHandler for TemplarCompState<B> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl<B: Backend + 'static> smithay::input::dnd::DndGrabHandler for TemplarCompState<B> {}
impl<B: Backend + 'static> WaylandDndGrabHandler for TemplarCompState<B> {}

/// Give `window_id` keyboard focus: `WindowService::activate` plus the matching
/// `xdg_toplevel.configure{activated}` round trip on every affected toplevel — the
/// newly-activated one and whichever previously held it (`WindowService::activate` already
/// cleared its `activated` flag; this just makes Smithay/the client agree). Used both from an
/// ordinary pointer click here and from a successful xdg-activation grant
/// (`src/xdg_shell.rs::XdgActivationHandler::request_activation`).
pub fn activate_window<B: Backend + 'static>(state: &mut TemplarCompState<B>, window_id: WindowId) {
    // Who had the keyboard *before* this — what `Super+G` groups the focused window with. Read
    // before the activation, because `WindowService::activate` clears the flag it comes from.
    let outgoing = state
        .window_service
        .windows()
        .find(|window| window.activated)
        .map(|window| window.id)
        .filter(|id| *id != window_id);
    let was_shelved = state
        .window_service
        .window(window_id)
        .is_some_and(|window| window.shelved);
    if !state.window_service.activate(window_id) {
        return;
    }
    if was_shelved
        && let Some(geometry) = state.window_service.window(window_id).map(|w| w.geometry)
    {
        // Unshelve may have repaired the rectangle against the current topology; the client
        // has to hear the size, the same way a shelf restore from the protocol does.
        crate::space_sync::apply_resized_geometry(state, window_id, geometry, false);
    }
    if let Some(outgoing) = outgoing {
        state.previous_focus = Some(outgoing);
    }
    let Some(wl_surface) = state.windows.surface_for(window_id).cloned() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.set_focus(state, Some(wl_surface.clone()), serial);
    }
    // Every toplevel the compositor knows about, not only the ones `Space` currently shows: a
    // shelved or reveal-hidden window is unmapped from `Space` (`src/space_sync.rs`) and would
    // otherwise keep the `activated` state it had when it went away, so two windows would come
    // back believing they both had the keyboard.
    for window in state.windows.elements().cloned().collect::<Vec<_>>() {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let is_target = toplevel.wl_surface() == &wl_surface;
        toplevel.with_pending_state(|pending| {
            if is_target {
                pending.states.set(XdgState::Activated);
            } else {
                pending.states.unset(XdgState::Activated);
            }
        });
        toplevel.send_pending_configure();
    }
    crate::space_sync::sync_after_change(state);
}

/// Keyboard focus after a "go home" reveal started (`revealed`) or ended. Starting one hands the
/// keyboard to the shell's `Layer::HomeSurface` window, so the Home Field's own keyboard path
/// works while every client window is hidden; ending one activates the topmost client window
/// that is visible again, so typing never lands in a window nobody can see.
pub fn refocus_after_reveal<B: Backend + 'static>(state: &mut TemplarCompState<B>, revealed: bool) {
    if revealed {
        let home = state
            .window_service
            .stacking_order()
            .into_iter()
            .find(|id| {
                state.window_service.layer_of(*id) == Some(templar_window::Layer::HomeSurface)
            });
        let surface = home.and_then(|id| state.windows.surface_for(id).cloned());
        if let Some(keyboard) = state.seat.get_keyboard() {
            keyboard.set_focus(state, surface, SERIAL_COUNTER.next_serial());
        }
        return;
    }
    let top = state
        .window_service
        .switch_order()
        .into_iter()
        .find(|id| state.window_service.is_visible(*id));
    if let Some(window_id) = top {
        activate_window(state, window_id);
    }
}

/// Move keyboard focus away from an application that just became unmapped. The semantic MRU is
/// authoritative here too: visual stacking may include shell chrome and may have changed while
/// the surface was disappearing. If no application remains, Home becomes the keyboard target.
pub fn refocus_after_window_hidden<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    if let Some(window_id) = state
        .window_service
        .switch_order()
        .into_iter()
        .find(|id| state.window_service.is_visible(*id))
    {
        activate_window(state, window_id);
        return;
    }

    let home = state
        .window_service
        .stacking_order()
        .into_iter()
        .find(|id| {
            state.window_service.layer_of(*id) == Some(templar_window::Layer::HomeSurface)
                && state.window_service.is_visible(*id)
        });
    let surface = home.and_then(|id| state.windows.surface_for(id).cloned());
    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.set_focus(state, surface, SERIAL_COUNTER.next_serial());
    }
    crate::space_sync::sync_after_change(state);
}

/// Focus the Home surface when it is the only usable desktop, or while a reveal explicitly owns
/// the screen. Initial shell mapping otherwise leaves the seat with no keyboard target, which
/// makes a correctly rendered Home Field look completely inert.
pub fn focus_home_if_needed<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    surface: &WlSurface,
) {
    let has_visible_client =
        state.window_service.stacking_order().into_iter().any(|id| {
            state.window_service.is_client_window(id) && state.window_service.is_visible(id)
        });
    if (state.window_service.home_revealed() || !has_visible_client)
        && let Some(keyboard) = state.seat.get_keyboard()
    {
        if keyboard.current_focus().is_some_and(|focused| {
            state
                .windows
                .window_id_for(&focused.id())
                .is_some_and(|id| {
                    state.window_service.is_visible(id)
                        && matches!(
                            state.window_service.layer_of(id),
                            Some(
                                templar_window::Layer::ShellOverlay
                                    | templar_window::Layer::SystemModal
                            )
                        )
                })
        }) {
            return;
        }
        keyboard.set_focus(state, Some(surface.clone()), SERIAL_COUNTER.next_serial());
    }
}

/// What a primary-button press on a window's compositor-owned chrome asks for
/// ([`press_target_at`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Affordance {
    /// One of the control tags (`chrome::layout::control_tags`).
    Button(ControlTag),
    /// The name tag: a move.
    Tag,
    /// Just outside the frame: a resize of these edges.
    Edge(templar_window::ResizeEdges),
}

/// One window as the press hit test sees it — everything [`press_target_at`] needs and nothing
/// else, so the decision can be exercised with no compositor, no `Space` and no display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PressCandidate {
    window_id: WindowId,
    /// The window's *frame* (tag strip included), which is what owns the point.
    geometry: templar_window::Rect,
    is_client: bool,
    has_frame: bool,
    maximized: bool,
    /// The tag strip along the bottom edge, when the window wears one (`has_frame`) or is being
    /// shown one over itself (`covering_chrome`).
    strip: Option<templar_window::Rect>,
    /// Whether that strip is drawn *over* the window's own pixels rather than below them — true
    /// for a screen-filling window's chrome, false for an ordinary window's frame, where the
    /// strip is compositor-owned screen the client does not occupy.
    strip_over_client: bool,
}

/// What a pointer press at a point is asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PressTarget {
    /// A compositor-owned affordance on this window.
    Affordance(WindowId, Affordance),
    /// This window's own area: the press belongs to the client (or to the shell surface), and
    /// the window takes focus first.
    Window(WindowId),
    /// No window `WindowService` knows owns this point.
    Nothing,
}

/// The press target at `pos`, given every visible window top-first.
///
/// Pure, and deliberately so: this is the one place that decides which window a press belongs
/// to, and getting it wrong is indistinguishable from "the windows fight for focus".
fn press_target_at(candidates: &[PressCandidate], pos: TemplarPoint) -> PressTarget {
    for candidate in candidates {
        if rect_contains(candidate.geometry, pos) {
            if !candidate.is_client {
                return PressTarget::Window(candidate.window_id);
            }
            if let Some(strip) = candidate.strip
                && rect_contains(strip, pos)
            {
                if let Some(tag) = layout::control_tag_at(strip, pos) {
                    return PressTarget::Affordance(candidate.window_id, Affordance::Button(tag));
                }
                if rect_contains(layout::name_tag(strip), pos) {
                    return PressTarget::Affordance(candidate.window_id, Affordance::Tag);
                }
                // The gap between the two tags. On a framed window no pixels of it are there, so
                // the press belongs to whatever is behind. On a screen-filling window the strip
                // is drawn over the window's own client area, and falling through would raise
                // the window *underneath* on every click along the bottom row of the one in
                // front.
                if candidate.strip_over_client {
                    return PressTarget::Window(candidate.window_id);
                }
                continue;
            }
            return PressTarget::Window(candidate.window_id);
        }
        if candidate.has_frame
            && !candidate.maximized
            && let Some(edges) = templar_window::resize_edges_at(candidate.geometry, pos)
        {
            return PressTarget::Affordance(candidate.window_id, Affordance::Edge(edges));
        }
    }
    PressTarget::Nothing
}

/// What a primary-button press-and-release in the window overview turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskfieldRelease {
    /// A click. Go to this window and leave; `None` — a click on the scrim — leaves and activates
    /// nothing, because looking at your windows is not choosing one.
    Activate(Option<WindowId>),
    /// A card dragged out of its own section and dropped on another's. Group the two and leave.
    /// `anchor` is the section's *first* card, so the group whose identity survives a merge is the
    /// one that was already there (`window_groups::group_windows`).
    Group { dragged: WindowId, anchor: WindowId },
    /// A drag that landed nowhere. Nothing happens and the mode stays up: a gesture the user
    /// started and then thought better of must not also close what they were looking at.
    Cancel,
    /// A click on an overflowing section's header: show the next page of that unit.
    Overflow(usize),
}

/// Decide what a press at `pressed_at` released at `released_at` asked for, over the frozen
/// layout: one `Vec` of `(window, card)` per section, in the order [`templar_window::taskfield`]
/// laid them out.
///
/// Pure, and the whole of the gesture: the mode already owns the pointer, so there is no grab
/// state, no threshold timer and nothing to unwind — a press point and a release point are
/// everything, and the same [`is_click`] slop the name-tag pin uses separates the two readings.
fn taskfield_release(
    sections: &[Vec<(WindowId, templar_window::Rect)>],
    overflow_headers: &[(usize, templar_window::Rect)],
    pressed_at: TemplarPoint,
    released_at: TemplarPoint,
) -> TaskfieldRelease {
    let card_at = |pos: TemplarPoint| {
        sections.iter().enumerate().find_map(|(index, cards)| {
            cards
                .iter()
                .find(|(_, rect)| rect_contains(*rect, pos))
                .map(|(window_id, _)| (index, *window_id))
        })
    };
    let from = card_at(pressed_at);
    if is_click(pressed_at, released_at) {
        if from.is_none()
            && let Some(&(index, _)) = overflow_headers
                .iter()
                .find(|(_, header)| rect_contains(*header, pressed_at))
        {
            return TaskfieldRelease::Overflow(index);
        }
        return TaskfieldRelease::Activate(from.map(|(_, window_id)| window_id));
    }
    // A drag has to start on a card and end on another section's: dragging from the scrim relates
    // nothing to anything, and dropping inside the section a card is already in is a no-op that
    // must not be spelled as a regroup.
    let (Some((source, dragged)), Some((target, _))) = (from, card_at(released_at)) else {
        return TaskfieldRelease::Cancel;
    };
    if source == target {
        return TaskfieldRelease::Cancel;
    }
    match sections.get(target).and_then(|cards| cards.first()) {
        Some((anchor, _)) => TaskfieldRelease::Group {
            dragged,
            anchor: *anchor,
        },
        None => TaskfieldRelease::Cancel,
    }
}

/// Which window a press should bring forward before it does anything else, or `None` when it
/// should bring none forward.
///
/// Everything a window owns focuses it: its client area, its name tag and its edge zones. The two
/// exceptions are the close and park tags — activating a window in
/// order to close it, or to send it to the shelf a frame later, would take the keyboard away from
/// whatever the user was typing into and hand it to something that is about to disappear.
fn press_activates(target: PressTarget) -> Option<WindowId> {
    match target {
        PressTarget::Affordance(_, Affordance::Button(ControlTag::Close | ControlTag::Park))
        | PressTarget::Nothing => None,
        PressTarget::Affordance(window_id, _) | PressTarget::Window(window_id) => Some(window_id),
    }
}

/// The band's park button: put `window_id` on the Home shelf and hand the keyboard to whatever
/// is left on top — the next visible client window, or Home when there is none — exactly as
/// Ctrl+Alt+P does for the activated window (`keybindings::park_focused`).
fn park_window<B: Backend + 'static>(state: &mut TemplarCompState<B>, window_id: WindowId) {
    let ctx = templar_window::CommandContext {
        trusted_shell: true,
        now_monotonic_ns: crate::window_bridge::now_monotonic_ns(&state.clock),
    };
    if state
        .window_service
        .handle_command(templar_window::WindowCommand::Shelve { window_id }, ctx)
        .is_err()
    {
        return;
    }
    crate::space_sync::sync_after_change(state);
    let any_left =
        state.window_service.stacking_order().into_iter().any(|id| {
            state.window_service.is_visible(id) && state.window_service.is_client_window(id)
        });
    refocus_after_reveal(state, !any_left);
}

/// How a gesture came to be requested, which decides where its grab-start data comes from.
///
/// A client request (`xdg_toplevel.move`/`resize`, or the XWayland equivalent) must present the
/// serial of the button press that motivated it — the implicit grab Smithay installed when that
/// press was delivered is both the proof the press happened and the start data. The compositor's
/// own affordances (recovery band, resize grip) hit-test the press *before* it is routed through
/// `PointerHandle::button`, deliberately, so the press never reaches a client — but that also
/// means no implicit grab exists yet at that moment, so demanding one silently disabled both
/// affordances. Their start data is synthesized here instead: the press is the compositor's own
/// observation, there is no client claim to validate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GestureOrigin {
    ClientRequest,
    CompositorAffordance,
}

/// Start a client-requested (`xdg_toplevel.move`) drag: `begin_drag` and install
/// a [`WindowGestureGrab`]. `pointer_global` is already logical — see the module doc.
pub fn start_drag<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    seat: &Seat<TemplarCompState<B>>,
    serial: Serial,
    window_id: WindowId,
) {
    start_gesture(
        state,
        seat,
        serial,
        window_id,
        Gesture::Move,
        GestureOrigin::ClientRequest,
    );
}

/// Start a client-requested interactive resize dragging `edges`: `begin_resize` and install a
/// [`WindowGestureGrab`]. Reached from `xdg_toplevel.resize` and its XWayland equivalent (a
/// client-drawn resize border).
pub fn start_resize<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    seat: &Seat<TemplarCompState<B>>,
    serial: Serial,
    window_id: WindowId,
    edges: templar_window::ResizeEdges,
) {
    start_gesture(
        state,
        seat,
        serial,
        window_id,
        Gesture::Resize { edges },
        GestureOrigin::ClientRequest,
    );
}

fn start_gesture<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    seat: &Seat<TemplarCompState<B>>,
    serial: Serial,
    window_id: WindowId,
    gesture: Gesture,
    origin: GestureOrigin,
) {
    let Some(pointer) = seat.get_pointer() else {
        return;
    };
    let start_data = match origin {
        GestureOrigin::ClientRequest => {
            if !pointer.has_grab(serial) {
                return;
            }
            let Some(start_data) = pointer.grab_start_data() else {
                return;
            };
            start_data
        }
        // See `GestureOrigin`: at band/grip press time the press has not been routed through
        // `PointerHandle::button` yet, so there is no implicit grab to inherit — and must not
        // be, because this press is swallowed rather than delivered to any client.
        GestureOrigin::CompositorAffordance => PointerGrabStartData {
            focus: None,
            button: BTN_LEFT,
            location: pointer.current_location(),
        },
    };
    let pointer_global = to_templar_point(pointer.current_location());
    let started = match gesture {
        Gesture::Move => state.window_service.begin_drag(window_id, pointer_global),
        Gesture::Resize { edges } => {
            state
                .window_service
                .begin_resize(window_id, pointer_global, edges)
        }
    };
    if !started {
        return;
    }
    // A band drag moves the whole group; `Shift` held moves that member alone (planning doc
    // §2.1). Decided once, here, from the modifiers as the gesture starts — mid-drag the answer
    // must not change under the pointer.
    let followers = match gesture {
        Gesture::Move if !shift_held(state) => {
            crate::window_groups::drag_followers(state, window_id)
        }
        _ => Vec::new(),
    };
    // Latched for the whole gesture: the pointer routinely leaves the perimeter mid-resize (that
    // is what growing a window *is*), and the shape must not go with it.
    if let Gesture::Resize { edges } = gesture {
        state.show_compositor_cursor(edges.cursor());
    }
    let grab = WindowGestureGrab {
        pressed_at: to_templar_point(start_data.location),
        start_data,
        window_id,
        gesture,
        followers,
        // A press on a name tag that never moves is a click, and a click on a name tag pins the
        // window. Only from the tag: `xdg_toplevel.move` is a client asking to be dragged, and a
        // client-side title bar that happens to receive a bare click must not pin anything.
        pins_on_click: origin == GestureOrigin::CompositorAffordance
            && matches!(gesture, Gesture::Move),
    };
    pointer.set_grab(state, grab, serial, Focus::Clear);
}

/// Linux kernel `input-event-codes.h` `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;

/// Whether either Shift is held right now, from the seat's own modifier state — the one modifier
/// a pointer gesture consults, and only at the moment it starts.
fn shift_held<B: Backend + 'static>(state: &TemplarCompState<B>) -> bool {
    state
        .seat
        .get_keyboard()
        .is_some_and(|keyboard| keyboard.modifier_state().shift)
}

/// The pointer's position as a logical pixel *index*, which is what every rectangle here is in.
///
/// Floored, not rounded. Smithay's pointer is `f64` and an output's last row is `y` in
/// `[1079.0, 1080.0)`; rounding sends the top half of that row to 1080, which is outside the
/// output, so a pointer resting on the very bottom edge found no output at all and armed no
/// dwell — the one place a person is most likely to put it. Flooring keeps the last row on the
/// screen it is on, and matches how a rectangle's `bottom()` is already exclusive.
fn to_templar_point(point: Point<f64, Logical>) -> TemplarPoint {
    TemplarPoint {
        x: point.x.floor() as i32,
        y: point.y.floor() as i32,
    }
}

/// Which gesture one [`WindowGestureGrab`] is running. The two differ only in which
/// `WindowService` pair they drive and how the result reaches the client — a move changes where
/// `Space` puts the window, a resize also has to be configured onto the client — so they share
/// one grab rather than two near-identical copies of Smithay's forwarding boilerplate.
#[derive(Clone, Copy, Debug)]
enum Gesture {
    Move,
    Resize { edges: templar_window::ResizeEdges },
}

/// The interactive move/resize grab. Every event but `motion`/`button` is forwarded to Smithay's
/// default handling unchanged — a drag has no opinion about scroll wheels or gestures — matching
/// the shape every Smithay compositor's move grab takes (`anvil`, `smallvil`, `cosmic-comp`).
/// `motion` and `button` are the two rows COMPOSITOR.md specifies: the pointer's position applied
/// as-is, never clamped, and the invariant enforced once at release.
pub struct WindowGestureGrab<B: Backend + 'static> {
    start_data: PointerGrabStartData<TemplarCompState<B>>,
    window_id: WindowId,
    gesture: Gesture,
    /// Where the press landed, so the release can tell a click from a drag.
    pressed_at: TemplarPoint,
    /// Whether a release that never left [`CLICK_SLOP`] of [`Self::pressed_at`] pins the window.
    pins_on_click: bool,
    /// The other members of the dragged window's group, with their offsets from it: a band drag
    /// moves the whole group. Empty for a resize, for an ungrouped window and for a `Shift`-held
    /// drag, which moves that member alone.
    followers: Vec<(WindowId, templar_window::Point)>,
}

impl<B: Backend + 'static> std::fmt::Debug for WindowGestureGrab<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowGestureGrab")
            .field("window_id", &self.window_id)
            .field("gesture", &self.gesture)
            .finish_non_exhaustive()
    }
}

impl<B: Backend + 'static> PointerGrab<TemplarCompState<B>> for WindowGestureGrab<B> {
    fn motion(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        let pointer = to_templar_point(event.location);
        // Below the click threshold a name-tag press is still a candidate click, and a click must
        // not nudge the window three pixels on its way to pinning it. `update_drag` works from
        // the recorded drag origin, so the first motion past the threshold applies the whole
        // displacement and nothing is lost by not applying the ones before it.
        if self.pins_on_click && is_click(self.pressed_at, pointer) {
            return;
        }
        match self.gesture {
            Gesture::Move => {
                if let Some((_, geometry)) = data.window_service.update_drag(pointer) {
                    crate::space_sync::apply_dragged_geometry(data, self.window_id, geometry);
                    // The rest of the group rides along at its own offset — no repair while the
                    // gesture is in flight, exactly as for the dragged window itself.
                    for (id, offset) in &self.followers {
                        let Some(size) = data.window_service.window(*id).map(|w| w.geometry) else {
                            continue;
                        };
                        let placed = templar_window::Rect::new(
                            geometry.x.saturating_add(offset.x),
                            geometry.y.saturating_add(offset.y),
                            size.width,
                            size.height,
                        );
                        data.window_service.set_geometry(*id, placed);
                        crate::space_sync::apply_dragged_geometry(data, *id, placed);
                    }
                }
                // Which band, if any, this drag is hovering: releasing there groups the two
                // (planning doc §2 rule 6). Only a band — an ordinary drop anywhere else,
                // including on top of another window, still just moves a window.
                data.drop_target =
                    crate::window_groups::tag_drop_target(data, self.window_id, pointer);
            }
            Gesture::Resize { .. } => {
                if let Some((_, geometry)) = data.window_service.update_resize(pointer) {
                    crate::space_sync::apply_resized_geometry(data, self.window_id, geometry, true);
                }
            }
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&BTN_LEFT) {
            // A name tag that was pressed and let go without moving is a click, and a click on a
            // name tag pins the window above the others. Checked before the drag is finished, so
            // "did it move" is asked of the pointer rather than of a geometry that a repair may
            // have changed for reasons of its own.
            let released_at = to_templar_point(handle.current_location());
            if self.pins_on_click && is_click(self.pressed_at, released_at) {
                data.window_service.finish_drag();
                data.drop_target = None;
                data.toggle_always_on_top(self.window_id);
                data.show_compositor_cursor(None);
                handle.unset_grab(self, data, event.serial, event.time, true);
                return;
            }
            match self.gesture {
                Gesture::Move => {
                    // Capture the unrepaired anchor *before* `finish_drag` writes the repaired
                    // rectangle. `move_group` computes its displacement from the anchor's current
                    // geometry; after the repair that delta is zero and followers miss the
                    // displacement that put the anchor back on screen.
                    let unrepaired = data
                        .window_service
                        .window(self.window_id)
                        .map(|window| window.geometry);
                    if let Some((_, geometry)) = data.window_service.finish_drag() {
                        crate::space_sync::apply_dragged_geometry(data, self.window_id, geometry);
                        if !self.followers.is_empty() && unrepaired.is_some() {
                            for (id, offset) in &self.followers {
                                let Some(size) =
                                    data.window_service.window(*id).map(|w| w.geometry)
                                else {
                                    continue;
                                };
                                let placed = templar_window::Rect::new(
                                    geometry.x.saturating_add(offset.x),
                                    geometry.y.saturating_add(offset.y),
                                    size.width,
                                    size.height,
                                );
                                let repaired = templar_window::repair_window(
                                    placed,
                                    data.window_service.outputs(),
                                );
                                data.window_service.set_geometry(*id, repaired);
                                crate::space_sync::apply_dragged_geometry(data, *id, repaired);
                            }
                        }
                    }
                    // Dropped on another window's band: that is the gesture that groups
                    // (planning doc §2 rule 6). It happens after the drag has settled, so the
                    // placement `place_beside` computes starts from where the window was let go.
                    if let Some(target) = data.drop_target.take()
                        && target != self.window_id
                    {
                        crate::window_groups::group_windows(data, target, self.window_id);
                    }
                }
                Gesture::Resize { .. } => {
                    if let Some((_, geometry)) = data.window_service.finish_resize() {
                        // `still_resizing: false` clears `xdg_toplevel.state.resizing`, which is
                        // how a client knows to stop throttling its own relayout and settle.
                        crate::space_sync::apply_resized_geometry(
                            data,
                            self.window_id,
                            geometry,
                            false,
                        );
                    }
                }
            }
            // Whatever the gesture was, the pointer goes back to the client's cursor; the next
            // motion event re-derives the perimeter shape if it is still over one.
            data.show_compositor_cursor(None);
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }
    fn gesture_swipe_update(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }
    fn gesture_swipe_end(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }
    fn gesture_pinch_begin(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }
    fn gesture_pinch_update(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }
    fn gesture_pinch_end(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }
    fn gesture_hold_begin(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }
    fn gesture_hold_end(
        &mut self,
        data: &mut TemplarCompState<B>,
        handle: &mut PointerInnerHandle<'_, TemplarCompState<B>>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<TemplarCompState<B>> {
        &self.start_data
    }

    /// The grab ending for any reason at all — including one this grab did not handle, such as
    /// the seat being taken away — clears the drop-target highlight, so a band can never be left
    /// glowing at a drag that is over. It also drops any in-flight drag/resize and the
    /// compositor cursor, because cancellation used to leave both latched with nothing to
    /// finish them.
    fn unset(&mut self, data: &mut TemplarCompState<B>) {
        data.drop_target = None;
        let _ = data.window_service.finish_drag();
        let _ = data.window_service.finish_resize();
        data.show_compositor_cursor(None);
    }
}

impl<B: Backend + 'static> TemplarCompState<B> {
    /// The one entry point for every backend's input events (COMPOSITOR.md's pointer/keyboard
    /// rows). `output_name` is `None` on udev (multi-output; the event already names its output)
    /// and `Some(name)` on winit (single synthetic output, whose name the winit backend supplies).
    pub fn process_input_event<I: InputBackend>(
        &mut self,
        event: InputEvent<I>,
        output_name: Option<&str>,
    ) {
        // Every event, before it is dispatched anywhere: this is the session's only idle source
        // (`src/idle.rs`), and "any input at all" is the whole condition.
        self.note_activity();
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard::<I>(&event),
            InputEvent::PointerMotion { event, .. } => self.on_pointer_motion::<I>(&event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.on_pointer_motion_absolute::<I>(&event, output_name);
            }
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<I>(&event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(&event),
            InputEvent::DeviceAdded { device } => log_input_device::<I>("added", &device),
            InputEvent::DeviceRemoved { device } => log_input_device::<I>("removed", &device),
            _ => {}
        }
    }

    /// The pressure overlay's modal keyboard capture. `true` when the overlay took the key and
    /// nothing else should look at it.
    fn overlay_took_key<I: InputBackend>(
        &mut self,
        keyboard: &smithay::input::keyboard::KeyboardHandle<Self>,
        event: &I::KeyboardKeyEvent,
        serial: Serial,
        time: u32,
    ) -> bool {
        // The pressure overlay is modal while it is showing: every key is captured here, never
        // forwarded to whatever client happened to have focus before it appeared (COMPOSITOR.md's
        // "Layer::Recovery ... above everything except the lock screen"). Gated on `!locked` for
        // the same reason `run_recovery_action` below is: a hostile overlay state must not become
        // a way to interact with anything while the session is locked (`src/session_lock.rs`'s
        // module doc on partial input isolation).
        if self.session_lock.locked || self.recovery_protocol.pressure_overlay.is_none() {
            return false;
        }
        let pressed = event.state() == smithay::backend::input::KeyState::Pressed;
        let action = keyboard.input::<Option<crate::keybindings::OverlayAction>, _>(
            self,
            event.key_code(),
            event.state(),
            serial,
            time,
            |state, modifiers, handle| {
                // The overlay owns the keyboard, but a switcher walk must still see the chord
                // come up, or its HUD outlives the hold (`on_keyboard` ends the walk).
                state.chord_held = modifiers.alt || modifiers.logo;
                // Always `Intercept`, even for a key that maps to nothing: `Forward` would
                // hand it to the client behind the overlay, which is exactly what a modal
                // overlay must not do.
                if !pressed {
                    return FilterResult::Intercept(None);
                }
                FilterResult::Intercept(crate::keybindings::overlay_action(
                    *modifiers,
                    handle.modified_sym(),
                ))
            },
        );
        if let Some(Some(action)) = action {
            self.apply_overlay_action(action);
        }
        true
    }

    fn shell_popup_focused(&self, surface: Option<&WlSurface>) -> bool {
        surface.is_some_and(|surface| {
            self.windows.window_id_for(&surface.id()).is_some_and(|id| {
                matches!(
                    self.window_service.layer_of(id),
                    Some(templar_window::Layer::ShellOverlay | templar_window::Layer::SystemModal)
                )
            })
        })
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: &I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(event);
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        if self.overlay_took_key::<I>(&keyboard, event, serial, time) {
            // The overlay took this key, so a walk in progress is over: it chooses nothing, since
            // activating a window behind a modal overlay would be a change nobody saw happen.
            self.cancel_switch_walk();
            return;
        }

        let home_revealed = self.window_service.home_revealed();
        // A focused shell popup owns plain Escape before the underlying Home reveal.
        // Read focus outside keyboard.input: querying it inside the filter would re-lock it.
        let popup_focused = self.shell_popup_focused(keyboard.current_focus().as_ref());
        // The window overview is modal: every key is the compositor's while it is up, and none
        // reaches the client behind a card. The reserved combinations still run first, so
        // `Super+Tab` closes the mode, `Ctrl+Alt+Escape` still escapes, and a bare Super still
        // goes Home — a modal mode that swallowed the recovery chords would be a trap.
        let in_taskfield = matches!(
            self.window_service.presentation(),
            templar_window::Presentation::Taskfield(_)
        );
        // Escape closes an open rail and still goes wherever it was going: the rail takes no
        // keyboard, so whatever has focus may well be waiting for this key too.
        let mut escape_pressed = false;
        // Escape during a switcher walk cancels it, and is the compositor's, not the client's.
        let walking = self.switch_ring.is_some();
        // A modifier going down mid-walk — Shift, to walk backwards — is part of the chord, not
        // "some other key", and must not cancel it.
        let mut modifier_pressed = false;
        let action = keyboard.input::<Option<crate::keybindings::RecoveryAction>, _>(
            self,
            event.key_code(),
            event.state(),
            serial,
            time,
            |state, modifiers, handle| {
                let keysym = handle.modified_sym();
                escape_pressed = event.state() == smithay::backend::input::KeyState::Pressed
                    && keysym.raw() == smithay::input::keyboard::keysyms::KEY_Escape;
                modifier_pressed = event.state() == smithay::backend::input::KeyState::Pressed
                    && keysym.is_modifier_key();
                // The switcher walks a list frozen for the length of the hold; this is where the
                // release that commits it is seen. Read from the modifier state rather than
                // matched by keysym, so either Alt (or either Super) ending the walk cannot be
                // missed.
                state.chord_held = modifiers.alt || modifiers.logo;
                match state
                    .super_tap
                    .update(event.state(), keysym, time, home_revealed)
                {
                    crate::keybindings::SuperTapDecision::Intercept => {
                        return FilterResult::Intercept(None);
                    }
                    crate::keybindings::SuperTapDecision::Reveal => {
                        state.pending_super_reveal = Some(templar_window::RevealHome::Show);
                        return FilterResult::Intercept(None);
                    }
                    crate::keybindings::SuperTapDecision::SetReveal(revealed) => {
                        state.pending_super_reveal = Some(if revealed {
                            templar_window::RevealHome::Show
                        } else {
                            templar_window::RevealHome::Hide
                        });
                        return FilterResult::Intercept(None);
                    }
                    crate::keybindings::SuperTapDecision::ForwardEndingReveal => {
                        // A chord key arrived during the momentary reveal: end it first, so the
                        // chord's own action (which may itself be a reveal toggle, e.g. Super+D)
                        // computes from the restored state, then let the key take its normal
                        // path below.
                        state.pending_super_reveal = Some(templar_window::RevealHome::Hide);
                    }
                    crate::keybindings::SuperTapDecision::Forward => {}
                }
                if event.state() != smithay::backend::input::KeyState::Pressed {
                    // A release of a key whose press was swallowed must be swallowed too, or the
                    // client behind a card sees half a keystroke it never got the start of.
                    if in_taskfield {
                        return FilterResult::Intercept(None);
                    }
                    return FilterResult::Forward;
                }
                match crate::keybindings::recovery_action(
                    *modifiers,
                    keysym,
                    home_revealed && !popup_focused,
                ) {
                    Some(action) => FilterResult::Intercept(Some(action)),
                    None if in_taskfield => FilterResult::Intercept(
                        crate::keybindings::taskfield_key(*modifiers, keysym)
                            .map(crate::keybindings::RecoveryAction::Taskfield),
                    ),
                    // Swallowed, and then ends the walk below like any other key press — but
                    // the client never sees an Escape meant for the switcher.
                    None if walking && escape_pressed => FilterResult::Intercept(None),
                    None => FilterResult::Forward,
                }
            },
        );

        // Apply the gesture's reveal change before any chord action, so an action that toggles
        // the reveal itself (Super+D) computes from the restored state. Applied here, not inside
        // the filter closure: `reveal_home` refocuses the keyboard, which must not re-enter
        // `keyboard.input`. Dropped while locked, exactly like `run_recovery_action` below.
        if let Some(reveal) = self.pending_super_reveal.take()
            && !self.session_lock.locked
        {
            self.reveal_home(reveal);
        }

        if escape_pressed && self.rail.is_some() {
            self.close_rail("escape");
        }

        // A key press that is not a modifier — Escape included — ends a walk unless it is a step.
        // A *release* must not: clearing on every forwarded key is what used to kill the HUD one
        // keystroke after it appeared, on the release of the very Tab that opened it.
        let ends_walk =
            event.state() == smithay::backend::input::KeyState::Pressed && !modifier_pressed;
        self.finish_key(action.flatten(), ends_walk);
    }

    /// After the keyboard filter: run the reserved action the key performed, end a switcher walk
    /// the key interrupted (choosing nothing), and commit the walk if the chord has come up.
    fn finish_key(&mut self, action: Option<crate::keybindings::RecoveryAction>, ends_walk: bool) {
        if let Some(action) = action {
            // Any reserved action other than another switcher step ends the walk choosing
            // nothing; `run_recovery_action` itself moves the ring and redraws the HUD for a step.
            if !matches!(
                action,
                crate::keybindings::RecoveryAction::CycleFocusForward
                    | crate::keybindings::RecoveryAction::CycleFocusBackward
                    | crate::keybindings::RecoveryAction::WithinUnitForward
                    | crate::keybindings::RecoveryAction::WithinUnitBackward
            ) {
                self.cancel_switch_walk();
            }
            if !self.session_lock.locked {
                self.run_recovery_action(action);
            }
        } else if ends_walk {
            self.cancel_switch_walk();
        }
        if !self.chord_held {
            // The chord came up: the walk commits to whatever it is on (a no-op with no walk).
            self.commit_switch_walk();
        }
    }

    fn on_pointer_motion<I: InputBackend>(&mut self, event: &I::PointerMotionEvent) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let current = pointer.current_location();
        let current_under = self.surface_under(current);
        pointer.relative_motion(
            self,
            current_under,
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                utime: event.time(),
            },
        );

        // Walked twice at most (once to test containment, once to find the nearest), and never
        // collected: this runs for every relative motion report there is.
        let space = &self.space;
        let position = clamp_to_outputs(current + event.delta(), || {
            space
                .outputs()
                .filter_map(|output| space.output_geometry(output))
        });
        let under = self.surface_under(position);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: position,
                serial: SERIAL_COUNTER.next_serial(),
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
        self.update_perimeter_cursor(to_templar_point(position));
        self.update_dwell(to_templar_point(position));
        self.update_gesture_gate(current, position);
    }

    /// Apply one pressure-overlay keyboard step (`src/keybindings.rs::overlay_action`) — move the
    /// highlighted choice, or send it back to whichever connection asked for the overlay
    /// (`protocols_recovery.rs::RecoveryProtocolState::confirm_selected`).
    fn apply_overlay_action(&mut self, action: crate::keybindings::OverlayAction) {
        match action {
            crate::keybindings::OverlayAction::MovePrevious => {
                self.recovery_protocol.move_selection(false);
            }
            crate::keybindings::OverlayAction::MoveNext => {
                self.recovery_protocol.move_selection(true);
            }
            crate::keybindings::OverlayAction::Confirm => {
                self.recovery_protocol.confirm_selected();
            }
        }
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: &I::PointerMotionAbsoluteEvent,
        output_name: Option<&str>,
    ) {
        let Some(output) = (match output_name {
            Some(name) => self.space.outputs().find(|o| o.name() == name),
            None => self.space.outputs().next(),
        }) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        // The one place device coordinates become the global logical scene (module doc).
        let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer();
        let Some(pointer) = pointer else { return };
        let previous = pointer.current_location();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
        self.update_perimeter_cursor(to_templar_point(pos));
        self.update_dwell(to_templar_point(pos));
        self.update_gesture_gate(previous, pos);
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: &I::PointerButtonEvent) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();

        // A press on anything that is not the shell's ends an open rail — the person has moved on
        // to something else. A press on the rail itself (or any shell surface) is the rail's to
        // act on, and the shell decides whether that closes it. The press is delivered either way.
        if button_state == ButtonState::Pressed
            && self.rail.is_some()
            && !self.press_lands_on_shell(pointer.current_location())
        {
            self.close_rail("press outside the shell");
        }

        // Same modality as `on_keyboard`'s overlay branch: a click while the overlay is up picks
        // a button (or does nothing, on a miss) and is never forwarded to a client underneath.
        if !self.session_lock.locked && self.recovery_protocol.pressure_overlay.is_some() {
            if button_state == ButtonState::Pressed {
                self.handle_pressure_overlay_click(pointer.current_location());
            }
            return;
        }

        // The window overview owns the pointer, and it needs the *release* as much as the press:
        // a click chooses a card, and a drag beyond `CLICK_SLOP` drops one card's window into
        // another section's group. `swallowing_press` would eat the release before this could see
        // it, so the mode is asked first.
        if !self.session_lock.locked
            && matches!(
                self.window_service.presentation(),
                templar_window::Presentation::Taskfield(_)
            )
            && self.taskfield_button(pointer.current_location(), button, button_state)
        {
            return;
        }

        if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
            // A click means the user is doing something else — a switcher walk is over, and
            // chooses nothing: the click itself says where the user is going.
            self.cancel_switch_walk();
            let compositor_owned =
                self.handle_press_for_focus_or_drag(pointer.current_location(), serial, button);
            // A grab installed by that press owns the matching release (`WindowGestureGrab::button`
            // finishes the drag). Swallowing the pair is how a tag/edge grab used to never see
            // `BTN_LEFT` come up. Only affordances that act and return — close, park, maximize,
            // pin — still swallow the release; they installed no grab to deliver it to.
            let grab_installed = self
                .seat
                .get_pointer()
                .is_some_and(|grabbed| grabbed.is_grabbed());
            self.swallowing_press = swallow_matching_release(compositor_owned, grab_installed);
            if compositor_owned {
                return;
            }
        }
        // A press the compositor took is not also the client's. Where a grab was installed this
        // makes no difference (the grab has `Focus::Clear`), but a tag that acts and returns —
        // close, park, unmaximize, pin — installs none, and under a screen-filling window's chrome
        // the surface with pointer focus is that window's own client, right there under the strip.
        // The release goes with the press: half a click is worse than none.
        if self.swallowing_press {
            if ButtonState::Released == button_state {
                self.swallowing_press = false;
            }
            return;
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Hit-test `location` (global logical) against whichever output it falls on and, if it lands
    /// on one of the pressure overlay's four drawn buttons (`chrome::layout::pressure_choice_at`,
    /// the same layout `chrome::mod::collect_elements` draws), select and immediately confirm it
    /// — a click is both "select" and "activate" for a button, unlike keyboard navigation's two
    /// separate steps. A miss (clicking elsewhere on the overlay) does nothing, matching a modal
    /// dialog with no implicit dismiss.
    fn handle_pressure_overlay_click(&mut self, location: Point<f64, Logical>) {
        let pos = to_templar_point(location);
        let Some(output) = self.space.outputs().find(|output| {
            self.space
                .output_geometry(output)
                .is_some_and(|geometry| geometry.to_f64().contains(location))
        }) else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(output) else {
            return;
        };
        let area = templar_window::Rect::new(
            geometry.loc.x,
            geometry.loc.y,
            geometry.size.w,
            geometry.size.h,
        );
        if let Some(choice) = crate::chrome::layout::pressure_choice_at(area, pos) {
            self.recovery_protocol.select(choice);
            self.recovery_protocol.confirm_selected();
        }
    }

    /// The press target under `pos`, from `WindowService` alone.
    ///
    /// Windows are walked top-first (`WindowService::stacking_order`), and window geometry — not
    /// `Space`'s own input-region test — is the hit-test authority, so the band always wins
    /// over whatever the client's surface would have done with the same point (COMPOSITOR.md:
    /// "the compositor-owned recovery band" wins over client content). The first window whose
    /// frame contains the point settles it: a band button, the band, the grip, or the window
    /// itself because the point is the client's. A point just *outside* a framed window's edge
    /// is that window's resize zone (`templar_window::resize_edges_at`), and a window above another
    /// takes its zone before the one beneath sees the press — the invisible border belongs to
    /// the window on top.
    fn press_target_at(&self, pos: TemplarPoint) -> PressTarget {
        press_target_at(&self.press_candidates(), pos)
    }

    /// Every visible window as the hit-test sees it, top first. Shared by the press path and the
    /// motion path, so what the cursor promises and what a press then does cannot disagree.
    fn press_candidates(&self) -> Vec<PressCandidate> {
        // In the window overview a window owns its *card* and nothing else — the same rectangle
        // the renderer drew it into (`WindowService::presentation_rect`), so a card is clicked
        // exactly where it is seen. It wears no frame there: no band, no resize zone, no control
        // tags, because none of them was drawn.
        if matches!(
            self.window_service.presentation(),
            templar_window::Presentation::Taskfield(_)
        ) {
            return self
                .window_service
                .stacking_order()
                .into_iter()
                .filter_map(|window_id| {
                    Some(PressCandidate {
                        window_id,
                        geometry: self.window_service.presentation_rect(window_id)?,
                        is_client: true,
                        has_frame: false,
                        maximized: false,
                        strip: None,
                        strip_over_client: false,
                    })
                })
                .collect();
        }
        self.window_service
            .stacking_order()
            .into_iter()
            .filter(|window_id| self.window_service.is_visible(*window_id))
            .filter_map(|window_id| {
                let record = self.window_service.window(window_id)?;
                let framed = self.window_service.has_frame(window_id);
                let chrome_shown = self.window_service.covering_chrome() == Some(window_id);
                Some(PressCandidate {
                    window_id,
                    geometry: record.geometry,
                    is_client: self.window_service.is_client_window(window_id),
                    has_frame: framed || chrome_shown,
                    maximized: record.maximized,
                    strip: (framed || chrome_shown)
                        .then(|| {
                            self.window_service
                                .recovery_affordance(window_id)
                                .map(|affordance| affordance.strip)
                        })
                        .flatten(),
                    strip_over_client: !framed && chrome_shown,
                })
            })
            .collect()
    }

    /// Show the resize a press here would start, and put the client's cursor back on the way out.
    ///
    /// Called from both motion paths (relative on udev, absolute on winit) rather than one, and
    /// never while a gesture is running — `start_gesture` has already latched the shape for the
    /// duration of the drag, and the pointer leaving the perimeter mid-resize must not take it
    /// away. Locked sessions show nothing of the windows behind the lock surface, cursor included.
    fn update_perimeter_cursor(&mut self, pos: TemplarPoint) {
        if self.session_lock.locked || self.pointer_gesture_active() {
            return;
        }
        // While a card is being dragged the selection ring is the drop target: one ring, already
        // drawn, moved — rather than a second widget that would be the dragged card's ghost. The
        // ring is on the card the release would group *into*, which is the thing the gesture is
        // about.
        if let Some(pressed_at) = self.taskfield_press
            && !is_click(pressed_at, pos)
            && let PressTarget::Window(window_id) = press_target_at(&self.press_candidates(), pos)
            && self.window_service.taskfield_selected(Some(window_id))
        {
            crate::protocols_shell::send_taskfield_state(self);
            B::request_redraw(self);
            return;
        }
        let shape = match press_target_at(&self.press_candidates(), pos) {
            PressTarget::Affordance(_, Affordance::Edge(edges)) => edges.cursor(),
            _ => None,
        };
        self.show_compositor_cursor(shape);
    }

    /// A dwell at a screen edge: the **top-left** corner is Home ([`DwellIntent::HomeCorner`]),
    /// the bottom-right corner of a window filling the screen is that window's own chrome
    /// ([`DwellIntent::CoveringChrome`]), and the whole bottom edge — bottom-left corner included
    /// — is Peek ([`DwellIntent::BarPeek`]).
    ///
    /// Both have to be the compositor's rule rather than the shell's: whatever is under the
    /// pointer owns it, so under a maximized or fullscreen window the shell never learns the
    /// pointer reached the edge at all. And both need a timer, because a client that is not
    /// animating sends no further motion once the pointer stops — "rested here for a moment" is
    /// exactly the state that separates a deliberate reach from a pointer sweeping past.
    ///
    /// Nothing about the bar surface changes for a peek: it stays mapped, it keeps its exclusive
    /// zone, and so no work area is recomputed and no window reflows on the way in or out. Only
    /// the stacking order does ([`templar_window::WindowService::set_peek_output`]).
    fn update_dwell(&mut self, pos: TemplarPoint) {
        if self.session_lock.locked {
            self.dwell = None;
            return;
        }
        // A peek ends the moment the pointer walks away from it — no dwell on the way out, the
        // same asymmetry the shell's own reveal has. A *reveal* of Home does not end here: it is
        // a deliberate act, and `WindowService::set_home_revealed` (a click, `Super+H`, a window
        // being activated) is what decides when Home goes away again. Nor does the peek an open
        // rail holds: that one lasts exactly as long as the rail (`close_rail`).
        if let Some(peeked) = self.window_service.peek_output().map(str::to_owned)
            && self
                .rail
                .as_ref()
                .is_none_or(|(output, _)| *output != peeked)
            && !self.holding_peek(&peeked, pos)
        {
            self.dwell = None;
            if self.window_service.set_peek_output(None) {
                crate::space_sync::sync_after_change(self);
            }
        }
        // A covering window's own chrome goes the same way, and holds for the same reason: the
        // strip it just showed has to be reachable, so the pointer is allowed anywhere on it.
        if let Some(shown) = self.window_service.covering_chrome()
            && !self.holding_covering_chrome(shown, pos)
        {
            self.dwell = None;
            if self.window_service.set_covering_chrome(None) {
                crate::space_sync::sync_after_change(self);
            }
        }
        // A held button is a drag, not a request: a window thrown at the corner is not someone
        // asking for Home. `is_grabbed` is true for the whole of a held button (smithay's
        // `DefaultGrab` installs a `ClickGrab` on press) as well as for our own move/resize
        // grabs, so this is both rules at once.
        let intent = if self.pointer_gesture_active() {
            None
        } else {
            self.dwell_intent_at(pos)
        };
        let now = crate::window_bridge::now_monotonic_ns(&self.clock);
        self.dwell = dwell_after_motion(self.dwell.take(), intent, pos, now);
        if self.dwell.is_some() && !self.dwell_timer_pending {
            self.arm_dwell_timer();
        }
    }

    /// One timer source per dwell, rescheduled rather than replaced.
    ///
    /// The pointer settling somewhere new restarts the dwell (`dwell_after_motion`), and with a
    /// four-pixel rest slop that happens on most motion events of an approach; inserting a source
    /// per approach — what the first version of this did — would leave a pile of them to fire and
    /// drop. Instead the callback asks `finish_dwell` how much of the dwell is left and waits out
    /// the remainder on the same source.
    fn arm_dwell_timer(&mut self) {
        self.dwell_timer_pending = true;
        let timer = Timer::from_duration(Duration::from_millis(DWELL_MS));
        if let Err(error) = self
            .loop_handle
            .insert_source(timer, |_, (), state: &mut Self| {
                if let Some(remaining) = state.finish_dwell() {
                    return TimeoutAction::ToDuration(remaining);
                }
                state.dwell_timer_pending = false;
                TimeoutAction::Drop
            })
        {
            tracing::warn!(%error, "arming the bottom-edge dwell");
            self.dwell_timer_pending = false;
            self.dwell = None;
        }
    }

    /// The dwell elapsed. Act, wait out the remainder, or fire into a no-op.
    ///
    /// Re-reads where the pointer actually is rather than trusting the position that armed the
    /// timer: motion since then may have moved the dwell to another intent, ended it, or reset
    /// its anchor. `Some(remaining)` means the anchor was reset while this timer was in flight.
    fn finish_dwell(&mut self) -> Option<Duration> {
        let dwell = self.dwell.take()?;
        let pointer = self.seat.get_pointer()?;
        let pos = to_templar_point(pointer.current_location());
        if self.dwell_intent_at(pos).as_ref() != Some(&dwell.intent) {
            return None;
        }
        let elapsed =
            crate::window_bridge::now_monotonic_ns(&self.clock).saturating_sub(dwell.since_ns);
        let dwell_ns = DWELL_MS * 1_000_000;
        if elapsed < dwell_ns {
            let remaining = Duration::from_nanos(dwell_ns - elapsed);
            self.dwell = Some(dwell);
            return Some(remaining);
        }
        match dwell.intent {
            // `reveal_home` rather than `set_home_revealed` directly: reaching Home from the
            // corner needs the scatter and switcher torn down, the `Space` mirror re-synced and
            // the keyboard moved somewhere visible, exactly as reaching it from `Super+H` does.
            DwellIntent::HomeCorner => self.reveal_home(RevealHome::Show),
            DwellIntent::BarPeek(output) => {
                // A first dwell raises the island. The timer is then re-armed for promotion
                // without requiring another motion event — a pointer that has stopped sending
                // motion is exactly the rest that earned the peek, and it is still resting.
                if self.window_service.set_peek_output(Some(output.clone())) {
                    crate::space_sync::sync_after_change(self);
                }
                let now = crate::window_bridge::now_monotonic_ns(&self.clock);
                self.dwell = Some(Dwell {
                    intent: DwellIntent::BarPromote(output),
                    at: pos,
                    since_ns: now,
                });
                return Some(Duration::from_millis(DWELL_MS));
            }
            DwellIntent::BarPromote(_output) => {
                let _ = self.window_service.set_peek_output(None);
                if self.enter_taskfield(templar_window::TaskFilter::All) {
                    crate::space_sync::sync_after_change(self);
                }
            }
            DwellIntent::CoveringChrome(window_id) => {
                if self.window_service.set_covering_chrome(Some(window_id)) {
                    crate::space_sync::sync_after_change(self);
                }
            }
        }
        None
    }

    /// What a dwell at `pos` would be about, if anything.
    ///
    /// The three regions are disjoint by construction (`in_peek_band` excludes both squares it
    /// could otherwise meet), so there is never a precedence question between them: each region
    /// has one rule.
    ///
    /// - the **top-left** square is **Home**, whatever is on the screen. The bar has no Home chip,
    ///   so this is the only pointer route there, and a window filling the screen is exactly when
    ///   it is needed most. It moved here from the bottom-left because Peek needs that corner:
    ///   the affordance has to be where the island starts, or the pointer travels away from the
    ///   thing it is reaching for.
    /// - the **bottom-right** square is a screen-filling window's **own chrome**, shown over it.
    ///   One dwell shows the whole strip, tag included, because it *is* one strip.
    /// - the **bottom edge**, its left corner included, is **Peek**: raise the island over a
    ///   window filling the screen, and say so with `bar_peeked`.
    ///
    /// The one state-dependent part: while that chrome is already showing, neither the
    /// bottom-right corner nor the bottom edge asks for anything. The strip is under the pointer
    /// and the strip is what is being reached for — the strip is `TAG_HEIGHT` tall and the peek
    /// band is its bottom ten pixels, so without this, resting on the close tag long enough to aim
    /// at it opens the island underneath the thing being aimed at.
    fn dwell_intent_at(&self, pos: TemplarPoint) -> Option<DwellIntent> {
        let output = self
            .window_service
            .outputs()
            .iter()
            .find(|output| rect_contains(output.logical, pos))?;
        // What is filling this output, if anything — maximized or fullscreen alike
        // (`WindowRecord::covers_screen`). Such a window has no frame to reach for and the shell's
        // bar carries no copy of its controls, so the right-hand square is the only route to them.
        // The topmost one, because that is the one whose chrome the corner would be over.
        let covering = self.window_service.stacking_order().into_iter().find(|id| {
            self.window_service.is_client_window(*id)
                // Visible, not merely covering: a reveal hides every client window, so with Home
                // in front there is nothing over the corner to show chrome for and nothing over
                // the edge for the switcher to come out from under.
                && self.window_service.is_visible(*id)
                && self.window_service.window(*id).is_some_and(|window| {
                    window.covers_screen() && window.geometry.intersection_area(output.logical) > 0
                })
        });

        // The top-left square is Home, whatever is on the screen — it is nowhere near the tag
        // strip a screen-filling window shows along its bottom edge, so unlike the other two it
        // needs no "already showing" guard.
        if in_home_corner(output.logical, pos) {
            // Not while the shell's own chrome is under the pointer: whatever it is showing there
            // is what the person is reaching for, and it gets the press.
            if self.over_shell_overlay(pos) {
                return None;
            }
            // Nothing to ask for if Home is already showing. Without this, a pointer resting in
            // the corner re-fires the reveal on every twitch inside the rest slop, and each one
            // costs a re-sync and a refocus for a state that has not changed.
            return (!self.window_service.home_revealed()).then_some(DwellIntent::HomeCorner);
        }
        // The user's rule: "close/maximize/minimize should appear on hover in the bottom
        // right, over the full screen window, not adjacent to it." One dwell shows the whole
        // strip, name tag included — they are one strip.
        if in_controls_corner(output.logical, pos) {
            if self.over_shell_overlay(pos) {
                return None;
            }
            // Already showing: the strip is the thing under the pointer, and the corner is not
            // asking for anything else while it is.
            if self.window_service.covering_chrome().is_some() {
                return None;
            }
            return covering.map(DwellIntent::CoveringChrome);
        }
        // Gesture rails replace Peek outright: two activators for the same surfaces would open
        // and close them over each other (`src/gesture_gate.rs`). The corners above stay.
        if self.gesture_gate.config().enabled {
            return None;
        }
        // The strip a covering window's chrome puts on screen is `TAG_HEIGHT` tall and the peek
        // band is its bottom ten pixels, so without this, resting on the close tag long enough to
        // aim at it opens the switcher underneath the thing being aimed at.
        if self.window_service.covering_chrome().is_some() || !in_peek_band(output.logical, pos) {
            return None;
        }
        covering.map(|_| {
            if self.window_service.peek_output() == Some(output.name.as_str()) {
                DwellIntent::BarPromote(output.name.clone())
            } else {
                DwellIntent::BarPeek(output.name.clone())
            }
        })
    }

    /// Pin or unpin a window on a click of its name tag, and put the new answer on screen: the
    /// stacking order changes, and the tag draws the window's name in italic while it holds.
    ///
    /// The user's rule, and the reason it is the tag rather than a fourth control: the tag is
    /// already the thing you take hold of to say "this window, here", and there is nothing else
    /// a bare click on it could have meant.
    fn toggle_always_on_top(&mut self, window_id: WindowId) {
        if !self.window_service.is_client_window(window_id) {
            return;
        }
        self.window_service.toggle_always_on_top(window_id);
        crate::space_sync::sync_after_change(self);
    }

    /// Whether a covering window's chrome should stay shown: the pointer is still in one of the
    /// corners that opened it, or anywhere on the strip it put there. Without the strip, moving on
    /// to the close button would be moving off the thing that summoned it.
    fn holding_covering_chrome(&self, window_id: WindowId, pos: TemplarPoint) -> bool {
        let Some(window) = self.window_service.window(window_id) else {
            return false;
        };
        if in_controls_corner(window.geometry, pos) {
            return true;
        }
        // The strip is `TAG_HEIGHT` tall and spans the window, so it already contains both bottom
        // corner squares — including the bottom-left one, which is Peek's now and must not end the
        // chrome the person is reaching across.
        self.window_service
            .recovery_affordance(window_id)
            .is_some_and(|affordance| rect_contains(affordance.strip, pos))
    }

    /// Whether a peek on `output` should stay up: the pointer is still on the edge that opened
    /// it, or anywhere on the shell's own surface.
    ///
    /// The surface is the point. The first version of this widened the band to the strip the
    /// shell *reserved* there, which was right when the bar reserved 48 pixels and is nothing at
    /// all now that it claims a zone of 0 — moving onto the bar that just appeared would have
    /// dismissed it again.
    fn holding_peek(&self, output: &str, pos: TemplarPoint) -> bool {
        let Some(output) = self
            .window_service
            .outputs()
            .iter()
            .find(|candidate| candidate.name == output)
        else {
            return false;
        };
        in_peek_band(output.logical, pos) || self.over_shell_overlay(pos)
    }

    /// Whether a visible shell-overlay surface is under `pos`.
    fn over_shell_overlay(&self, pos: TemplarPoint) -> bool {
        self.window_service.windows().any(|window| {
            self.window_service.layer_of(window.id) == Some(Layer::ShellOverlay)
                && self.window_service.is_visible(window.id)
                && rect_contains(window.geometry, pos)
        })
    }

    /// Feed the gesture gate one motion segment (`src/gesture_gate.rs`) — `from` and `to` are the
    /// pointer before and after the report, global logical — and, while a rail is open, tell the
    /// shell where the pointer went instead. Both motion paths call this after `update_dwell`.
    ///
    /// The gate is fed on the output under `to`, in that output's own rectangle, so leaving the
    /// output an arm started on is seen as the output changing.
    fn update_gesture_gate(&mut self, from: Point<f64, Logical>, to: Point<f64, Logical>) {
        if self.rail.is_some() {
            self.send_rail_pointer(to);
            return;
        }
        // Nothing arms under the lock, the window overview or the pressure overlay: each owns the
        // pointer outright, and a rail opening under one would be a menu nobody can see.
        if !self.gesture_gate.config().enabled
            || self.session_lock.locked
            || self.recovery_protocol.pressure_overlay.is_some()
            || matches!(
                self.window_service.presentation(),
                templar_window::Presentation::Taskfield(_)
            )
        {
            return;
        }
        let point = to_templar_point(to);
        let Some((index, screen)) = self
            .window_service
            .outputs()
            .iter()
            .enumerate()
            .find(|(_, output)| rect_contains(output.logical, point))
            .map(|(index, output)| (index, output.logical))
        else {
            return;
        };
        let sample = crate::gesture_gate::Sample {
            output: index,
            screen: crate::gesture_gate::Area {
                x: f64::from(screen.x),
                y: f64::from(screen.y),
                width: f64::from(screen.width),
                height: f64::from(screen.height),
            },
            from: crate::gesture_gate::Pos {
                x: from.x,
                y: from.y,
            },
            to: crate::gesture_gate::Pos { x: to.x, y: to.y },
            now_ms: crate::window_bridge::now_monotonic_ns(&self.clock) / 1_000_000,
            // `is_grabbed` covers a held button (smithay's click grab) as well as our own move
            // and resize grabs — the same "a drag is not a request" rule `update_dwell` uses.
            button_held: self.pointer_gesture_active(),
        };
        let window_service = &self.window_service;
        let Some(transition) = self.gesture_gate.feed(sample, |config| {
            !under_gate_exception(window_service, config, point)
        }) else {
            return;
        };
        match transition {
            crate::gesture_gate::Transition::Armed => {
                tracing::debug!(output = index, x = to.x, y = to.y, "gesture gate armed");
            }
            crate::gesture_gate::Transition::Disarmed(reason) => {
                tracing::debug!(?reason, "gesture gate disarmed");
            }
            crate::gesture_gate::Transition::Opened(side) => {
                let output = self.window_service.outputs()[index].name.clone();
                tracing::debug!(%output, ?side, "gesture gate opened a rail");
                self.open_rail(output, side, to);
            }
        }
        // The learning outline brightens while armed; nothing else on screen changed, so a
        // damage-driven backend has to be asked for the frame.
        if self.gesture_gate.config().learning {
            B::request_redraw(self);
        }
    }

    /// The gate opened `side` on `output`: peek that output (which raises the shell's overlay over
    /// a fullscreen window) and tell the shell, then where the pointer is.
    fn open_rail(
        &mut self,
        output: String,
        side: crate::gesture_gate::Side,
        pointer: Point<f64, Logical>,
    ) {
        self.rail = Some((output.clone(), side));
        if self.window_service.set_peek_output(Some(output)) {
            crate::space_sync::sync_after_change(self);
        }
        crate::protocols_shell::send_rail(self);
        self.send_rail_pointer(pointer);
    }

    /// `rail_pointer`, in the rail's output's own coordinates. An output that has gone away takes
    /// its rail with it.
    fn send_rail_pointer(&mut self, pointer: Point<f64, Logical>) {
        let Some((name, _)) = &self.rail else {
            return;
        };
        let Some(origin) = self
            .window_service
            .outputs()
            .iter()
            .find(|output| output.name == *name)
            .map(|output| (output.logical.x, output.logical.y))
        else {
            self.close_rail("output gone");
            return;
        };
        crate::protocols_shell::send_rail_pointer(
            self,
            pointer.x - f64::from(origin.0),
            pointer.y - f64::from(origin.1),
        );
    }

    /// End the rail, whoever ended it — the shell's `close_rail`, a press on anything not the
    /// shell's, Escape, the session locking, Taskfield opening, the gate being disabled, the shell
    /// going away. The one close path, so every one of them lowers the rail's peek, tells the shell
    /// (`rail` with no side) and leaves the gate needing a fresh approach from above. Also drops
    /// a pending arm, so it is safe to call with no rail open.
    pub(crate) fn close_rail(&mut self, reason: &'static str) {
        let was_armed = self.gesture_gate.phase() != crate::gesture_gate::Phase::Idle;
        self.gesture_gate.close();
        let Some((output, side)) = self.rail.take() else {
            if was_armed && self.gesture_gate.config().learning {
                B::request_redraw(self);
            }
            return;
        };
        tracing::debug!(reason, %output, ?side, "rail closed");
        if self.window_service.peek_output() == Some(output.as_str())
            && self.window_service.set_peek_output(None)
        {
            crate::space_sync::sync_after_change(self);
        }
        crate::protocols_shell::send_rail(self);
        if self.gesture_gate.config().learning {
            B::request_redraw(self);
        }
    }

    /// Whether a press at `location` lands on one of the trusted shell's surfaces.
    fn press_lands_on_shell(&self, location: Point<f64, Logical>) -> bool {
        self.surface_under(location)
            .and_then(|(surface, _)| self.display_handle.get_client(surface.id()).ok())
            .is_some_and(|client| crate::state::client_is_trusted_shell(&client))
    }

    /// Show `shape` (a `cursor-spec` name) as the compositor's own cursor, or `None` to give the
    /// pointer back to whatever the client had. Idempotent: showing the shape already on screen
    /// changes nothing, so this is safe to call on every motion event.
    fn show_compositor_cursor(&mut self, shape: Option<&'static str>) {
        match shape {
            Some(name) => {
                // `templar_window` names the shape the way `cursor-spec` does, so the enum comes
                // back out of the name rather than the geometry crate depending on `cursor-icon`
                // (`xtask deps-check` keeps it free of everything but geometry).
                let icon = name.parse().unwrap_or(CursorIcon::Default);
                let named = CursorImageStatus::Named(icon);
                if self.cursor_before_perimeter.is_none() {
                    self.cursor_before_perimeter =
                        Some(std::mem::replace(&mut self.cursor_status, named));
                } else {
                    self.cursor_status = named;
                }
            }
            None => {
                if let Some(saved) = self.cursor_before_perimeter.take() {
                    self.cursor_status = saved;
                }
            }
        }
    }

    /// Whether a compositor-run pointer gesture (a move or a resize) is in progress.
    fn pointer_gesture_active(&self) -> bool {
        self.seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
    }

    /// One pointer button while the window overview is up. `true` when the mode took the event and
    /// no client may see it.
    ///
    /// The shell's own overview chrome is the one exception: its surface covers the output but
    /// accepts input only where it drew something, so a point its input region takes is its query
    /// field or its menu and goes straight through.
    fn taskfield_button(
        &mut self,
        location: Point<f64, Logical>,
        button: u32,
        state: ButtonState,
    ) -> bool {
        if self.surface_under(location).is_some() {
            self.taskfield_press = None;
            return false;
        }
        // Secondary and middle buttons are swallowed and do nothing. A card's context menu —
        // close, park, ungroup — is the shell's, drawn over its own Presentation surface; the
        // compositor has no menu widget, and half of one would be worse than none.
        if button != BTN_LEFT {
            return true;
        }
        let pos = to_templar_point(location);
        if state == ButtonState::Pressed {
            self.taskfield_press = Some(pos);
            return true;
        }
        let Some(pressed_at) = self.taskfield_press.take() else {
            return true;
        };
        let Some((sections, overflow_headers, overflow_keys)) =
            (match self.window_service.presentation() {
                templar_window::Presentation::Taskfield(taskfield) => Some((
                    taskfield
                        .sections
                        .iter()
                        .map(|section| {
                            section
                                .cards
                                .iter()
                                .map(|card| (card.window, card.rect))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>(),
                    taskfield
                        .sections
                        .iter()
                        .enumerate()
                        .filter(|(_, section)| section.overflow > 0)
                        .map(|(index, section)| (index, section.header))
                        .collect::<Vec<_>>(),
                    taskfield
                        .sections
                        .iter()
                        .map(|section| section.key.clone())
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            })
        else {
            return true;
        };
        match taskfield_release(&sections, &overflow_headers, pressed_at, pos) {
            TaskfieldRelease::Cancel => {}
            TaskfieldRelease::Overflow(index) => {
                if let Some(key) = overflow_keys.get(index).cloned()
                    && self.window_service.taskfield_overflow(key)
                {
                    crate::protocols_shell::send_taskfield_state(self);
                    crate::space_sync::sync_after_change(self);
                    B::request_redraw(self);
                }
            }
            TaskfieldRelease::Activate(window_id) => {
                self.leave_taskfield();
                if let Some(window_id) = window_id {
                    activate_window(self, window_id);
                }
                crate::space_sync::sync_after_change(self);
            }
            TaskfieldRelease::Group { dragged, anchor } => {
                // Grouping is the same one entry point the band drop and `Super+G` take, so the
                // three cannot drift: the target is the anchor, and the dragged window is placed
                // beside it. Then out of the mode, because the answer has been given.
                self.leave_taskfield();
                crate::window_groups::group_windows(self, anchor, dragged);
                crate::space_sync::sync_after_change(self);
            }
        }
        true
    }

    /// Returns whether the press was the compositor's own — one of the affordances on a window's
    /// tag strip — and must therefore not also be delivered to a client.
    fn handle_press_for_focus_or_drag(
        &mut self,
        location: Point<f64, Logical>,
        serial: Serial,
        button: u32,
    ) -> bool {
        if self.session_lock.locked {
            return false;
        }
        let pos = to_templar_point(location);
        let target = self.press_target_at(pos);

        // Click to focus, and only click — TemplarOS does not follow the mouse. Pressing anywhere a
        // window owns brings that window forward first: its title band, its resize affordances
        // and its client area alike. Doing it before anything else means a drag started from the
        // band drags a window that is already on top and already has the keyboard, rather than
        // moving a window the user cannot type into — which is what "the windows fight for
        // focus" looked like from the outside.
        if let Some(window_id) = press_activates(target) {
            if self.window_service.is_client_window(window_id) {
                activate_window(self, window_id);
            } else if let Some(surface) = self.windows.surface_for(window_id).cloned()
                && let Some(keyboard) = self.seat.get_keyboard()
            {
                // A shell surface (the Home Surface) takes the keyboard without being activated
                // or raised: `WindowService::activate` is for client windows, and raising the
                // Home Surface out of `Layer::HomeSurface` is not a thing that can happen.
                keyboard.set_focus(self, Some(surface), serial);
            }
        }

        // The compositor's own affordances, checked before the client sees the press: the band
        // moves the window, the corner grip and the edge zones resize it. All are
        // compositor-owned screen area by the same argument (`press_target_at`'s doc) — without
        // them a window whose client draws no frame, which is most XWayland windows, could be
        // neither moved nor resized by anyone.
        //
        // Primary button only. Both grabs end on `BTN_LEFT`'s release (`WindowGestureGrab`), so a
        // right- or middle-press that started one would install a grab nothing the user does with
        // that button could ever end.
        if button == BTN_LEFT
            && let PressTarget::Affordance(window_id, affordance) = target
        {
            let seat = self.seat.clone();
            let gesture = match affordance {
                Affordance::Button(ControlTag::Close) => {
                    crate::backend::close_window(self, window_id);
                    return true;
                }
                Affordance::Button(ControlTag::Park) => {
                    park_window(self, window_id);
                    return true;
                }
                Affordance::Button(ControlTag::Maximize) => {
                    // The middle tag is the way back out of whichever way this window filled the
                    // screen. Fullscreen first: maximizing instead configured the client to the
                    // maximized rectangle while `fullscreen` stayed true, which is a window that
                    // is both and neither.
                    let fullscreen = self
                        .window_service
                        .window(window_id)
                        .is_some_and(|window| window.fullscreen);
                    if fullscreen {
                        crate::xdg_shell::apply_fullscreen(self, window_id, false);
                        return true;
                    }
                    let maximized = self
                        .window_service
                        .window(window_id)
                        .is_some_and(|window| window.maximized);
                    crate::xdg_shell::apply_maximized(self, window_id, !maximized);
                    return true;
                }
                // A covering window's name tag names it and pins it, and nothing else. Dragging
                // it would move a window whose geometry is the output off the output, which
                // COMPOSITOR.md says is never repaired — and there is nowhere for it to go
                // anyway — so the click is taken here rather than at the end of a gesture that
                // must not start.
                Affordance::Tag if self.window_service.covering_chrome() == Some(window_id) => {
                    self.toggle_always_on_top(window_id);
                    return true;
                }
                Affordance::Tag => Gesture::Move,
                Affordance::Edge(edges) => Gesture::Resize { edges },
            };
            start_gesture(
                self,
                &seat,
                serial,
                window_id,
                gesture,
                GestureOrigin::CompositorAffordance,
            );
            return true;
        }

        if target != PressTarget::Nothing {
            // A window owned the point and has already been focused above; the press itself goes
            // on to the client through `PointerHandle::button`.
            return false;
        }

        // Nothing `WindowService` knows owns this point. That is not the same as nothing being
        // there: an `xdg_popup` — a menu, a tooltip — lives outside its parent window's geometry
        // and is reachable only through the surface tree, which is exactly what `element_under`
        // walks.
        if let Some((window, _)) = self
            .space
            .element_under(location)
            .map(|(w, l)| (w.clone(), l))
        {
            let Some(surface) = window.wl_surface().map(std::borrow::Cow::into_owned) else {
                return false;
            };
            if let Some(window_id) = self.windows.window_id_for(&surface.id()) {
                if self.window_service.is_client_window(window_id) {
                    activate_window(self, window_id);
                } else if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, Some(surface), serial);
                }
            }
        } else if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }
        false
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: &I::PointerAxisEvent) {
        let source = event.source();
        let horizontal_amount = event
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
        let vertical_amount = event
            .amount(Axis::Vertical)
            .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);
        let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
        let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        if horizontal_amount != 0.0 {
            frame = frame.value(Axis::Horizontal, horizontal_amount);
            if let Some(discrete) = horizontal_amount_discrete {
                frame = frame.v120(Axis::Horizontal, discrete as i32);
            }
        }
        if vertical_amount != 0.0 {
            frame = frame.value(Axis::Vertical, vertical_amount);
            if let Some(discrete) = vertical_amount_discrete {
                frame = frame.v120(Axis::Vertical, discrete as i32);
            }
        }
        if source == AxisSource::Finger {
            if event.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if event.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }

        if let Some(pointer) = self.seat.get_pointer() {
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // While the session is locked, the lock surface for the output under the pointer is the
        // only thing that can be pointed at. Fixed here rather than at each caller because this
        // is the one hit-test every pointer path routes through — motion, buttons, axis and drag
        // alike — so one guard is both the smaller diff and the only way a later caller cannot
        // forget it. `src/session_lock.rs`'s module doc named this as the outstanding half of
        // input isolation: refusing recovery keybindings and window drags stopped the two ways
        // this crate reached another window's chrome, but Smithay's ordinary focus resolution
        // still handed clicks to whatever was behind the lock screen.
        if self.session_lock.locked {
            let output = self.space.outputs().find(|output| {
                self.space
                    .output_geometry(output)
                    .is_some_and(|geometry| geometry.to_f64().contains(pos))
            })?;
            let origin = self.space.output_geometry(output)?.loc.to_f64();
            let surface = self.session_lock.surfaces.get(&output.name())?;
            return Some((surface.wl_surface().clone(), origin));
        }
        // The window overview is modal for the pointer exactly as it is for the keyboard: the
        // only surface that can be pointed at is the shell's `Layer::Presentation` one, and only
        // inside its own rectangle. Every client window on screen is a *picture* of itself there
        // (`press_candidates`), and a picture takes no pointer events. Guarded here, in the one
        // hit-test every pointer path routes through, so no later caller can forget it.
        if matches!(
            self.window_service.presentation(),
            templar_window::Presentation::Taskfield(_)
        ) {
            let presentation = self
                .window_service
                .stacking_order()
                .into_iter()
                .find(|id| {
                    self.window_service.layer_of(*id) == Some(templar_window::Layer::Presentation)
                })?;
            let window = self.windows.element_of(presentation)?;
            let location = self.space.element_location(window)?;
            return window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()));
        }
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }
}

fn log_input_device<I: InputBackend>(event: &str, device: &I::Device) {
    tracing::info!(
        event,
        name = %device.name(),
        id = %device.id(),
        keyboard = device.has_capability(DeviceCapability::Keyboard),
        pointer = device.has_capability(DeviceCapability::Pointer),
        touch = device.has_capability(DeviceCapability::Touch),
        gesture = device.has_capability(DeviceCapability::Gesture),
        "input device"
    );
}

/// Keep a relative pointer on the nearest real output, including negative-origin layouts.
///
/// `outputs` yields the output rectangles afresh each call, so the caller can walk its own
/// geometry twice without collecting it.
fn clamp_to_outputs<I: Iterator<Item = Rectangle<i32, Logical>>>(
    position: Point<f64, Logical>,
    outputs: impl Fn() -> I,
) -> Point<f64, Logical> {
    if outputs().any(|geometry| geometry.to_f64().contains(position)) {
        return position;
    }
    outputs()
        .map(|geometry| {
            let left = f64::from(geometry.loc.x);
            let top = f64::from(geometry.loc.y);
            let right = f64::from(geometry.loc.x + geometry.size.w).max(left + 1.0) - 1.0;
            let bottom = f64::from(geometry.loc.y + geometry.size.h).max(top + 1.0) - 1.0;
            let clamped =
                Point::from((position.x.clamp(left, right), position.y.clamp(top, bottom)));
            let dx = clamped.x - position.x;
            let dy = clamped.y - position.y;
            (dx * dx + dy * dy, clamped)
        })
        .min_by(|(a, _), (b, _)| a.total_cmp(b))
        .map_or(position, |(_, clamped)| clamped)
}

fn rect_contains(rect: templar_window::Rect, point: TemplarPoint) -> bool {
    point.x >= rect.x && point.x < rect.right() && point.y >= rect.y && point.y < rect.bottom()
}

/// Whether the gate must not arm at `point` because the topmost visible client window there is
/// one of the configured exceptions. The *resolved* identity, as `keybindings::terminal_window`
/// reads it: what a client calls itself does not get it out of — or into — the list.
fn under_gate_exception(
    window_service: &templar_window::WindowService<templar_comp_integration::DbusIdentityCache>,
    config: &crate::gesture_gate::GateConfig,
    point: TemplarPoint,
) -> bool {
    if config.exceptions.is_empty() {
        return false;
    }
    window_service
        .stacking_order()
        .into_iter()
        .filter(|id| window_service.is_client_window(*id) && window_service.is_visible(*id))
        .filter_map(|id| window_service.window(id))
        .find(|window| rect_contains(window.geometry, point))
        .and_then(|window| window.identity.app_id.as_ref())
        .is_some_and(|app_id| config.is_exception(app_id.as_str()))
}

/// The side of each of the three corner squares an output reserves, in logical pixels: Home at the
/// top-left, a screen-filling window's own chrome at the bottom-right, and Peek at the bottom-left
/// (which is simply the near end of [`PEEK_BAND`], not a square of its own).
///
/// A calibration knob rather than a constant of nature: a person pushing a real mouse into a
/// corner of a real screen has to be able to land on it, and a fractional scale eats a pixel or
/// two of it. Widen it if a corner is hard to hit; narrow it if one fires on its own.
const CORNER_SQUARE: i32 = 8;
/// How close to an output's bottom edge the pointer must come to *start* a peek, in logical
/// pixels. Same knob, same reasoning.
///
/// Wider than the corner because of what it has to be crossed *to*: the peek raises the shell's
/// collapsed hint tab, which is six logical pixels tall, and leaving the band ends the peek at
/// once. A four-pixel band meant the pointer had to travel to a six-pixel target while never
/// leaving a four-pixel one — natural with a mouse pushed into the bottom of the screen, not with
/// a touchpad. It is the tab's height plus the rest slop, so reaching the thing that appeared can
/// never dismiss it.
const PEEK_BAND: i32 = 10;
/// Thickness of the corner affordance Peek draws under a screen-filling window, in logical pixels
/// (`chrome::collect_output_chrome`). It marks the corner; the region that answers a dwell is the
/// whole of [`PEEK_BAND`], which is five times taller.
pub const PEEK_AFFORDANCE: i32 = 2;
/// How long the pointer must rest before either dwell fires.
///
/// The same number as the shell's own reveal (`templar_shell::surface::bar_screen::HINT_OPEN_MS`),
/// on purpose: reaching the bar from under a fullscreen window and reaching it from under a
/// maximized one should feel like the same gesture, because to the person doing it they are.
const DWELL_MS: u64 = 350;
/// How far the pointer may drift and still be resting, in logical pixels — the shell's
/// `HINT_REST_SLOP`. Resting is the condition, not merely being low: a pointer crossing the
/// bottom of the screen on its way somewhere else never holds still long enough to ask for
/// anything.
const DWELL_REST_SLOP: i32 = 4;

/// How far the pointer may travel between a name tag's press and its release and still count as a
/// click rather than a drag, in logical pixels. Not zero: a hand holding a button down moves, and
/// a pin that only happened when the pointer was perfectly still would read as unreliable rather
/// than as strict.
const CLICK_SLOP: i32 = 3;

/// Whether a press at `pressed` released at `released` was a click rather than a drag.
fn is_click(pressed: TemplarPoint, released: TemplarPoint) -> bool {
    (released.x - pressed.x).abs() <= CLICK_SLOP && (released.y - pressed.y).abs() <= CLICK_SLOP
}

/// Whether a compositor-owned press still owns its matching release.
///
/// A grab installed by that press (`WindowGestureGrab`) needs the release to finish; swallowing
/// the pair is how a tag/edge drag used to never end. Affordances that act and return — close,
/// park, maximize, pin — install no grab, so they still swallow the release.
fn swallow_matching_release(compositor_owned: bool, grab_installed: bool) -> bool {
    compositor_owned && !grab_installed
}

/// What the pointer resting at the bottom of the screen is asking for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DwellIntent {
    /// The top-left corner: reveal the Home Surface.
    HomeCorner,
    /// The bottom edge of a named output carrying a window that fills the screen, its left corner
    /// (where the affordance is drawn) included:
    /// raise the shell's overlay over it (fullscreen, which outranks it) and tell the shell the
    /// edge has been asked for (`bar_peeked`, either way). The shell cannot see this for itself —
    /// whatever is under the pointer owns it — so it is the compositor that has to notice.
    BarPeek(String),
    /// A second dwell on an already-peeked edge: open Taskfield. Distinct from [`Self::BarPeek`]
    /// so the dwell clock restarts after the island comes up, rather than treating "already
    /// peeked" as no intent at all — which is how promotion used to be unreachable without a
    /// motion past the island.
    BarPromote(String),
    /// Either bottom corner of a window that fills the screen: show that window's own name tag
    /// and controls over it. Such a window has no frame to reach for and the shell's bar carries
    /// no copy of its controls, so without this there is no pointer route to closing it at all.
    CoveringChrome(WindowId),
}

/// A dwell in progress: what it is for, where the pointer settled, and when it settled there.
#[derive(Clone, Debug)]
pub struct Dwell {
    pub intent: DwellIntent,
    at: TemplarPoint,
    since_ns: u64,
}

/// One motion event's effect on a dwell.
///
/// Pure, because this is the whole of the "swept through versus rested" rule and it is worth
/// being able to drive it directly: a pointer that keeps moving keeps resetting `since_ns`, so it
/// never accumulates a dwell however long it spends in the region.
fn dwell_after_motion(
    current: Option<Dwell>,
    intent: Option<DwellIntent>,
    pos: TemplarPoint,
    now_ns: u64,
) -> Option<Dwell> {
    let intent = intent?;
    if let Some(dwell) = current
        && dwell.intent == intent
        && (pos.x - dwell.at.x).abs() <= DWELL_REST_SLOP
        && (pos.y - dwell.at.y).abs() <= DWELL_REST_SLOP
    {
        return Some(dwell);
    }
    Some(Dwell {
        intent,
        at: pos,
        since_ns: now_ns,
    })
}

/// Whether `pos` is in the square at `output`'s **top-left** corner that reveals Home.
///
/// It used to be the bottom-left one. Peek took that corner (planning doc §6: the corner
/// affordance is where the island starts, so Peek reveals it in place), and two dwells cannot
/// share a square. The top-left is the only other corner nothing owns — the bottom-right is a
/// screen-filling window's own chrome — and Home's primary route was never this anyway: it is the
/// bare `Super` tap, of which this is the pointer spelling.
fn in_home_corner(output: templar_window::Rect, pos: TemplarPoint) -> bool {
    pos.x >= output.x
        && pos.x < output.x + CORNER_SQUARE
        && pos.y >= output.y
        && pos.y < output.y + CORNER_SQUARE
}

/// The bottom-*right* square. It is where a screen-filling window's own controls appear, and it
/// means nothing when no window fills the screen — a corner that does something only sometimes is
/// better than one that does two things.
fn in_controls_corner(output: templar_window::Rect, pos: TemplarPoint) -> bool {
    pos.x >= output.right() - CORNER_SQUARE
        && pos.x < output.right()
        && pos.y >= output.bottom() - CORNER_SQUARE
        && pos.y < output.bottom()
}

/// Whether `pos` is in the band along `output`'s bottom edge that opens a peek — the bottom-left
/// corner **included**, which is the corner the affordance is drawn in
/// (`chrome::collect_output_chrome`).
///
/// Disjoint from the other two squares by construction rather than by a precedence rule: Home is
/// at the top-left and a screen-filling window's chrome at the bottom-right, so one pointer
/// position never means two things. The `in_home_corner` exclusion is kept for an output shorter
/// than `CORNER_SQUARE + PEEK_BAND`, where the two would otherwise meet.
fn in_peek_band(output: templar_window::Rect, pos: TemplarPoint) -> bool {
    pos.x >= output.x
        && pos.x < output.right()
        && pos.y >= output.bottom() - PEEK_BAND
        && pos.y < output.bottom()
        && !in_home_corner(output, pos)
        && !in_controls_corner(output, pos)
}

/// The affordance Peek draws in `output`'s bottom-left corner: [`PEEK_AFFORDANCE`] logical pixels
/// tall and [`CORNER_SQUARE`] wide, flush with the corner.
///
/// Two logical pixels, because it has to be visible without being furniture: a person watching a
/// fullscreen video is not being shown a bar, they are being shown where the bar is. Pure so the
/// rectangle chrome paints and the region [`in_peek_band`] answers for are derived from the same
/// corner rather than from two independent readings of "bottom-left".
pub fn peek_affordance(output: templar_window::Rect) -> templar_window::Rect {
    templar_window::Rect::new(
        output.x,
        output.bottom() - PEEK_AFFORDANCE,
        CORNER_SQUARE.min(output.width),
        PEEK_AFFORDANCE.min(output.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;
    use templar_window::{Rect, frame_around};

    fn output(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((width, height)))
    }

    fn at(x: i32, y: i32) -> TemplarPoint {
        TemplarPoint { x, y }
    }

    /// The two regions along the bottom of the screen, and the fact that they are disjoint: the
    /// corner square is Home, the rest of the edge is the bar, and no point is both. That is what
    /// makes "the corner works under a fullscreen window too" a rule rather than a precedence
    /// argument — the peek never gets a chance to claim it.
    #[test]
    fn home_is_the_top_left_corner_and_peek_owns_the_whole_bottom_edge() {
        let screen = Rect::new(0, 0, 1920, 1080);
        // Home moved to the top-left so Peek could have the bottom-left: the affordance has to be
        // where the island starts, or the pointer travels away from what it is reaching for. The
        // bottom-left corner is now simply the near end of the peek band, not a square of its own.
        for (x, y, home, band) in [
            (0, 0, true, false),       // the top-left corner itself
            (7, 7, true, false),       // the far edge of the square
            (8, 0, false, false),      // one pixel past it, along the top: nothing
            (0, 8, false, false),      // one pixel below it, down the left edge: nothing
            (0, 1079, false, true),    // the bottom-left corner is Peek now
            (7, 1072, false, true),    // and so is the square that used to be Home
            (8, 1079, false, true),    // and the edge beside it
            (0, 1069, false, false),   // one pixel above the band
            (960, 1079, false, true),  // the middle of the edge
            (1900, 1076, false, true), // the other end of it, short of the controls' corner
            (960, 1080, false, false), // off the output
        ] {
            assert_eq!(in_home_corner(screen, at(x, y)), home, "home at {x},{y}");
            assert_eq!(in_peek_band(screen, at(x, y)), band, "band at {x},{y}");
            assert!(
                !(in_home_corner(screen, at(x, y)) && in_peek_band(screen, at(x, y))),
                "disjoint at {x},{y}"
            );
        }
    }

    /// The mark chrome paints and the region the dwell answers for come from the same corner: the
    /// affordance is inside the band it advertises, flush with the bottom-left, and two logical
    /// pixels tall so it says where the island is without becoming furniture over a video.
    #[test]
    fn the_peek_affordance_sits_inside_the_band_it_advertises() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let mark = peek_affordance(screen);
        assert_eq!(mark, Rect::new(0, 1078, CORNER_SQUARE, PEEK_AFFORDANCE));
        for x in mark.x..mark.right() {
            for y in mark.y..mark.bottom() {
                assert!(in_peek_band(screen, at(x, y)), "mark pixel {x},{y} is band");
            }
        }
        // On an output offset in the global scene it follows the output, not the origin.
        let second = Rect::new(1920, -200, 1280, 720);
        assert_eq!(
            peek_affordance(second),
            Rect::new(1920, 518, CORNER_SQUARE, PEEK_AFFORDANCE)
        );
    }

    /// A fullscreen window owns both bottom corners: the tag at the left, the controls at the
    /// right, drawn over it rather than beside it. The user's rule, and the reason for it is that
    /// there is otherwise no pointer route to closing a fullscreen window at all — it wears no
    /// frame, and the island carries no copy of its controls.
    #[test]
    fn a_fullscreen_window_owns_the_bottom_right_corner_and_peek_owns_the_rest_of_the_edge() {
        let screen = Rect::new(0, 0, 1920, 1080);
        for (x, y, home, right, band) in [
            (0, 0, true, false, false),        // Home, out of the way at the top
            (0, 1079, false, false, true),     // the bottom-left corner is Peek's
            (1919, 1079, false, true, false),  // the controls' corner
            (1912, 1072, false, true, false),  // the far edge of that square
            (1911, 1079, false, false, true),  // one pixel inside it, along the bottom
            (960, 1079, false, false, true),   // the middle of the edge is Peek's
            (960, 1069, false, false, false),  // above the band
            (1919, 1069, false, false, false), // and above the right-hand square
        ] {
            assert_eq!(in_home_corner(screen, at(x, y)), home, "home at {x},{y}");
            assert_eq!(
                in_controls_corner(screen, at(x, y)),
                right,
                "right at {x},{y}"
            );
            assert_eq!(in_peek_band(screen, at(x, y)), band, "band at {x},{y}");
            // Three regions along one edge, and never two of them at once.
            let claims = u8::from(in_home_corner(screen, at(x, y)))
                + u8::from(in_controls_corner(screen, at(x, y)))
                + u8::from(in_peek_band(screen, at(x, y)));
            assert!(claims <= 1, "{claims} regions claim {x},{y}");
        }
    }

    /// The dwell is a *rest*, not a presence: a pointer swept through the corner keeps resetting
    /// the clock and never earns a reveal, however many events it spends inside the square.
    #[test]
    fn sweeping_through_the_corner_never_completes_a_dwell() {
        let ms = |n: u64| n * 1_000_000;
        let dwell_ns = ms(DWELL_MS);
        let completed = |dwell: &Option<Dwell>, now: u64| {
            dwell
                .as_ref()
                .is_some_and(|dwell| now.saturating_sub(dwell.since_ns) >= dwell_ns)
        };

        // Swept: one pixel every 10ms, right through the square and out the other side. Every
        // step is further than the rest slop from where the last one settled.
        let mut dwell = None;
        for step in 0..40u64 {
            let now = ms(step * 10);
            let x = i32::try_from(step).unwrap() * (DWELL_REST_SLOP + 1);
            let intent = in_home_corner(Rect::new(0, 0, 1920, 1080), at(x, 1079))
                .then_some(DwellIntent::HomeCorner);
            dwell = dwell_after_motion(dwell, intent, at(x, 1079), now);
            assert!(!completed(&dwell, now), "swept through at step {step}");
        }

        // Rested: arrives once and stops. Nothing further is sent — which is exactly why the
        // timer exists — so the dwell completes on elapsed time alone.
        let arrived = dwell_after_motion(None, Some(DwellIntent::HomeCorner), at(2, 1079), ms(0));
        assert!(!completed(&arrived, ms(DWELL_MS - 1)));
        assert!(completed(&arrived, dwell_ns));

        // And drifting within the slop is still resting: the anchor does not move, so the clock
        // does not restart.
        let drifted = dwell_after_motion(
            arrived,
            Some(DwellIntent::HomeCorner),
            at(2 + DWELL_REST_SLOP, 1079),
            ms(200),
        );
        assert!(
            completed(&drifted, dwell_ns),
            "a drift within the slop rests"
        );
    }

    /// Leaving the region ends the dwell, and so does changing what it is about: one intent at a
    /// time, and the clock restarts when the intent does.
    #[test]
    fn a_dwell_ends_with_its_intent_and_restarts_with_a_new_one() {
        assert!(dwell_after_motion(None, None, at(0, 1079), 0).is_none());
        let corner =
            dwell_after_motion(None, Some(DwellIntent::HomeCorner), at(2, 1079), 0).unwrap();
        assert!(
            dwell_after_motion(Some(corner.clone()), None, at(400, 500), 5_000_000).is_none(),
            "the pointer left"
        );
        let peeking = dwell_after_motion(
            Some(corner),
            Some(DwellIntent::BarPeek("eDP-1".to_owned())),
            at(3, 1079),
            5_000_000,
        )
        .unwrap();
        assert_eq!(peeking.intent, DwellIntent::BarPeek("eDP-1".to_owned()));
        assert_eq!(peeking.since_ns, 5_000_000, "a new intent is a new clock");
    }

    #[test]
    fn a_second_dwell_on_an_already_peeked_edge_is_a_new_intent() {
        let peek = dwell_after_motion(
            None,
            Some(DwellIntent::BarPeek("eDP-1".to_owned())),
            at(0, 1079),
            0,
        )
        .unwrap();
        let promote = dwell_after_motion(
            Some(peek),
            Some(DwellIntent::BarPromote("eDP-1".to_owned())),
            at(0, 1079),
            5_000_000,
        )
        .unwrap();
        assert_eq!(promote.intent, DwellIntent::BarPromote("eDP-1".to_owned()));
        assert_eq!(promote.since_ns, 5_000_000, "promotion restarts the clock");
        assert!(
            swallow_matching_release(true, false),
            "close/park/pin still swallow the release"
        );
        assert!(
            !swallow_matching_release(true, true),
            "a grab owns its matching release"
        );
        assert!(!swallow_matching_release(false, false));
    }

    /// A framed client window, laid out the way the compositor lays one out: the record's
    /// geometry is the *frame*, and the strip is the one `templar_window` puts below the client
    /// area (`frame_around`/`tag_strip`).
    fn framed(raw: u64, client: Rect) -> PressCandidate {
        let geometry = frame_around(client);
        PressCandidate {
            window_id: WindowId(raw),
            geometry,
            is_client: true,
            has_frame: true,
            maximized: false,
            strip: Some(templar_window::tag_strip(geometry)),
            strip_over_client: false,
        }
    }

    /// A window filling the output with its own chrome shown over it: no frame, so the strip is
    /// its own client area rather than compositor-owned screen beside it.
    fn covering(raw: u64, output: Rect) -> PressCandidate {
        PressCandidate {
            window_id: WindowId(raw),
            geometry: output,
            is_client: true,
            has_frame: true,
            maximized: true,
            strip: Some(templar_window::tag_strip(output)),
            strip_over_client: true,
        }
    }

    /// A card as `press_candidates` builds one in the window overview: the presentation
    /// rectangle, and nothing else the window normally owns.
    fn card(raw: u64, rect: Rect) -> PressCandidate {
        PressCandidate {
            window_id: WindowId(raw),
            geometry: rect,
            is_client: true,
            has_frame: false,
            maximized: false,
            strip: None,
            strip_over_client: false,
        }
    }

    /// In the overview a window is a picture of itself: the whole card chooses it, and none of
    /// the affordances it wears in the ordinary scene exist — there is no band to drag, no tag
    /// to press and no resize zone outside it, because none of them was drawn.
    #[test]
    fn a_card_is_chosen_anywhere_and_offers_no_affordance() {
        let candidates = [card(1, Rect::new(200, 100, 320, 180))];
        for (x, y) in [(200, 100), (360, 190), (519, 279)] {
            assert_eq!(
                press_target_at(&candidates, at(x, y)),
                PressTarget::Window(WindowId(1)),
                "inside the card at {x},{y}"
            );
        }
        // Just outside every edge: nothing, where a framed window would have offered a resize.
        for (x, y) in [(199, 190), (520, 190), (360, 99), (360, 280), (198, 98)] {
            assert_eq!(
                press_target_at(&candidates, at(x, y)),
                PressTarget::Nothing,
                "outside the card at {x},{y}"
            );
        }
    }

    /// The overview's one pointer gesture, decided from two points and the frozen layout: a click
    /// chooses, a drag between sections groups, and anything else changes nothing.
    #[test]
    fn a_click_chooses_a_card_and_a_drag_between_sections_groups_them() {
        // Two sections, two cards each, laid out the way `taskfield::layout` stacks them.
        let sections = vec![
            vec![
                (WindowId(1), Rect::new(100, 100, 200, 120)),
                (WindowId(2), Rect::new(320, 100, 200, 120)),
            ],
            vec![
                (WindowId(3), Rect::new(100, 300, 200, 120)),
                (WindowId(4), Rect::new(320, 300, 200, 120)),
            ],
        ];
        let inside = at(150, 150);

        // A press and release on the same card, within the slop: go there.
        assert_eq!(
            taskfield_release(&sections, &[], inside, at(150 + CLICK_SLOP, 150)),
            TaskfieldRelease::Activate(Some(WindowId(1)))
        );
        // A click on the scrim: leave, and activate nothing. This is the acceptance.
        assert_eq!(
            taskfield_release(&sections, &[], at(700, 700), at(700, 700)),
            TaskfieldRelease::Activate(None)
        );
        // A click on an overflowing section's header pages that unit rather than leaving.
        assert_eq!(
            taskfield_release(
                &sections,
                &[(0, Rect::new(100, 70, 400, 28))],
                at(120, 80),
                at(120, 80)
            ),
            TaskfieldRelease::Overflow(0)
        );

        // Dragged out of its section and onto the other's: group, with that section's *first*
        // card as the anchor — the group already there is the one whose identity survives.
        assert_eq!(
            taskfield_release(&sections, &[], inside, at(400, 350)),
            TaskfieldRelease::Group {
                dragged: WindowId(1),
                anchor: WindowId(3),
            }
        );
        // And the other way, so the anchor is not simply "the lower one".
        assert_eq!(
            taskfield_release(&sections, &[], at(400, 350), inside),
            TaskfieldRelease::Group {
                dragged: WindowId(4),
                anchor: WindowId(1),
            }
        );

        // Within one section there is nothing to relate: those windows are already one unit.
        assert_eq!(
            taskfield_release(&sections, &[], inside, at(400, 150)),
            TaskfieldRelease::Cancel
        );
        // Dragged from the scrim, or dropped on it: a gesture the user thought better of leaves
        // the mode up rather than closing what they were looking at.
        assert_eq!(
            taskfield_release(&sections, &[], at(700, 700), inside),
            TaskfieldRelease::Cancel
        );
        assert_eq!(
            taskfield_release(&sections, &[], inside, at(700, 700)),
            TaskfieldRelease::Cancel
        );
        // No cards at all: every reading is "nothing".
        assert_eq!(
            taskfield_release(&[], &[], inside, at(400, 350)),
            TaskfieldRelease::Cancel
        );
    }

    /// Eight points around one window's perimeter, two per side, and the four cursor names they
    /// promise. What the pointer shows and what a press then starts come from the same
    /// hit-test, so they cannot disagree.
    #[test]
    fn the_perimeter_promises_the_resize_a_press_would_start() {
        let window = framed(1, Rect::new(100, 100, 400, 300));
        let frame = window.geometry;
        let candidates = [window];
        let shape = |x, y| match press_target_at(&candidates, at(x, y)) {
            PressTarget::Affordance(_, Affordance::Edge(edges)) => edges.cursor(),
            _ => None,
        };
        let (mid_x, mid_y) = (frame.x + frame.width / 2, frame.y + frame.height / 2);

        assert_eq!(
            shape(frame.x - 2, mid_y - 1),
            Some("nwse-resize"),
            "left, upper"
        );
        assert_eq!(
            shape(frame.x - 2, mid_y),
            Some("nesw-resize"),
            "left, lower"
        );
        assert_eq!(
            shape(frame.right() + 1, mid_y - 1),
            Some("nesw-resize"),
            "right, upper"
        );
        assert_eq!(
            shape(frame.right() + 1, mid_y),
            Some("nwse-resize"),
            "right, lower"
        );
        assert_eq!(
            shape(mid_x - 1, frame.y - 2),
            Some("nwse-resize"),
            "top, left"
        );
        assert_eq!(shape(mid_x, frame.y - 2), Some("nesw-resize"), "top, right");
        assert_eq!(
            shape(mid_x - 1, frame.bottom() + 1),
            Some("nesw-resize"),
            "bottom, left"
        );
        assert_eq!(
            shape(mid_x, frame.bottom() + 1),
            Some("nwse-resize"),
            "bottom, right"
        );

        // Inside the window and far outside it are the client's and nobody's: no promise either way.
        assert_eq!(shape(mid_x, mid_y), None);
        assert_eq!(shape(frame.x - 40, mid_y), None);
    }

    /// The fault the user reported as "the windows fight for focus": pressing a window's title
    /// band started a move and nothing else. The window being dragged was neither raised nor
    /// given the keyboard, so a person moved a window they still could not type into and the
    /// window they *had* been typing into kept the keyboard — every drag left the two disagreeing
    /// about which window was in front.
    #[test]
    fn pressing_a_windows_name_tag_focuses_that_window() {
        let window = framed(1, Rect::new(100, 140, 400, 300));
        let strip = window.strip.expect("a framed window has a strip");
        let target = press_target_at(&[window], at(strip.x + 20, strip.y + 4));
        assert_eq!(
            target,
            PressTarget::Affordance(WindowId(1), Affordance::Tag)
        );
        assert_eq!(
            press_activates(target),
            Some(WindowId(1)),
            "a press on the name tag is click-to-focus before it is a drag"
        );
        // The control tags act; the gap between the two tags belongs to nobody here.
        let close = press_target_at(&[window], at(strip.right() - 4, strip.y + 4));
        assert_eq!(
            close,
            PressTarget::Affordance(WindowId(1), Affordance::Button(ControlTag::Close))
        );
        let name = layout::name_tag(strip);
        let gap = press_target_at(&[window], at(name.right() + 2, strip.y + 4));
        assert_eq!(gap, PressTarget::Nothing, "the gap falls through");
        // A window beneath owns the gap when it is there.
        let beneath = framed(2, Rect::new(0, 0, 900, 700));
        assert_eq!(
            press_target_at(&[window, beneath], at(name.right() + 2, strip.y + 4)),
            PressTarget::Window(WindowId(2))
        );
    }

    /// A maximized window under the shell's bar has no strip: its whole frame is the client's,
    /// and its controls are the bar's.
    #[test]
    fn a_window_without_a_frame_offers_no_tags() {
        let mut window = framed(1, Rect::new(0, 0, 1600, 900));
        window.has_frame = false;
        window.maximized = true;
        window.strip = None;
        let target = press_target_at(&[window], at(1590, 928));
        assert_eq!(target, PressTarget::Window(WindowId(1)));
    }

    /// The gap between a covering window's two tags is the window's own pixels, not a hole
    /// through to whatever is behind it. Without this, every click along the bottom row of the
    /// front window raised the one underneath.
    #[test]
    fn the_gap_between_a_covering_windows_tags_still_belongs_to_that_window() {
        let output = Rect::new(0, 0, 1920, 1080);
        let front = covering(1, output);
        let behind = covering(2, output);
        let strip = templar_window::tag_strip(output);
        let middle = at(output.width / 2, strip.y + strip.height / 2);
        assert!(layout::control_tag_at(strip, middle).is_none());
        assert!(!rect_contains(layout::name_tag(strip), middle));
        assert_eq!(
            press_target_at(&[front, behind], middle),
            PressTarget::Window(WindowId(1))
        );

        // A framed window's strip really is a hole: nothing of it is drawn there.
        let over = framed(3, Rect::new(0, 0, 1920, 1000));
        let hole = templar_window::tag_strip(over.geometry);
        let gap = at(over.geometry.width / 2, hole.y + hole.height / 2);
        assert_eq!(
            press_target_at(&[over, behind], gap),
            PressTarget::Window(WindowId(2))
        );
    }

    /// A press that never moves is a click; one that leaves the slop is the drag it always was.
    /// This is the whole of "clicking the name tag pins the window, dragging still drags it".
    #[test]
    fn a_name_tag_press_that_never_moves_is_a_click() {
        let pressed = at(400, 928);
        assert!(is_click(pressed, pressed));
        assert!(is_click(pressed, at(400 + CLICK_SLOP, 928 - CLICK_SLOP)));
        assert!(!is_click(pressed, at(400 + CLICK_SLOP + 1, 928)));
        assert!(!is_click(pressed, at(400, 928 + CLICK_SLOP + 1)));
    }

    /// The same for the client area — the ordinary click-to-focus case — and for the resize
    /// zones, so a window grabbed by its edge comes forward like one grabbed by its title.
    #[test]
    fn pressing_a_windows_own_area_or_its_resize_zone_focuses_it() {
        let window = framed(1, Rect::new(100, 140, 400, 300));
        let geometry = window.geometry;

        let inside = press_target_at(&[window], at(geometry.x + 50, geometry.y + 20));
        assert_eq!(inside, PressTarget::Window(WindowId(1)));
        assert_eq!(press_activates(inside), Some(WindowId(1)));

        let edge = press_target_at(&[window], at(geometry.right() + 1, geometry.y + 60));
        assert!(
            matches!(
                edge,
                PressTarget::Affordance(WindowId(1), Affordance::Edge(_))
            ),
            "{edge:?}"
        );
        assert_eq!(press_activates(edge), Some(WindowId(1)));
    }

    /// Close and park are the two presses that must *not* focus: activating a window in order to
    /// close it, or to send it to the shelf a frame later, takes the keyboard away from whatever
    /// the person was typing into and hands it to something about to disappear.
    #[test]
    fn closing_or_parking_a_window_does_not_hand_it_the_keyboard_first() {
        for button in [ControlTag::Close, ControlTag::Park] {
            assert_eq!(
                press_activates(PressTarget::Affordance(
                    WindowId(1),
                    Affordance::Button(button)
                )),
                None,
                "{button:?}"
            );
        }
        assert_eq!(
            press_activates(PressTarget::Affordance(
                WindowId(1),
                Affordance::Button(ControlTag::Maximize)
            )),
            Some(WindowId(1)),
            "maximizing a background window brings it forward, like every other band press"
        );
        assert_eq!(press_activates(PressTarget::Nothing), None);
    }

    /// The stacking order settles overlap, and it settles it once: the window on top owns every
    /// point inside its frame, including the points where the window beneath it has drawn its own
    /// title band. Chrome that shows through from underneath (the compositing fault fixed in
    /// `src/scene.rs`) must never become a click target — a person clicking a title bar they can
    /// see and having a different window take the keyboard is exactly what "fighting for focus"
    /// looks like.
    #[test]
    fn the_window_on_top_owns_every_point_inside_its_frame() {
        let top = framed(1, Rect::new(100, 340, 400, 300));
        let buried = framed(2, Rect::new(120, 140, 400, 300));
        let buried_strip = buried.strip.expect("a framed window has a strip");
        let overlap = at(buried_strip.x + 20, buried_strip.y + 4);
        assert!(
            rect_contains(top.geometry, overlap),
            "the test point overlaps"
        );
        assert!(
            !rect_contains(top.strip.expect("a framed window has a strip"), overlap),
            "and lands in the top window's client area rather than its own tags"
        );

        let target = press_target_at(&[top, buried], overlap);
        assert_eq!(
            target,
            PressTarget::Window(WindowId(1)),
            "the point is inside the top window's frame, so it belongs to the top window"
        );
    }

    /// A shell surface (the Home Surface) is not a client window: it takes the keyboard, but it
    /// is never activated or raised, and it has no band to press.
    #[test]
    fn a_shell_surface_takes_the_press_without_offering_a_tag() {
        let home = PressCandidate {
            window_id: WindowId(9),
            geometry: Rect::new(0, 0, 1920, 1080),
            is_client: false,
            has_frame: false,
            maximized: false,
            strip: None,
            strip_over_client: false,
        };
        let target = press_target_at(&[home], at(400, 400));
        assert_eq!(target, PressTarget::Window(WindowId(9)));
        assert_eq!(press_activates(target), Some(WindowId(9)));
    }

    /// A press over no window at all is nobody's, and falls through to the surface-tree walk
    /// that finds popups.
    #[test]
    fn a_press_over_nothing_belongs_to_nobody() {
        let window = framed(1, Rect::new(100, 140, 400, 300));
        assert_eq!(
            press_target_at(&[window], at(1500, 900)),
            PressTarget::Nothing
        );
        assert_eq!(press_target_at(&[], at(10, 10)), PressTarget::Nothing);
    }

    fn clamp(
        position: Point<f64, Logical>,
        outputs: &[Rectangle<i32, Logical>],
    ) -> Point<f64, Logical> {
        clamp_to_outputs(position, || outputs.iter().copied())
    }

    #[test]
    fn relative_pointer_positions_inside_an_output_are_unchanged() {
        let position = Point::from((42.5, 18.25));
        assert_eq!(clamp(position, &[output(0, 0, 100, 100)]), position);
    }

    #[test]
    fn relative_pointer_clamping_handles_negative_origin_outputs() {
        let outputs = [output(-200, -100, 100, 100), output(0, 0, 100, 100)];
        assert_eq!(
            clamp(Point::from((-250.0, -50.0)), &outputs),
            Point::from((-200.0, -50.0))
        );
    }

    #[test]
    fn relative_pointer_clamping_chooses_the_nearest_output_across_a_gap() {
        let outputs = [output(0, 0, 100, 100), output(200, 0, 100, 100)];
        assert_eq!(
            clamp(Point::from((180.0, 50.0)), &outputs),
            Point::from((200.0, 50.0))
        );
    }

    #[test]
    fn no_outputs_leaves_the_relative_pointer_position_unchanged() {
        let position = Point::from((-12.0, 300.0));
        assert_eq!(clamp(position, &[]), position);
    }
}
