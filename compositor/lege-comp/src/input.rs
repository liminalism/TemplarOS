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

use crate::state::{Backend, LegeCompState};
use lege_window::{Point as LegePoint, WindowId};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    desktop::WindowSurfaceType,
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::FilterResult,
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, Focus, GestureHoldBeginEvent,
            GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent,
            PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
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

impl<B: Backend + 'static> SeatHandler for LegeCompState<B> {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
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
    if !state.window_service.activate(window_id) {
        return;
    }
    let Some(wl_surface) = state.windows.surface_for(window_id).cloned() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.set_focus(state, Some(wl_surface.clone()), serial);
    }
    for window in state.space.elements() {
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

/// Start a recovery-band (or client-initiated `xdg_toplevel.move`) drag: `begin_drag` and install
/// [`DragGrab`]. `pointer_global` is already logical — see the module doc.
pub fn start_drag<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
    serial: Serial,
    window_id: WindowId,
) {
    let Some(pointer) = seat.get_pointer() else {
        return;
    };
    if !pointer.has_grab(serial) {
        return;
    }
    let Some(start_data) = pointer.grab_start_data() else {
        return;
    };
    let pointer_global = to_lege_point(pointer.current_location());
    if !state.window_service.begin_drag(window_id, pointer_global) {
        return;
    }
    let grab = DragGrab {
        start_data,
        window_id,
    };
    pointer.set_grab(state, grab, serial, Focus::Clear);
}

/// Linux kernel `input-event-codes.h` `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;

fn to_lege_point(point: Point<f64, Logical>) -> LegePoint {
    LegePoint {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
    }
}

/// The interactive-move grab. Every event but `motion`/`button` is forwarded to Smithay's default
/// handling unchanged — a drag has no opinion about scroll wheels or gestures — matching the shape
/// every Smithay compositor's move grab takes (`anvil`, `smallvil`, `cosmic-comp`). `motion` and
/// `button` are the two rows COMPOSITOR.md specifies: `update_drag` applied as-is, never clamped,
/// and `finish_drag` on release.
pub struct DragGrab<B: Backend + 'static> {
    start_data: PointerGrabStartData<LegeCompState<B>>,
    window_id: WindowId,
}

impl<B: Backend + 'static> PointerGrab<LegeCompState<B>> for DragGrab<B> {
    fn motion(
        &mut self,
        data: &mut LegeCompState<B>,
        handle: &mut PointerInnerHandle<'_, LegeCompState<B>>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        if let Some((_, geometry)) = data
            .window_service
            .update_drag(to_lege_point(event.location))
        {
            crate::space_sync::apply_dragged_geometry(data, self.window_id, geometry);
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
            if let Some((_, geometry)) = data.window_service.finish_drag() {
                crate::space_sync::apply_dragged_geometry(data, self.window_id, geometry);
            }
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

    fn unset(&mut self, _data: &mut LegeCompState<B>) {}
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
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard::<I>(&event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.on_pointer_motion_absolute::<I>(&event, output_name);
            }
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<I>(&event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(&event),
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: &I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(event);
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        let action = keyboard.input::<crate::keybindings::RecoveryAction, _>(
            self,
            event.key_code(),
            event.state(),
            serial,
            time,
            |_, modifiers, handle| {
                if event.state() != smithay::backend::input::KeyState::Pressed {
                    return FilterResult::Forward;
                }
                match crate::keybindings::recovery_action(*modifiers, handle.modified_sym()) {
                    Some(action) => FilterResult::Intercept(action),
                    None => FilterResult::Forward,
                }
            },
        );

        if let Some(action) = action
            && !self.session_lock.locked
        {
            self.run_recovery_action(action);
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
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: &I::PointerButtonEvent) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();

        if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
            self.handle_press_for_focus_or_drag(pointer.current_location(), serial);
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

    /// The topmost window (by `WindowService::stacking_order`) whose geometry contains `pos`, if
    /// any — window geometry, not `Space`'s own input-region test, is the hit-test authority here,
    /// so the recovery band always takes priority over whatever the client's surface would have
    /// done with the same point (COMPOSITOR.md: "the compositor-owned recovery band" wins over
    /// client content that happens to be drawn there, e.g. a borderless window).
    fn topmost_window_at(&self, pos: LegePoint) -> Option<WindowId> {
        self.window_service.stacking_order().into_iter().find(|id| {
            self.window_service
                .window(*id)
                .is_some_and(|record| rect_contains(record.geometry, pos))
        })
    }

    fn handle_press_for_focus_or_drag(&mut self, location: Point<f64, Logical>, serial: Serial) {
        if self.session_lock.locked {
            return;
        }
        let pos = to_lege_point(location);
        if let Some(window_id) = self.topmost_window_at(pos)
            && let Some(affordance) = self.window_service.recovery_affordance(window_id)
            && rect_contains(affordance.band, pos)
        {
            let seat = self.seat.clone();
            start_drag(self, &seat, serial, window_id);
            return;
        }

        if let Some((window, _)) = self
            .space
            .element_under(location)
            .map(|(w, l)| (w.clone(), l))
        {
            let Some(surface) = window.wl_surface().map(std::borrow::Cow::into_owned) else {
                return;
            };
            if let Some(window_id) = self.windows.window_id_for(&surface.id()) {
                activate_window(self, window_id);
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
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }
}

fn rect_contains(rect: lege_window::Rect, point: LegePoint) -> bool {
    point.x >= rect.x && point.x < rect.right() && point.y >= rect.y && point.y < rect.bottom()
}
