//! Input: pointer motion/button/axis, keyboard, and the drag grab. COMPOSITOR.md's rows for
//! "pointer button down on a window's decoration/recovery band", "pointer motion during a drag"
//! and "pointer button up ending a drag" all live here, plus the fixed recovery keybindings
//! (`src/keybindings.rs`) intercepted before a key ever reaches a client.
//!
//! Coordinates are converted from device/physical space to the one global logical scene exactly
//! once, at `AbsolutePositionEvent::position_transformed` — the same boundary
//! `lege_window::geometry::physical_to_logical` exists for — and never again: everything from
//! `MotionEvent` onward (`WindowService::update_drag`, hit-testing, focus) stays in that one
//! logical space, matching `crates/lege-window`'s own invariant.

use crate::chrome::layout::{self, ControlTag};
use crate::state::{Backend, LegeCompState};
use lege_window::{Layer, Point as LegePoint, RevealHome, WindowId};
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

use std::time::Duration;

impl<B: Backend + 'static> SeatHandler for LegeCompState<B> {
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

impl<B: Backend + 'static> SelectionHandler for LegeCompState<B> {
    type SelectionUserData = ();
}

impl<B: Backend + 'static> DataDeviceHandler for LegeCompState<B> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl<B: Backend + 'static> smithay::input::dnd::DndGrabHandler for LegeCompState<B> {}
impl<B: Backend + 'static> WaylandDndGrabHandler for LegeCompState<B> {}

/// Give `window_id` keyboard focus: `WindowService::activate` plus the matching
/// `xdg_toplevel.configure{activated}` round trip on every affected toplevel — the
/// newly-activated one and whichever previously held it (`WindowService::activate` already
/// cleared its `activated` flag; this just makes Smithay/the client agree). Used both from an
/// ordinary pointer click here and from a successful xdg-activation grant
/// (`src/xdg_shell.rs::XdgActivationHandler::request_activation`).
pub fn activate_window<B: Backend + 'static>(state: &mut LegeCompState<B>, window_id: WindowId) {
    // Who had the keyboard *before* this — what `Super+G` groups the focused window with. Read
    // before the activation, because `WindowService::activate` clears the flag it comes from.
    let outgoing = state
        .window_service
        .windows()
        .find(|window| window.activated)
        .map(|window| window.id)
        .filter(|id| *id != window_id);
    if !state.window_service.activate(window_id) {
        return;
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
pub fn refocus_after_reveal<B: Backend + 'static>(state: &mut LegeCompState<B>, revealed: bool) {
    if revealed {
        let home = state
            .window_service
            .stacking_order()
            .into_iter()
            .find(|id| state.window_service.layer_of(*id) == Some(lege_window::Layer::HomeSurface));
        let surface = home.and_then(|id| state.windows.surface_for(id).cloned());
        if let Some(keyboard) = state.seat.get_keyboard() {
            keyboard.set_focus(state, surface, SERIAL_COUNTER.next_serial());
        }
        return;
    }
    let top = state
        .window_service
        .stacking_order()
        .into_iter()
        .find(|id| {
            state.window_service.is_visible(*id)
                && matches!(
                    state.window_service.layer_of(*id),
                    Some(lege_window::Layer::Normal | lege_window::Layer::AppOnTop)
                )
        });
    if let Some(window_id) = top {
        activate_window(state, window_id);
    }
}

