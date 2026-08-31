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
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
        InputBackend, InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent,
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

/// Start a recovery-band (or client-initiated `xdg_toplevel.move`) drag: `begin_drag` and install
/// a [`WindowGestureGrab`]. `pointer_global` is already logical — see the module doc.
pub fn start_drag<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
    serial: Serial,
    window_id: WindowId,
) {
    start_gesture(state, seat, serial, window_id, Gesture::Move);
}

/// Start an interactive resize dragging `edges`: `begin_resize` and install a
/// [`WindowGestureGrab`]. Reached from `xdg_toplevel.resize` and its XWayland equivalent (a
/// client-drawn resize border), and from a press on the compositor's own grip
/// (`lege_window::resize_grip`) for a window that draws no border of its own.
pub fn start_resize<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
    serial: Serial,
    window_id: WindowId,
    edges: lege_window::ResizeEdges,
) {
    start_gesture(state, seat, serial, window_id, Gesture::Resize { edges });
}

fn start_gesture<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    seat: &Seat<LegeCompState<B>>,
    serial: Serial,
    window_id: WindowId,
    gesture: Gesture,
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
    let grab = WindowGestureGrab {
        start_data,
        window_id,
        gesture,
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
                }
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
            // The fallback switcher HUD (`chrome::SwitcherState`) stays up only until something
            // else happens — any reserved action other than another cycle step clears it here;
            // `run_recovery_action` itself repopulates it for a cycle step.
            if !matches!(
                action,
                crate::keybindings::RecoveryAction::CycleFocusForward
                    | crate::keybindings::RecoveryAction::CycleFocusBackward
            ) {
                self.switcher = None;
            }
            if !self.session_lock.locked {
                self.run_recovery_action(action);
            }
        } else {
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

    /// The topmost window (by `WindowService::stacking_order`) whose geometry contains `pos`, if
    /// any — window geometry, not `Space`'s own input-region test, is the hit-test authority here,
    /// so the recovery band always takes priority over whatever the client's surface would have
    /// done with the same point (COMPOSITOR.md: "the compositor-owned recovery band" wins over
    /// client content that happens to be drawn there, e.g. a borderless window).
    fn topmost_window_at(&self, pos: LegePoint) -> Option<WindowId> {
        self.window_service.stacking_order().into_iter().find(|id| {
            self.window_service.is_visible(*id)
                && self
                    .window_service
                    .window(*id)
                    .is_some_and(|record| rect_contains(record.geometry, pos))
        })
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
        // The compositor's own two affordances, checked before the client sees the press: the
        // band moves the window, the corner grip resizes it. Both are compositor-owned screen
        // area by the same argument (`topmost_window_at`'s doc) — without them a window whose
        // client draws no frame, which is most XWayland windows, could be neither moved nor
        // resized by anyone.
        //
        // Primary button only. Both grabs end on `BTN_LEFT`'s release (`WindowGestureGrab`), so a
        // right- or middle-press that started one would install a grab nothing the user does with
        // that button could ever end.
        if button == BTN_LEFT
            && let Some(window_id) = self.topmost_window_at(pos)
        {
            if let Some(grip) = self.window_service.resize_grip(window_id)
                && rect_contains(grip, pos)
            {
                let seat = self.seat.clone();
                start_resize(
                    self,
                    &seat,
                    serial,
                    window_id,
                    lege_window::ResizeEdges::BOTTOM_RIGHT,
                );
                return;
            }
            if let Some(affordance) = self.window_service.recovery_affordance(window_id)
                && rect_contains(affordance.band, pos)
            {
                let seat = self.seat.clone();
                start_drag(self, &seat, serial, window_id);
                return;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;

    fn output(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((width, height)))
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
