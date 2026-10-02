//! The compositor's top-level state: one `TemplarCompState<B>` per running compositor, owning every
//! Smithay protocol delegate plus the one `WindowService` that actually decides geometry, layers
//! and recoverability (`crates/templar-window`). Everything Smithay-protocol-shaped lives here or in
//! a sibling module implementing a trait on this type; `templar_window`/`templar_comp_integration` are
//! never modified to fit Smithay's shape — this crate is the adapter, not the other way round.
//!
//! Smithay 0.7.0 wires every protocol delegate through one macro,
//! [`smithay::delegate_dispatch2`], rather than the classic per-protocol `delegate_xxx!` calls:
//! any type implementing `Dispatch2`/`GlobalDispatch2` for a given Wayland interface is picked up
//! automatically, which is also how our own `templar_shell_v1`/`templar_recovery_v1` protocol objects
//! (`src/protocols_shell.rs`, `src/protocols_recovery.rs`) are wired — the same one macro call at
//! the bottom of this file covers Smithay's built-in protocols and ours alike.

use crate::window_bridge::WindowIndex;
use smithay::{
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    reexports::{
        calloop::LoopHandle,
        wayland_server::{
            Client, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Clock, Monotonic},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::DmabufState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        presentation::PresentationState,
        selection::{
            data_device::DataDeviceState, primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState,
        },
        session_lock::SessionLockManagerState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState, dialog::XdgDialogState},
        shm::ShmState,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xwayland_keyboard_grab::XWaylandKeyboardGrabState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};
use templar_comp_integration::{
    DbusIdentityCache, GovernorNotifier, IdleNotifier, ShellStateBroadcaster,
};
use templar_window::WindowService;

/// Per-`wl_client` bookkeeping. The one field the rest of this crate cares about is
/// `trusted_shell`: it is `true` for exactly the connection accepted on the private socket
/// (`src/shell_socket.rs`) the compositor itself listens on for the shell, and it is the sole
/// source of the `CommandContext::trusted_shell` flag `handle_command` and the `templar_shell_v1`
/// request handlers (`src/protocols_shell.rs`) both gate on — see [`client_is_trusted_shell`].
#[derive(Debug, Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub trusted_shell: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Whether `client` is the one connection the compositor itself accepted as the trusted shell
/// (`ClientKind::TrustedShell`, COMPOSITOR.md's `templar_shell_v1` row). Every other connection is
/// `ClientKind::Application` — there is no request-time way to become trusted, only
/// connection-time (`src/shell_socket.rs`).
pub fn client_is_trusted_shell(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|state| state.trusted_shell)
}

/// What each backend (winit, udev) supplies. Kept intentionally small: everything that is not
/// backend-specific — protocol state, `WindowService`, input policy — lives on `TemplarCompState`
/// itself so it is shared, not duplicated, between the two backends (COMPOSITOR.md "Backends").
pub trait Backend: Sized {
    /// Human-readable seat name advertised over `wl_seat`.
    fn seat_name(&self) -> String;

    /// Give the backend a chance to import a client buffer early (DMA-BUF direct scanout paths);
    /// a no-op is always correct, just less efficient.
    fn early_import(&mut self, _surface: &WlSurface) {}

    /// Turn every output off (idle blanking) or back on — DPMS on real hardware, a black frame
    /// nested (`src/idle.rs`).
    ///
    /// Takes the whole state rather than `&mut self` because waking is not only a DRM call: the
    /// udev backend's redraw cadence is driven by page-flip completion, and a blanked output has
    /// no flips in flight to drive the frame that would light it again, so waking has to reach
    /// the event loop as well as the device. A backend with no outputs to blank (the self-check)
    /// correctly does nothing.
    fn set_blanked(state: &mut TemplarCompState<Self>, blanked: bool)
    where
        Self: 'static,
    {
        let _ = (state, blanked);
    }

    /// Draw a frame on every output now, whether or not anything is damaged.
    ///
    /// A screen capture is answered by the next render pass (`src/screencopy.rs`), and a still
    /// desktop has no next render pass: nothing is damaged, so there is no page flip in flight to
    /// drive one. Without this, a screenshot of a screen that is not moving waits forever — which
    /// is most screenshots. The default does nothing, which is correct for a backend whose frames
    /// are driven by something other than damage.
    fn request_redraw(state: &mut TemplarCompState<Self>)
    where
        Self: 'static,
    {
        let _ = state;
    }
}