/// Focus the Home surface when it is the only usable desktop, or while a reveal explicitly owns
/// the screen. Initial shell mapping otherwise leaves the seat with no keyboard target, which
/// makes a correctly rendered Home Field look completely inert.
pub fn focus_home_if_needed<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    surface: &WlSurface,
) {
    let has_visible_client =
        state.window_service.stacking_order().into_iter().any(|id| {
            state.window_service.is_client_window(id) && state.window_service.is_visible(id)
        });
    if (state.window_service.home_revealed() || !has_visible_client)
        && let Some(keyboard) = state.seat.get_keyboard()
    {
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
    Edge(lege_window::ResizeEdges),
}

/// One window as the press hit test sees it — everything [`press_target_at`] needs and nothing
/// else, so the decision can be exercised with no compositor, no `Space` and no display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PressCandidate {
    window_id: WindowId,
    /// The window's *frame* (tag strip included), which is what owns the point.
    geometry: lege_window::Rect,
    is_client: bool,
    has_frame: bool,
    maximized: bool,
    /// The tag strip along the bottom edge, when the window wears one (`has_frame`) or is being
    /// shown one over itself (`covering_chrome`).
    strip: Option<lege_window::Rect>,
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
fn press_target_at(candidates: &[PressCandidate], pos: LegePoint) -> PressTarget {
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
            && let Some(edges) = lege_window::resize_edges_at(candidate.geometry, pos)
        {
            return PressTarget::Affordance(candidate.window_id, Affordance::Edge(edges));
        }
    }
    PressTarget::Nothing
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
fn park_window<B: Backend + 'static>(state: &mut LegeCompState<B>, window_id: WindowId) {
    let ctx = lege_window::CommandContext {
        trusted_shell: true,
        now_monotonic_ns: crate::window_bridge::now_monotonic_ns(&state.clock),
    };
    if state
        .window_service
        .handle_command(lege_window::WindowCommand::Shelve { window_id }, ctx)
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
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
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
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
    serial: Serial,
    window_id: WindowId,
    edges: lege_window::ResizeEdges,
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
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
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
    let pointer_global = to_lege_point(pointer.current_location());
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
        pressed_at: to_lege_point(start_data.location),
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
fn shift_held<B: Backend + 'static>(state: &LegeCompState<B>) -> bool {
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
fn to_lege_point(point: Point<f64, Logical>) -> LegePoint {
    LegePoint {
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
    Resize { edges: lege_window::ResizeEdges },
}

/// The interactive move/resize grab. Every event but `motion`/`button` is forwarded to Smithay's
/// default handling unchanged — a drag has no opinion about scroll wheels or gestures — matching
/// the shape every Smithay compositor's move grab takes (`anvil`, `smallvil`, `cosmic-comp`).
/// `motion` and `button` are the two rows COMPOSITOR.md specifies: the pointer's position applied
/// as-is, never clamped, and the invariant enforced once at release.
pub struct WindowGestureGrab<B: Backend + 'static> {
    start_data: PointerGrabStartData<LegeCompState<B>>,
    window_id: WindowId,
    gesture: Gesture,
    /// Where the press landed, so the release can tell a click from a drag.
    pressed_at: LegePoint,
    /// Whether a release that never left [`CLICK_SLOP`] of [`Self::pressed_at`] pins the window.
    pins_on_click: bool,
    /// The other members of the dragged window's group, with their offsets from it: a band drag
    /// moves the whole group. Empty for a resize, for an ungrouped window and for a `Shift`-held
    /// drag, which moves that member alone.
    followers: Vec<(WindowId, lege_window::Point)>,
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

impl<B: Backend + 'static> PointerGrab<LegeCompState<B>> for WindowGestureGrab<B> {
    fn motion(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        let pointer = to_lege_point(event.location);
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
                        let placed = lege_window::Rect::new(
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
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&BTN_LEFT) {
            // A name tag that was pressed and let go without moving is a click, and a click on a
            // name tag pins the window above the others. Checked before the drag is finished, so
            // "did it move" is asked of the pointer rather than of a geometry that a repair may
            // have changed for reasons of its own.
            let released_at = to_lege_point(handle.current_location());
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
                    if let Some((_, geometry)) = data.window_service.finish_drag() {
                        crate::space_sync::apply_dragged_geometry(data, self.window_id, geometry);
                        if !self.followers.is_empty() {
                            // The invariant is enforced once, at the end, for the whole group:
                            // `move_group` carries every member through the anchor's own repair
                            // displacement and then repairs each of them in turn.
                            for (id, moved) in
                                data.window_service.move_group(self.window_id, geometry)
                            {
                                crate::space_sync::apply_dragged_geometry(data, id, moved);
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
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }
    fn gesture_swipe_update(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }
    fn gesture_swipe_end(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }
    fn gesture_pinch_begin(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }
    fn gesture_pinch_update(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }
    fn gesture_pinch_end(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }
    fn gesture_hold_begin(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }
    fn gesture_hold_end(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<LegeCompState<B>> {
        &self.start_data
    }

    /// The grab ending for any reason at all — including one this grab did not handle, such as
    /// the seat being taken away — clears the drop-target highlight, so a band can never be left
    /// glowing at a drag that is over.
    fn unset(&mut self, data: &mut LegeCompState<B>) {
        data.drop_target = None;
    }
}

impl<B: Backend + 'static> LegeCompState<B> {
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

    fn on_keyboard<I: InputBackend>(&mut self, event: &I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(event);
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        // The pressure overlay is modal while it is showing: every key is captured here, never
        // forwarded to whatever client happened to have focus before it appeared (COMPOSITOR.md's
        // "Layer::Recovery ... above everything except the lock screen"). Gated on `!locked` for
        // the same reason `run_recovery_action` below is: a hostile overlay state must not become
        // a way to interact with anything while the session is locked (`src/session_lock.rs`'s
        // module doc on partial input isolation).
        if !self.session_lock.locked && self.recovery_protocol.pressure_overlay.is_some() {
            let pressed = event.state() == smithay::backend::input::KeyState::Pressed;
            let action = keyboard.input::<Option<crate::keybindings::OverlayAction>, _>(
                self,
                event.key_code(),
                event.state(),
                serial,
                time,
                |_, modifiers, handle| {
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
            return;
        }

        let home_revealed = self.window_service.home_revealed();
        let action = keyboard.input::<Option<crate::keybindings::RecoveryAction>, _>(
            self,
            event.key_code(),
            event.state(),
            serial,
            time,
            |state, modifiers, handle| {
                let keysym = handle.modified_sym();
                // The switcher walks a list frozen for the length of the hold; this is where the
                // release that ends it is seen. Read from the modifier state rather than matched
                // by keysym, so either Alt (or either Super) ending the walk cannot be missed.
                state.chord_held = modifiers.alt || modifiers.logo;
                match state
                    .super_tap
                    .update(event.state(), keysym, time, home_revealed)
                {
                    crate::keybindings::SuperTapDecision::Intercept => {
                        return FilterResult::Intercept(None);
                    }
                    crate::keybindings::SuperTapDecision::Reveal => {
                        state.pending_super_reveal = Some(lege_window::RevealHome::Show);
                        return FilterResult::Intercept(None);
                    }
                    crate::keybindings::SuperTapDecision::SetReveal(revealed) => {
                        state.pending_super_reveal = Some(if revealed {
                            lege_window::RevealHome::Show
                        } else {
                            lege_window::RevealHome::Hide
                        });
                        return FilterResult::Intercept(None);
                    }
                    crate::keybindings::SuperTapDecision::ForwardEndingReveal => {
                        // A chord key arrived during the momentary reveal: end it first, so the
                        // chord's own action (which may itself be a reveal toggle, e.g. Super+D)
                        // computes from the restored state, then let the key take its normal
                        // path below.
                        state.pending_super_reveal = Some(lege_window::RevealHome::Hide);
                    }
                    crate::keybindings::SuperTapDecision::Forward => {}
                }
                if event.state() != smithay::backend::input::KeyState::Pressed {
                    return FilterResult::Forward;
                }
                match crate::keybindings::recovery_action(*modifiers, keysym, home_revealed) {
                    Some(action) => FilterResult::Intercept(Some(action)),
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

        if let Some(Some(action)) = action {
            // Any reserved action other than another switcher step ends the walk;
            // `run_recovery_action` itself repopulates the ring and the HUD for a step.
            if !matches!(
                action,
                crate::keybindings::RecoveryAction::CycleFocusForward
                    | crate::keybindings::RecoveryAction::CycleFocusBackward
                    | crate::keybindings::RecoveryAction::GroupCycleForward
                    | crate::keybindings::RecoveryAction::GroupCycleBackward
            ) {
                self.switch_ring = None;
            }
            if !self.session_lock.locked {
                self.run_recovery_action(action);
            }
        } else if event.state() == smithay::backend::input::KeyState::Pressed {
            // Some other key: the walk is over. A *release* must not end it — clearing on every
            // forwarded key is what used to kill the HUD one keystroke after it appeared, on the
            // release of the very Tab that opened it.
            self.switch_ring = None;
        }
        if !self.chord_held {
            self.switch_ring = None;
        }
        // The HUD lives exactly as long as the list it draws.
        if self.switch_ring.is_none() {
            self.switcher = None;
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

        let outputs: Vec<Rectangle<i32, Logical>> = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .collect();
        let position = clamp_to_outputs(current + event.delta(), &outputs);
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
        self.update_perimeter_cursor(to_lege_point(position));
        self.update_dwell(to_lege_point(position));
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
        self.update_perimeter_cursor(to_lege_point(pos));
        self.update_dwell(to_lege_point(pos));
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: &I::PointerButtonEvent) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();

        // Same modality as `on_keyboard`'s overlay branch: a click while the overlay is up picks
        // a button (or does nothing, on a miss) and is never forwarded to a client underneath.
        if !self.session_lock.locked && self.recovery_protocol.pressure_overlay.is_some() {
            if button_state == ButtonState::Pressed {
                self.handle_pressure_overlay_click(pointer.current_location());
            }
            return;
        }

        if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
            // A click means the user is doing something else — the fallback switcher HUD
            // (`chrome::SwitcherState`) is done once anything but another Alt+Tab step happens.
            self.switcher = None;
            self.swallowing_press =
                self.handle_press_for_focus_or_drag(pointer.current_location(), serial, button);
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
        let pos = to_lege_point(location);
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
        let area = lege_window::Rect::new(
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
    /// is that window's resize zone (`lege_window::resize_edges_at`), and a window above another
    /// takes its zone before the one beneath sees the press — the invisible border belongs to
    /// the window on top.
    fn press_target_at(&self, pos: LegePoint) -> PressTarget {
        press_target_at(&self.press_candidates(), pos)
    }

    /// Every visible window as the hit-test sees it, top first. Shared by the press path and the
    /// motion path, so what the cursor promises and what a press then does cannot disagree.
    fn press_candidates(&self) -> Vec<PressCandidate> {
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
    fn update_perimeter_cursor(&mut self, pos: LegePoint) {
        if self.session_lock.locked || self.pointer_gesture_active() {
            return;
        }
        let shape = match press_target_at(&self.press_candidates(), pos) {
            PressTarget::Affordance(_, Affordance::Edge(edges)) => edges.cursor(),
            _ => None,
        };
        self.show_compositor_cursor(shape);
    }

    /// A dwell at the bottom of the screen: the bottom-left corner is Home
    /// ([`DwellIntent::HomeCorner`]), either corner of a window filling the screen is that
    /// window's own chrome ([`DwellIntent::CoveringChrome`]), and the edge between them asks for
    /// the shell's bar ([`DwellIntent::BarPeek`]).
    ///
    /// Both have to be the compositor's rule rather than the shell's: whatever is under the
    /// pointer owns it, so under a maximized or fullscreen window the shell never learns the
    /// pointer reached the edge at all. And both need a timer, because a client that is not
    /// animating sends no further motion once the pointer stops — "rested here for a moment" is
    /// exactly the state that separates a deliberate reach from a pointer sweeping past.
    ///
    /// Nothing about the bar surface changes for a peek: it stays mapped, it keeps its exclusive
    /// zone, and so no work area is recomputed and no window reflows on the way in or out. Only
    /// the stacking order does ([`lege_window::WindowService::set_peek_output`]).
    fn update_dwell(&mut self, pos: LegePoint) {
        if self.session_lock.locked {
            self.dwell = None;
            return;
        }
        // A peek ends the moment the pointer walks away from it — no dwell on the way out, the
        // same asymmetry the shell's own reveal has. A *reveal* of Home does not end here: it is
        // a deliberate act, and `WindowService::set_home_revealed` (a click, `Super+H`, a window
        // being activated) is what decides when Home goes away again.
        if let Some(peeked) = self.window_service.peek_output().map(str::to_owned)
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
        let pos = to_lege_point(pointer.current_location());
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
                if self.window_service.set_peek_output(Some(output)) {
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
    /// The three regions are disjoint by construction (`in_peek_band` excludes both corner
    /// squares), so there is never a precedence question between them: each region has one rule.
    ///
    /// - the bottom-left square is **Home**, whatever is on the screen. The bar has no Home chip,
    ///   so this is the only pointer route there, and a window filling the screen is exactly when
    ///   it is needed most — giving the corner to the window would leave Home reachable only from
    ///   the keyboard for as long as anything was maximized, which is most of the time.
    /// - the bottom-right square is a screen-filling window's **own chrome**, shown over it. One
    ///   dwell shows the whole strip, tag included, because it *is* one strip.
    /// - the edge between them asks for the shell's bar back.
    ///
    /// The one state-dependent part: while that chrome is already showing, neither corner asks
    /// for anything. The strip is under the pointer and the strip is what is being reached for —
    /// without this, moving onto the window's name tag, which lands in the left-hand square,
    /// would reveal Home out from under the click.
    fn dwell_intent_at(&self, pos: LegePoint) -> Option<DwellIntent> {
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

        if in_home_corner(output.logical, pos) || in_controls_corner(output.logical, pos) {
            // Not while the shell's own chrome is under the pointer: whatever it is showing there
            // is what the person is reaching for, and it gets the press. This is why the island
            // is centred (`lege-shell-v1.xml`'s `set_edge_alignment`) — a bottom-anchored surface
            // at the start of its edge sits exactly on top of the left-hand square.
            if self.over_shell_overlay(pos) {
                return None;
            }
            // Already showing: the strip is the thing under the pointer, and neither corner is
            // asking for anything else while it is.
            if self.window_service.covering_chrome().is_some() {
                return None;
            }
            // The user's rule: "close/maximize/minimize should appear on hover in the bottom
            // right, over the full screen window, not adjacent to it. same for the tag in the
            // bottom left." One dwell shows both — they are one strip.
            if in_controls_corner(output.logical, pos) {
                return covering.map(DwellIntent::CoveringChrome);
            }
            // Nothing to ask for if Home is already showing. Without this, a pointer resting in
            // the corner re-fires the reveal on every twitch inside the rest slop, and each one
            // costs a re-sync and a refocus for a state that has not changed.
            return (!self.window_service.home_revealed()).then_some(DwellIntent::HomeCorner);
        }
        // The strip a covering window's chrome puts on screen is `TAG_HEIGHT` tall and the peek
        // band is its bottom ten pixels, so without this, resting on the close tag long enough to
        // aim at it opens the switcher underneath the thing being aimed at.
        if self.window_service.covering_chrome().is_some()
            || !in_peek_band(output.logical, pos)
            || self.window_service.peek_output() == Some(output.name.as_str())
        {
            return None;
        }
        covering.map(|_| DwellIntent::BarPeek(output.name.clone()))
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
    fn holding_covering_chrome(&self, window_id: WindowId, pos: LegePoint) -> bool {
        let Some(window) = self.window_service.window(window_id) else {
            return false;
        };
        if in_home_corner(window.geometry, pos) || in_controls_corner(window.geometry, pos) {
            return true;
        }
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
    fn holding_peek(&self, output: &str, pos: LegePoint) -> bool {
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
    fn over_shell_overlay(&self, pos: LegePoint) -> bool {
        self.window_service.windows().any(|window| {
            self.window_service.layer_of(window.id) == Some(Layer::ShellOverlay)
                && self.window_service.is_visible(window.id)
                && rect_contains(window.geometry, pos)
        })
    }

    /// Show `shape` (a `cursor-spec` name) as the compositor's own cursor, or `None` to give the
    /// pointer back to whatever the client had. Idempotent: showing the shape already on screen
    /// changes nothing, so this is safe to call on every motion event.
    fn show_compositor_cursor(&mut self, shape: Option<&'static str>) {
        match shape {
            Some(name) => {
                // `lege_window` names the shape the way `cursor-spec` does, so the enum comes
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
        let pos = to_lege_point(location);
        let target = self.press_target_at(pos);

        // Click to focus, and only click — LegeOS does not follow the mouse. Pressing anywhere a
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
fn clamp_to_outputs(
    position: Point<f64, Logical>,
    outputs: &[Rectangle<i32, Logical>],
) -> Point<f64, Logical> {
    if outputs
        .iter()
        .any(|geometry| geometry.to_f64().contains(position))
    {
        return position;
    }
    outputs
        .iter()
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

fn rect_contains(rect: lege_window::Rect, point: LegePoint) -> bool {
    point.x >= rect.x && point.x < rect.right() && point.y >= rect.y && point.y < rect.bottom()
}

/// The square at the bottom-left of an output that is the route to the Home Surface, in logical
/// pixels.
///
/// A calibration knob rather than a constant of nature: a person pushing a real mouse into the
/// bottom-left of a real screen has to be able to land on it, and a fractional scale eats a pixel
/// or two of it. Widen it if the corner is hard to hit; narrow it if it fires on its own.
const HOME_CORNER: i32 = 8;
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
/// How long the pointer must rest before either dwell fires.
///
/// The same number as the shell's own reveal (`lege_shell::surface::bar_screen::HINT_OPEN_MS`),
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
fn is_click(pressed: LegePoint, released: LegePoint) -> bool {
    (released.x - pressed.x).abs() <= CLICK_SLOP && (released.y - pressed.y).abs() <= CLICK_SLOP
}

/// What the pointer resting at the bottom of the screen is asking for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DwellIntent {
    /// The bottom-left corner: reveal the Home Surface.
    HomeCorner,
    /// The rest of the bottom edge of a named output carrying a window that fills the screen:
    /// raise the shell's overlay over it (fullscreen, which outranks it) and tell the shell the
    /// edge has been asked for (`bar_peeked`, either way). The shell cannot see this for itself —
    /// whatever is under the pointer owns it — so it is the compositor that has to notice.
    BarPeek(String),
    /// Either bottom corner of a window that fills the screen: show that window's own name tag
    /// and controls over it. Such a window has no frame to reach for and the shell's bar carries
    /// no copy of its controls, so without this there is no pointer route to closing it at all.
    CoveringChrome(WindowId),
}

/// A dwell in progress: what it is for, where the pointer settled, and when it settled there.
#[derive(Clone, Debug)]
pub struct Dwell {
    pub intent: DwellIntent,
    at: LegePoint,
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
    pos: LegePoint,
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

/// Whether `pos` is in the square at `output`'s bottom-left corner that reveals Home.
fn in_home_corner(output: lege_window::Rect, pos: LegePoint) -> bool {
    pos.x >= output.x
        && pos.x < output.x + HOME_CORNER
        && pos.y >= output.bottom() - HOME_CORNER
        && pos.y < output.bottom()
}

/// The bottom-*right* square, mirroring [`in_home_corner`]. It is where a fullscreen window's
/// controls appear, and it means nothing when no window is fullscreen — a corner that does
/// something only sometimes is better than one that does two things.
fn in_controls_corner(output: lege_window::Rect, pos: LegePoint) -> bool {
    pos.x >= output.right() - HOME_CORNER
        && pos.x < output.right()
        && pos.y >= output.bottom() - HOME_CORNER
        && pos.y < output.bottom()
}

/// Whether `pos` is in the band along `output`'s bottom edge that opens a peek.
///
/// Disjoint from [`in_home_corner`] by construction rather than by a precedence rule: the corner
/// is Home and the rest of the edge is the bar, so one pointer position never means two things.
fn in_peek_band(output: lege_window::Rect, pos: LegePoint) -> bool {
    pos.x >= output.x
        && pos.x < output.right()
        && pos.y >= output.bottom() - PEEK_BAND
        && pos.y < output.bottom()
        && !in_home_corner(output, pos)
        && !in_controls_corner(output, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_window::{Rect, frame_around};
    use smithay::utils::Size;

    fn output(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((width, height)))
    }

    fn at(x: i32, y: i32) -> LegePoint {
        LegePoint { x, y }
    }

    /// The two regions along the bottom of the screen, and the fact that they are disjoint: the
    /// corner square is Home, the rest of the edge is the bar, and no point is both. That is what
    /// makes "the corner works under a fullscreen window too" a rule rather than a precedence
    /// argument — the peek never gets a chance to claim it.
    #[test]
    fn the_corner_and_the_peek_band_divide_the_bottom_edge_between_them() {
        let screen = Rect::new(0, 0, 1920, 1080);
        // The band is taller than the corner square (it has to be crossed to reach a six-pixel
        // tab), so the two pixels directly above the square are band, not nothing. That is
        // deliberate and still disjoint: only one of the two ever answers for a given point.
        for (x, y, corner, band) in [
            (0, 1079, true, false),    // the true corner
            (7, 1072, true, false),    // the far edge of the square
            (8, 1079, false, true),    // one pixel past it, along the bottom
            (0, 1071, false, true),    // directly above the square, inside the band
            (0, 1069, false, false),   // and one pixel above the band
            (7, 1079, true, false),    // inside the square, on the bottom row
            (960, 1079, false, true),  // the middle of the edge
            (1900, 1076, false, true), // the other end of it, short of the controls' corner
            (960, 1069, false, false), // above the band
            (960, 1080, false, false), // off the output
        ] {
            assert_eq!(
                in_home_corner(screen, at(x, y)),
                corner,
                "corner at {x},{y}"
            );
            assert_eq!(in_peek_band(screen, at(x, y)), band, "band at {x},{y}");
            assert!(
                !(in_home_corner(screen, at(x, y)) && in_peek_band(screen, at(x, y))),
                "disjoint at {x},{y}"
            );
        }
    }

    /// A fullscreen window owns both bottom corners: the tag at the left, the controls at the
    /// right, drawn over it rather than beside it. The user's rule, and the reason for it is that
    /// there is otherwise no pointer route to closing a fullscreen window at all — it wears no
    /// frame, and the island carries no copy of its controls.
    #[test]
    fn a_fullscreen_window_owns_both_bottom_corners_and_the_bar_owns_the_edge_between_them() {
        let screen = Rect::new(0, 0, 1920, 1080);
        for (x, y, left, right, band) in [
            (0, 1079, true, false, false),     // the tag's corner
            (1919, 1079, false, true, false),  // the controls' corner
            (1912, 1072, false, true, false),  // the far edge of that square
            (1911, 1079, false, false, true),  // one pixel inside it, along the bottom
            (960, 1079, false, false, true),   // the middle of the edge is the switcher's
            (960, 1069, false, false, false),  // above the band
            (1919, 1069, false, false, false), // and above the right-hand square
        ] {
            assert_eq!(in_home_corner(screen, at(x, y)), left, "left at {x},{y}");
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

    /// A framed client window, laid out the way the compositor lays one out: the record's
    /// geometry is the *frame*, and the strip is the one `lege_window` puts below the client
    /// area (`frame_around`/`tag_strip`).
    fn framed(raw: u64, client: Rect) -> PressCandidate {
        let geometry = frame_around(client);
        PressCandidate {
            window_id: WindowId(raw),
            geometry,
            is_client: true,
            has_frame: true,
            maximized: false,
            strip: Some(lege_window::tag_strip(geometry)),
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
            strip: Some(lege_window::tag_strip(output)),
            strip_over_client: true,
        }
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
        let strip = lege_window::tag_strip(output);
        let middle = at(output.width / 2, strip.y + strip.height / 2);
        assert!(layout::control_tag_at(strip, middle).is_none());
        assert!(!rect_contains(layout::name_tag(strip), middle));
        assert_eq!(
            press_target_at(&[front, behind], middle),
            PressTarget::Window(WindowId(1))
        );

        // A framed window's strip really is a hole: nothing of it is drawn there.
        let over = framed(3, Rect::new(0, 0, 1920, 1000));
        let hole = lege_window::tag_strip(over.geometry);
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

    #[test]
    fn relative_pointer_positions_inside_an_output_are_unchanged() {
        let position = Point::from((42.5, 18.25));
        assert_eq!(
            clamp_to_outputs(position, &[output(0, 0, 100, 100)]),
            position
        );
    }

    #[test]
    fn relative_pointer_clamping_handles_negative_origin_outputs() {
        let outputs = [output(-200, -100, 100, 100), output(0, 0, 100, 100)];
        assert_eq!(
            clamp_to_outputs(Point::from((-250.0, -50.0)), &outputs),
            Point::from((-200.0, -50.0))
        );
    }

    #[test]
    fn relative_pointer_clamping_chooses_the_nearest_output_across_a_gap() {
        let outputs = [output(0, 0, 100, 100), output(200, 0, 100, 100)];
        assert_eq!(
            clamp_to_outputs(Point::from((180.0, 50.0)), &outputs),
            Point::from((200.0, 50.0))
        );
    }

    #[test]
    fn no_outputs_leaves_the_relative_pointer_position_unchanged() {
        let position = Point::from((-12.0, 300.0));
        assert_eq!(clamp_to_outputs(position, &[]), position);
    }
}
