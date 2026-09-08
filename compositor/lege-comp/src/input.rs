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
use lege_window::{Point as LegePoint, WindowId};
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
    reexports::calloop::timer::Timer,
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
    /// The tag strip along the bottom edge, when the window wears one (`has_frame`).
    strip: Option<lege_window::Rect>,
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
                // The gap between the two tags: no pixels of this window are there, so the press
                // belongs to whatever is behind it.
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
#[derive(Clone, Copy, Debug)]
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
        start_data,
        window_id,
        gesture,
        followers,
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

fn to_lege_point(point: Point<f64, Logical>) -> LegePoint {
    LegePoint {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
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
        self.update_bar_peek(to_lege_point(position));
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
        self.update_bar_peek(to_lege_point(pos));
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
            self.handle_press_for_focus_or_drag(pointer.current_location(), serial, button);
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
                Some(PressCandidate {
                    window_id,
                    geometry: record.geometry,
                    is_client: self.window_service.is_client_window(window_id),
                    has_frame: self.window_service.has_frame(window_id),
                    maximized: record.maximized,
                    strip: if self.window_service.has_frame(window_id) {
                        self.window_service
                            .recovery_affordance(window_id)
                            .map(|affordance| affordance.strip)
                    } else {
                        None
                    },
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

    /// A dwell on an output's bottom edge peeks the shell's bar out from under a fullscreen
    /// window ([`lege_window::WindowService::set_peek_output`]).
    ///
    /// This has to be the compositor's rule rather than the shell's: while a window is fullscreen
    /// it is the topmost surface on that output, so it receives the pointer and the shell never
    /// learns that the pointer reached the bottom edge at all. Nothing about the bar surface
    /// changes — it stays mapped, it keeps its exclusive zone, and so nothing reflows on the way
    /// in or out; only the stacking order does.
    ///
    /// The dwell needs a timer because a client that is not animating sends no further motion
    /// once the pointer stops moving, and "rested here for a moment" is exactly the state that
    /// separates a deliberate reach for the bar from a pointer sweeping past.
    fn update_bar_peek(&mut self, pos: LegePoint) {
        if self.session_lock.locked {
            return;
        }
        let peeking = self.window_service.peek_output().is_some();
        let Some(candidate) = self.peek_output_at(pos, peeking) else {
            self.peek_armed = None;
            if self.window_service.set_peek_output(None) {
                crate::space_sync::sync_after_change(self);
            }
            return;
        };
        // Already up, or already waiting on this same edge: nothing to arm.
        if peeking || self.peek_armed.as_deref() == Some(candidate.as_str()) {
            return;
        }
        self.peek_armed = Some(candidate);
        let timer = Timer::from_duration(Duration::from_millis(PEEK_DWELL_MS));
        // Nothing cancels this timer: `finish_bar_peek` re-reads where the pointer actually is,
        // so a stale one fires into a no-op. One `Drop`ping timer per approach to the edge is
        // cheaper than tracking a `RegistrationToken` to remove.
        if let Err(error) = self
            .loop_handle
            .insert_source(timer, |_, (), state: &mut Self| {
                state.finish_bar_peek();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            })
        {
            tracing::warn!(%error, "arming the bar peek");
            self.peek_armed = None;
        }
    }

    /// The dwell elapsed: peek, if the pointer is still on the edge that armed it.
    fn finish_bar_peek(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let pos = to_lege_point(pointer.current_location());
        let Some(output) = self.peek_output_at(pos, false) else {
            self.peek_armed = None;
            return;
        };
        if self.peek_armed.as_deref() != Some(output.as_str()) {
            return;
        }
        if self.window_service.set_peek_output(Some(output)) {
            crate::space_sync::sync_after_change(self);
        }
    }

    /// The output whose bar `pos` is reaching for, if any: an output with a visible fullscreen
    /// client window on it, with the pointer inside the band along its bottom edge.
    fn peek_output_at(&self, pos: LegePoint, hold: bool) -> Option<String> {
        let output = self
            .window_service
            .outputs()
            .iter()
            .find(|output| rect_contains(output.logical, pos))?;
        let covered = self.window_service.windows().any(|window| {
            window.fullscreen
                && !window.shelved
                && self.window_service.is_client_window(window.id)
                && window.geometry.intersection_area(output.logical) > 0
        });
        let reserved = output.logical.bottom() - output.usable().bottom();
        (covered && in_peek_band(output.logical, reserved, pos, hold)).then(|| output.name.clone())
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

    fn handle_press_for_focus_or_drag(
        &mut self,
        location: Point<f64, Logical>,
        serial: Serial,
        button: u32,
    ) {
        if self.session_lock.locked {
            return;
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
                    return;
                }
                Affordance::Button(ControlTag::Park) => {
                    park_window(self, window_id);
                    return;
                }
                Affordance::Button(ControlTag::Maximize) => {
                    let maximized = self
                        .window_service
                        .window(window_id)
                        .is_some_and(|window| window.maximized);
                    crate::xdg_shell::apply_maximized(self, window_id, !maximized);
                    return;
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
            return;
        }

        if target != PressTarget::Nothing {
            // A window owned the point and has already been focused above; the press itself goes
            // on to the client through `PointerHandle::button`.
            return;
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
                return;
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

/// How close to an output's bottom edge the pointer must come to *start* a peek, in logical
/// pixels.
///
/// A calibration knob rather than a constant of nature: one or two pixels is under a pixel of
/// slack once a fractional scale is applied, and a person pushing a real mouse into the bottom of
/// a real screen has to be able to land on it. Widen it if the peek is hard to reach; narrow it
/// if it triggers on its own.
const PEEK_BAND: i32 = 4;
/// How long the pointer must rest on that edge before the bar comes up. Long enough that dragging
/// something across the bottom of a fullscreen window does not raise it.
const PEEK_DWELL_MS: u64 = 150;

/// Whether `pos` is inside the band along `output`'s bottom edge that a peek watches.
///
/// `hold` widens it from the few pixels that start a peek to the whole strip the shell reserved
/// there (`reserved`, an exclusive zone) — without that, moving the pointer *onto* the bar that
/// just appeared would leave the band and dismiss it again.
fn in_peek_band(output: lege_window::Rect, reserved: i32, pos: LegePoint, hold: bool) -> bool {
    let band = if hold {
        reserved.max(PEEK_BAND)
    } else {
        PEEK_BAND
    };
    pos.y >= output.bottom() - band && pos.y < output.bottom()
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

    /// The band a peek watches: a few pixels to start one, and the shell's whole reserved strip
    /// to keep it — otherwise reaching for the bar that just appeared dismisses it again.
    #[test]
    fn the_peek_band_is_the_edge_to_open_and_the_reserved_strip_to_hold() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let reserved = 48;
        for (y, open, hold) in [
            (1031, false, false),
            (1032, false, true),
            (1040, false, true),
            (1075, false, true),
            (1076, true, true),
            (1079, true, true),
            (1080, false, false),
        ] {
            assert_eq!(
                in_peek_band(screen, reserved, at(960, y), false),
                open,
                "opening at y={y}"
            );
            assert_eq!(
                in_peek_band(screen, reserved, at(960, y), true),
                hold,
                "holding at y={y}"
            );
        }
        // An output nothing reserved on still has the few pixels that start a peek.
        assert!(in_peek_band(screen, 0, at(960, 1079), true));
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