/// Everything the render-thread loop owns. Generic over `B` so the same state (and every handler
/// impl below) is shared verbatim between the winit and udev/KMS backends (`src/backend/`).
#[derive(Debug)]
pub struct TemplarCompState<B: Backend + 'static> {
    pub backend_data: B,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, TemplarCompState<B>>,
    pub loop_signal: smithay::reexports::calloop::LoopSignal,
    pub clock: Clock<Monotonic>,
    pub start_time: std::time::Instant,

    // Protocol delegate state. Each of these is created once in `TemplarCompState::new` and is the
    // `&mut Self -> &mut FooState` target of exactly one `FooHandler` impl, spread across this
    // file and its siblings by concern (see the module doc).
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    /// `zwlr_data_control_manager_v1`: clipboard managers and tools (`wl-copy`, the portal
    /// backend) reading and offering selections without owning keyboard focus. Held so the
    /// global stays registered.
    pub data_control_state: DataControlState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub presentation_state: PresentationState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_dialog_state: XdgDialogState,
    pub xdg_activation_state: XdgActivationState,
    pub session_lock_manager_state: SessionLockManagerState,
    /// `zwp_idle_inhibit_manager_v1`: a client (a video player, a presentation) declaring that the
    /// screen must not turn off while its surface is up. Held so the global stays registered;
    /// the count that matters lives in `idle`.
    pub idle_inhibit_manager_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    pub foreign_toplevel_list_state:
        smithay::wayland::foreign_toplevel_list::ForeignToplevelListState,
    pub dmabuf_state: DmabufState,
    pub xwayland_shell_state: XWaylandShellState,
    pub xwayland_keyboard_grab_state: XWaylandKeyboardGrabState,
    pub xwm: Option<X11Wm>,
    pub xwayland_display: Option<u32>,

    pub seat: Seat<Self>,
    pub seat_name: String,
    pub cursor_status: CursorImageStatus,
    /// The cursor the client owned before the pointer entered a window's resize perimeter (or
    /// before a resize gesture began), put back when it leaves. `Some` is exactly "the compositor
    /// is currently showing a cursor of its own"; while it is, `SeatHandler::cursor_image` writes
    /// here instead of over the live status, so a client setting its cursor under the perimeter
    /// is not lost and is not shown either.
    ///
    /// ponytail: one saved status for the whole seat, not one per surface. There is one pointer,
    /// so the only way to need more is a second seat — add a map keyed by seat if that ever
    /// happens.
    pub cursor_before_perimeter: Option<CursorImageStatus>,
    /// The bare-Super gesture: Super-down reveals Home immediately, holding keeps it up as a
    /// momentary surface, a short tap latches the toggle, and a chord key cancels the gesture.
    pub super_tap: crate::keybindings::SuperTapTracker,
    /// A reveal change the gesture tracker decided inside the keyboard filter closure, applied
    /// by `on_keyboard` after `keyboard.input` returns — `reveal_home` refocuses and syncs the
    /// space, which must not happen re-entrantly inside the filter.
    pub pending_super_reveal: Option<templar_window::RevealHome>,

    /// Rendering/hit-testing mirror of `window_service`, kept in sync by
    /// `src/space_sync.rs::sync_space` after every call that can change geometry or stacking —
    /// `templar_window::WindowService` remains the sole authority; nothing ever reads `Space` to make
    /// a policy decision, only to render or hit-test what policy already decided.
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub windows: WindowIndex,

    pub window_service: WindowService<DbusIdentityCache>,
    pub identity_cache: DbusIdentityCache,
    pub governor: GovernorNotifier,
    /// Tells sessiond when the screen blanks for want of input and when it lights again, so
    /// session time stops while an unlocked session sits untouched.
    pub idle_notifier: IdleNotifier,
    pub shell_broadcaster: ShellStateBroadcaster,

    /// State for the two private protocols (`src/protocols_shell.rs`, `src/protocols_recovery.rs`)
    /// and the compositor-drawn recovery UI they and the fixed keybindings
    /// (`src/keybindings.rs`) share.
    pub shell_protocol: crate::protocols_shell::ShellProtocolState,
    pub recovery_protocol: crate::protocols_recovery::RecoveryProtocolState,
    pub session_lock: crate::session_lock::SessionLockState,
    pub foreign_toplevels: crate::foreign_toplevel::ForeignToplevelState,
    /// Window previews for the Home Field over the standard capture protocols
    /// (`src/screencopy.rs`): globals, sessions, and the frames queued for the next render pass.
    pub screencopy: crate::screencopy::ScreencopyState,
    /// Where a primary-button press landed while the window overview is up, until its release
    /// decides what it was (`input::taskfield_release`): a click that chooses a card, or a drag
    /// that drops one card's window into another section's group. The mode already owns the
    /// pointer, so this one point is the whole of the gesture's state — there is no grab to
    /// install and nothing to tear down if the mode ends underneath it.
    pub taskfield_press: Option<templar_window::Point>,
    /// Non-`None` while the compositor-drawn fallback switcher (`src/keybindings.rs`) is showing.
    pub switcher: Option<crate::chrome::SwitcherState>,
    /// The list `Alt+Tab` / `Super+Tab` is walking, frozen at the first press and dropped when
    /// the modifier comes up (`src/input.rs::on_keyboard`). The flag is `true` for the group
    /// walk, so pressing the other chord rebuilds the list instead of continuing this one.
    ///
    /// Frozen because every step records activation and changes the semantic MRU order a newly
    /// built list would expose (`templar_window::SwitchRing`).
    pub switch_ring: Option<(templar_window::SwitchRing, bool)>,
    /// Whether `Alt` or `Super` was still held at the last key event — the release that ends a
    /// switcher walk. Read from the keyboard filter's modifier state rather than tracked by
    /// keysym, so a modifier released while another window has the keyboard cannot be missed.
    pub chord_held: bool,
    /// The window whose name tag a name-tag drag is hovering: releasing there groups the two
    /// (`src/input.rs`, planning doc §2 rule 6). Non-`None` only during a drag, and only while
    /// the pointer is actually over another window's name tag — it is what that tag draws itself
    /// as a drop target from.
    pub drop_target: Option<templar_window::WindowId>,
    /// The window that held the keyboard before the current one — what `Super+G` groups the
    /// focused window *with* (`src/keybindings.rs`). Tracked here rather than derived, because
    /// nothing in `WindowService` remembers a window's previous activation: activation is a flag,
    /// and the previous holder of it is gone by the time anything could ask.
    pub previous_focus: Option<templar_window::WindowId>,
    /// Whether the Home surface was on screen the last time `space_sync` looked, so a change of
    /// that answer can be logged once instead of every frame. Diagnostic only, and it exists
    /// because the black-screen-at-login fault was invisible: the units started, the journal said
    /// nothing, and the only account of what went wrong was a screen that did not change.
    pub home_mapped: Option<bool>,
    /// What the pointer is currently resting at the bottom of the screen *for*, and since when
    /// (`input::update_dwell`). Not the outcome — Home's reveal is `WindowService`'s
    /// `home_revealed` and the peek is its `peek_output`; this is only what a pending timer is
    /// about to check. One slot, because there is one pointer and one intent at a time.
    pub dwell: Option<crate::input::Dwell>,
    /// Whether a dwell timer is already in flight on the event loop. One source at a time: the
    /// callback reschedules itself (`TimeoutAction::ToDuration`) when the pointer settled again
    /// mid-flight, rather than a fresh source per motion event of a sweep.
    pub dwell_timer_pending: bool,
    /// The bottom-centre gesture gate (`src/gesture_gate.rs`): its configuration from the shell
    /// (`set_gesture_gate`) and the recognizer both pointer paths feed (`input::update_gesture_gate`).
    pub gesture_gate: crate::gesture_gate::GestureGate,
    /// Whether tapping the touchpad counts as a click, from the shell (`set_touchpad_tap`,
    /// relayed from `input.touchpad.tap-to-click`). Applied to every tracked libinput device;
    /// libinput leaves tap-to-click off unless asked, so the default is on.
    pub touchpad_tap: bool,
    /// The libinput devices seen so far (udev backend only; winit never fills this). Tracked
    /// because libinput offers no device enumeration: without this a setting change could only
    /// reach devices added afterwards.
    pub input_devices: Vec<smithay::reexports::input::Device>,
    /// The rail the gate opened, while it is open: the output's name and the side the pointer
    /// went. The peek on that output is the rail's (`input::update_dwell` leaves it alone), and
    /// every motion is reported to the shell as `rail_pointer` until it closes
    /// (`TemplarCompState::close_rail`, the one way it ends).
    pub rail: Option<(String, crate::gesture_gate::Side)>,
    /// The pointer scene the pointer's focus was last refreshed against — stacking order and
    /// client rectangles, top first. `space_sync`'s deferred refresh compares this to the scene
    /// as it is now, because snapshotting `WindowService` *after* the caller already mutated it
    /// made the motionless-pointer refresh compare a list to itself.
    pub last_pointer_scene: Vec<(templar_window::WindowId, templar_window::Rect)>,
    /// A pointer-focus refresh is queued on the event loop's idle list
    /// (`space_sync::sync_after_change`), so a burst of changes queues one.
    pub pointer_refresh_pending: bool,
    /// Set while a press on one of the compositor's own affordances is being swallowed, so its
    /// release is swallowed with it (`input::on_pointer_button`).
    ///
    /// A tag press is the compositor's, not the client's, and the two tag presses that install no
    /// grab — close, park, unmaximize, pin — would otherwise be forwarded to whatever surface has
    /// pointer focus. Under a screen-filling window's chrome that surface is the window's own
    /// client, drawn right there under the strip, so clicking "close" also clicks the terminal's
    /// last line.
    pub swallowing_press: bool,
    /// The compositor's own chrome renderer — the discovered font (if any) and every painted
    /// widget's cache (`src/chrome/mod.rs`). Long-lived like every other field here, never
    /// reconstructed per-frame.
    pub chrome: crate::chrome::ChromeState,
    /// Idle screen blanking: the configured delay, the armed timer, and whether the outputs are
    /// off right now (`src/idle.rs`). The compositor is the session's only idle source.
    pub idle: crate::idle::IdleState,
}

impl<B: Backend + 'static> TemplarCompState<B> {
    /// Build the compositor's whole state: every protocol global, the seat, and the
    /// `WindowService` that decides everything.
    ///
    /// This exists so the protocol set is declared exactly once. It used to be written out twice,
    /// identically but for the seat name, in `src/backend/winit.rs` and `src/backend/udev.rs` —
    /// which quietly made the winit backend's central claim ("every code path above this file is
    /// identical between winit and udev, so exercising it here exercises the real thing")
    /// contingent on nobody ever adding a protocol to one file and forgetting the other. A
    /// protocol advertised on hardware but not in test is precisely the bug that claim must
    /// exclude, so there is now one list and both backends read from it.
    ///
    /// What legitimately differs between backends — the renderer, the outputs, DMA-BUF, and the
    /// seat's name — stays with the backend and is either a parameter here or set up afterwards.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn new(
        backend_data: B,
        seat_name: &str,
        dh: &DisplayHandle,
        loop_handle: LoopHandle<'static, Self>,
        loop_signal: smithay::reexports::calloop::LoopSignal,
        identity_cache: DbusIdentityCache,
        governor: GovernorNotifier,
        idle_notifier: IdleNotifier,
        shell_broadcaster: ShellStateBroadcaster,
    ) -> anyhow::Result<Self> {
        use smithay::wayland::{
            compositor::CompositorState,
            fractional_scale::FractionalScaleManagerState,
            output::OutputManagerState,
            presentation::PresentationState,
            selection::{
                data_device::DataDeviceState, primary_selection::PrimarySelectionState,
                wlr_data_control::DataControlState,
            },
            session_lock::SessionLockManagerState,
            shell::xdg::{XdgShellState, decoration::XdgDecorationState, dialog::XdgDialogState},
            shm::ShmState,
            viewporter::ViewporterState,
            xdg_activation::XdgActivationState,
            xwayland_keyboard_grab::XWaylandKeyboardGrabState,
            xwayland_shell::XWaylandShellState,
        };

        let mut seat_state: SeatState<Self> = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, seat_name);
        // Not `XkbConfig::default()`: that is a bare US layout with no Compose key for anyone who
        // has not set `XKB_DEFAULT_*` by hand, and getting the wrong characters out of a text
        // field looks like a broken field, not a missing setting. See `crate::keymap`.
        let keymap = crate::keymap::XkbSource::resolve();
        tracing::info!(
            origin = keymap.origin,
            layout = %keymap.layout,
            variant = %keymap.variant,
            options = keymap.options.as_deref().unwrap_or("(default)"),
            "keyboard layout"
        );
        seat.add_keyboard(keymap.config(), 200, 25)
            .map_err(|error| anyhow::anyhow!("adding keyboard to seat: {error}"))?;
        seat.add_pointer();

        let primary_selection_state = PrimarySelectionState::new::<Self>(dh);
        // Visible to every client, like the session lock manager below: clipboard tools are
        // ordinary clients by design, and gating them would break `wl-copy` with no error.
        let data_control_state =
            DataControlState::new::<Self, _>(dh, Some(&primary_selection_state), |_| true);

        Ok(Self {
            backend_data,
            display_handle: dh.clone(),
            loop_handle,
            loop_signal,
            clock: Clock::new(),
            start_time: std::time::Instant::now(),

            compositor_state: CompositorState::new::<Self>(dh),
            shm_state: ShmState::new::<Self>(dh, Vec::new()),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            primary_selection_state,
            data_control_state,
            viewporter_state: ViewporterState::new::<Self>(dh),
            fractional_scale_manager_state: FractionalScaleManagerState::new::<Self>(dh),
            presentation_state: PresentationState::new::<Self>(dh, libc::CLOCK_MONOTONIC as u32),
            xdg_shell_state: XdgShellState::new::<Self>(dh),
            xdg_decoration_state: XdgDecorationState::new::<Self>(dh),
            xdg_dialog_state: XdgDialogState::new::<Self>(dh),
            xdg_activation_state: XdgActivationState::new::<Self>(dh),
            session_lock_manager_state: SessionLockManagerState::new::<Self, _>(dh, |_| true),
            idle_inhibit_manager_state:
                smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(dh),
            foreign_toplevel_list_state:
                smithay::wayland::foreign_toplevel_list::ForeignToplevelListState::new::<Self>(dh),
            dmabuf_state: smithay::wayland::dmabuf::DmabufState::new(),
            xwayland_shell_state: XWaylandShellState::new::<Self>(dh),
            xwayland_keyboard_grab_state: XWaylandKeyboardGrabState::new::<Self>(dh),
            xwm: None,
            xwayland_display: None,

            seat,
            seat_name: seat_name.to_owned(),
            cursor_status: CursorImageStatus::default_named(),
            cursor_before_perimeter: None,
            super_tap: crate::keybindings::SuperTapTracker::default(),
            pending_super_reveal: None,

            space: Space::default(),
            popups: PopupManager::default(),
            windows: WindowIndex::new(),

            window_service: WindowService::new(identity_cache.clone(), Vec::new()),
            identity_cache,
            governor,
            idle_notifier,
            shell_broadcaster,

            shell_protocol: crate::protocols_shell::ShellProtocolState::default(),
            recovery_protocol: crate::protocols_recovery::RecoveryProtocolState::default(),
            session_lock: crate::session_lock::SessionLockState::default(),
            foreign_toplevels: crate::foreign_toplevel::ForeignToplevelState::default(),
            screencopy: crate::screencopy::ScreencopyState::new::<B>(dh),
            taskfield_press: None,
            switcher: None,
            switch_ring: None,
            chord_held: false,
            drop_target: None,
            previous_focus: None,
            home_mapped: None,
            dwell: None,
            dwell_timer_pending: false,
            gesture_gate: crate::gesture_gate::GestureGate::default(),
            touchpad_tap: true,
            input_devices: Vec::new(),
            rail: None,
            last_pointer_scene: Vec::new(),
            pointer_refresh_pending: false,
            swallowing_press: false,
            chrome: crate::chrome::ChromeState::new(),
            idle: crate::idle::IdleState::default(),
        })
    }

    /// Something happened on the seat. Wake a blanked screen and restart the idle countdown.
    ///
    /// Called from one place — `src/input.rs::process_input_event`, before the event is
    /// dispatched anywhere — because "any input at all" is the whole condition, and a per-event
    /// list of what counts would be a list to forget something from.
    pub fn note_activity(&mut self) {
        if self.idle.wake() {
            tracing::info!(cause = "input", "outputs woken from idle blank");
            B::set_blanked(self, false);
            self.idle_notifier.set_idle(false);
        }
        self.arm_idle_timer();
    }

    /// (Re-)arm the idle countdown from now, replacing whatever was armed before. Arms nothing
    /// when idle blanking is off, when something is inhibiting it, or when the screen is already
    /// off (`idle::delay_before_blanking`).
    pub fn arm_idle_timer(&mut self) {
        if let Some(token) = self.idle.take_timer() {
            self.loop_handle.remove(token);
        }
        let Some(timer) = self.idle.timer_source() else {
            return;
        };
        match self
            .loop_handle
            .insert_source(timer, |_, (), state: &mut Self| {
                state.blank_now();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            }) {
            Ok(token) => self.idle.set_timer(token),
            Err(error) => tracing::warn!(%error, "arming the idle timer"),
        }
    }

    /// The idle timer fired: turn the outputs off.
    fn blank_now(&mut self) {
        let _ = self.idle.take_timer();
        if self.idle.expire() {
            tracing::info!(
                timeout_s = self.idle.timeout().map_or(0, |d| d.as_secs()),
                "outputs blanked after idle timeout"
            );
            B::set_blanked(self, true);
            self.idle_notifier.set_idle(true);
        }
    }

    /// The session's screen-off delay changed (`templar_shell_v1.set_idle_timeout`, ultimately
    /// `Power1`'s `screen_off_after`). Zero is never. A screen that is currently off comes back
    /// on, so a person who has just turned blanking off is not left looking at a dark screen.
    pub fn set_idle_timeout(&mut self, seconds: u32) {
        let timeout = crate::idle::timeout_from_seconds(seconds);
        if self.idle.timeout() == timeout {
            return;
        }
        let _ = self.idle.set_timeout(timeout);
        tracing::info!(seconds, "idle screen-off delay set");
        if timeout.is_none() && self.idle.wake() {
            tracing::info!(
                cause = "idle blanking turned off",
                "outputs woken from idle blank"
            );
            B::set_blanked(self, false);
            self.idle_notifier.set_idle(false);
        }
        self.arm_idle_timer();
    }

    /// Tap-to-click changed (`templar_shell_v1.set_touchpad_tap`, ultimately
    /// `input.touchpad.tap-to-click`). Applied to every tracked device now; devices added
    /// later pick it up in `track_input_device`.
    pub fn set_touchpad_tap(&mut self, enabled: bool) {
        self.touchpad_tap = enabled;
        for device in &mut self.input_devices {
            Self::apply_tap_to_device(device, enabled);
        }
    }

    /// Remember a newly seen libinput device and bring it under the current tap setting.
    /// Called from the udev backend's device-added path; the winit backend has no libinput
    /// devices and never calls this.
    pub fn track_input_device(&mut self, device: smithay::reexports::input::Device) {
        let mut device = device;
        Self::apply_tap_to_device(&mut device, self.touchpad_tap);
        if !self
            .input_devices
            .iter()
            .any(|known| known.sysname() == device.sysname())
        {
            self.input_devices.push(device);
        }
    }

    /// Forget a removed libinput device, by kernel name.
    pub fn untrack_input_device(&mut self, sysname: &str) {
        self.input_devices
            .retain(|known| known.sysname() != sysname);
    }

    /// One device's tap configuration. Devices without tap support (`config_tap_finger_count`
    /// 0 — keyboards, mice, anything that is not a touchpad) are left alone; a refusal from
    /// libinput is logged, never fatal.
    fn apply_tap_to_device(device: &mut smithay::reexports::input::Device, enabled: bool) {
        if device.config_tap_finger_count() == 0 {
            return;
        }
        if let Err(error) = device.config_tap_set_enabled(enabled) {
            tracing::debug!(
                device = device.sysname(),
                ?error,
                "libinput refused tap-to-click configuration"
            );
        }
    }

    /// Create the two private protocol globals (`templar_shell_v1`, `templar_recovery_v1`).
    ///
    /// Kept beside [`TemplarCompState::new`] for the same reason: both backends must advertise both.
    pub fn create_private_globals(dh: &DisplayHandle) {
        dh.create_global::<Self, crate::protocols::templar_shell_v1::templar_shell_manager_v1::TemplarShellManagerV1, _>(
            crate::protocols_shell::SHELL_PROTOCOL_VERSION,
            crate::protocols_shell::ShellManagerGlobalData,
        );
        dh.create_global::<Self, crate::protocols::templar_recovery_v1::templar_recovery_manager_v1::TemplarRecoveryManagerV1, _>(
            1,
            crate::protocols_recovery::RecoveryManagerGlobalData,
        );
    }
}

impl<B: Backend + 'static> smithay::wayland::idle_inhibit::IdleInhibitHandler
    for TemplarCompState<B>
{
    fn inhibit(&mut self, _surface: WlSurface) {
        self.idle.add_inhibitor();
        // Re-arm rather than merely counting: with an inhibitor up there is no timer at all
        // (`idle::delay_before_blanking`), so the one already armed has to come back out of the
        // event loop.
        self.arm_idle_timer();
    }

    fn uninhibit(&mut self, _surface: WlSurface) {
        self.idle.remove_inhibitor();
        self.arm_idle_timer();
    }
}

smithay::delegate_dispatch2!(@<B: Backend + 'static> TemplarCompState<B>);

/// Registers the two calloop sources every backend needs regardless of how it drives rendering:
/// new client connections on the main public socket, and the `Display`'s own fd (dispatching
/// already-connected clients' requests). Returns the socket name so the caller can put it in
/// `WAYLAND_DISPLAY` for spawned clients.
pub fn init_wayland_listener<B: Backend + 'static>(
    display: smithay::reexports::wayland_server::Display<TemplarCompState<B>>,
    loop_handle: &LoopHandle<'static, TemplarCompState<B>>,
    requested_name: Option<&str>,
) -> anyhow::Result<String> {
    use smithay::reexports::calloop::{Interest, Mode as CalloopMode, generic::Generic};
    use smithay::wayland::socket::ListeningSocketSource;

    let socket_source = match requested_name {
        Some(name) => ListeningSocketSource::with_name(name)?,
        None => ListeningSocketSource::new_auto()?,
    };
    let socket_name = socket_source.socket_name().to_string_lossy().into_owned();

    loop_handle
        .insert_source(
            socket_source,
            |client_stream, (), state: &mut TemplarCompState<B>| {
                if let Err(error) = state
                    .display_handle
                    .insert_client(client_stream, std::sync::Arc::new(ClientState::default()))
                {
                    tracing::warn!(%error, "failed to add wayland client");
                }
            },
        )
        .map_err(|error| anyhow::anyhow!("inserting wayland socket source: {error}"))?;

    // `display` is moved into the `Generic` source below and lives there for the rest of the
    // compositor's lifetime, so `NoIoDrop::get_mut`'s "must not be dropped while borrowed"
    // contract holds by construction — matching every Smithay-based compositor's identical
    // pattern (`anvil`, `smallvil`).
    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, CalloopMode::Level),
            |_, display, state: &mut TemplarCompState<B>| {
                // SAFETY: the wrapped `Display` is not dropped while this callback can run — it
                // is owned by this same calloop source for the compositor's whole lifetime. There
                // is no safe alternative: `NoIoDrop::get_mut` is `unsafe` by Smithay's own design
                // (every Smithay-based compositor's identical wayland-display source takes this
                // same call), because calloop's `Generic` wrapper cannot itself prove the I/O
                // object it owns is still alive at this point — only the caller's control-flow
                // guarantee (never dropped early, established above) can.
                #[allow(unsafe_code)]
                unsafe {
                    display.get_mut().dispatch_clients(state)?;
                }
                Ok(smithay::reexports::calloop::PostAction::Continue)
            },
        )
        .map_err(|error| anyhow::anyhow!("inserting wayland display source: {error}"))?;

    Ok(socket_name)
}
