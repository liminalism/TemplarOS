//! The compositor's top-level state: one `LegeCompState<B>` per running compositor, owning every
//! Smithay protocol delegate plus the one `WindowService` that actually decides geometry, layers
//! and recoverability (`crates/lege-window`). Everything Smithay-protocol-shaped lives here or in
//! a sibling module implementing a trait on this type; `lege_window`/`lege_comp_integration` are
//! never modified to fit Smithay's shape — this crate is the adapter, not the other way round.
//!
//! Smithay 0.7.0 wires every protocol delegate through one macro,
//! [`smithay::delegate_dispatch2`], rather than the classic per-protocol `delegate_xxx!` calls:
//! any type implementing `Dispatch2`/`GlobalDispatch2` for a given Wayland interface is picked up
//! automatically, which is also how our own `lege_shell_v1`/`lege_recovery_v1` protocol objects
//! (`src/protocols_shell.rs`, `src/protocols_recovery.rs`) are wired — the same one macro call at
//! the bottom of this file covers Smithay's built-in protocols and ours alike.

use crate::window_bridge::WindowIndex;
use lege_comp_integration::{DbusIdentityCache, GovernorNotifier, ShellStateBroadcaster};
use lege_window::WindowService;
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
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
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

/// Per-`wl_client` bookkeeping. The one field the rest of this crate cares about is
/// `trusted_shell`: it is `true` for exactly the connection accepted on the private socket
/// (`src/shell_socket.rs`) the compositor itself listens on for the shell, and it is the sole
/// source of the `CommandContext::trusted_shell` flag `handle_command` and the `lege_shell_v1`
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
/// (`ClientKind::TrustedShell`, COMPOSITOR.md's `lege_shell_v1` row). Every other connection is
/// `ClientKind::Application` — there is no request-time way to become trusted, only
/// connection-time (`src/shell_socket.rs`).
pub fn client_is_trusted_shell(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|state| state.trusted_shell)
}

/// What each backend (winit, udev) supplies. Kept intentionally small: everything that is not
/// backend-specific — protocol state, `WindowService`, input policy — lives on `LegeCompState`
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
    fn set_blanked(state: &mut LegeCompState<Self>, blanked: bool)
    where
        Self: 'static,
    {
        let _ = (state, blanked);
    }
}

/// Everything the render-thread loop owns. Generic over `B` so the same state (and every handler
/// impl below) is shared verbatim between the winit and udev/KMS backends (`src/backend/`).
#[derive(Debug)]
pub struct LegeCompState<B: Backend + 'static> {
    pub backend_data: B,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, LegeCompState<B>>,
    pub loop_signal: smithay::reexports::calloop::LoopSignal,
    pub clock: Clock<Monotonic>,
    pub start_time: std::time::Instant,

    // Protocol delegate state. Each of these is created once in `LegeCompState::new` and is the
    // `&mut Self -> &mut FooState` target of exactly one `FooHandler` impl, spread across this
    // file and its siblings by concern (see the module doc).
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
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
    /// The bare-Super gesture: Super-down reveals Home immediately, holding keeps it up as a
    /// momentary surface, a short tap latches the toggle, and a chord key cancels the gesture.
    pub super_tap: crate::keybindings::SuperTapTracker,
    /// A reveal change the gesture tracker decided inside the keyboard filter closure, applied
    /// by `on_keyboard` after `keyboard.input` returns — `reveal_home` refocuses and syncs the
    /// space, which must not happen re-entrantly inside the filter.
    pub pending_super_reveal: Option<lege_window::RevealHome>,

    /// Rendering/hit-testing mirror of `window_service`, kept in sync by
    /// `src/space_sync.rs::sync_space` after every call that can change geometry or stacking —
    /// `lege_window::WindowService` remains the sole authority; nothing ever reads `Space` to make
    /// a policy decision, only to render or hit-test what policy already decided.
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub windows: WindowIndex,

    pub window_service: WindowService<DbusIdentityCache>,
    pub identity_cache: DbusIdentityCache,
    pub governor: GovernorNotifier,
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
    /// Non-`None` while the "show every window" recovery view is active: the scattered layout to
    /// draw and hit-test against instead of `window_service`'s ordinary geometry.
    pub show_all_windows: Option<Vec<(lege_window::WindowId, lege_window::Rect)>>,
    /// Non-`None` while the compositor-drawn fallback switcher (`WindowService::cycle_focus`,
    /// `src/keybindings.rs`) is showing.
    pub switcher: Option<crate::chrome::SwitcherState>,
    /// The window whose band a band drag is hovering: releasing there groups the two
    /// (`src/input.rs`, planning doc §2 rule 6). Non-`None` only during a drag, and only while
    /// the pointer is actually over another window's band — it is what that band draws itself as
    /// a drop target from.
    pub drop_target: Option<lege_window::WindowId>,
    /// The window that held the keyboard before the current one — what `Super+G` groups the
    /// focused window *with* (`src/keybindings.rs`). Tracked here rather than derived, because
    /// nothing in `WindowService` remembers a window's previous activation: activation is a flag,
    /// and the previous holder of it is gone by the time anything could ask.
    pub previous_focus: Option<lege_window::WindowId>,
    /// The compositor's own chrome renderer — the discovered font (if any) and every painted
    /// widget's cache (`src/chrome/mod.rs`). Long-lived like every other field here, never
    /// reconstructed per-frame.
    pub chrome: crate::chrome::ChromeState,
    /// Idle screen blanking: the configured delay, the armed timer, and whether the outputs are
    /// off right now (`src/idle.rs`). The compositor is the session's only idle source.
    pub idle: crate::idle::IdleState,
}

impl<B: Backend + 'static> LegeCompState<B> {
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend_data: B,
        seat_name: &str,
        dh: &DisplayHandle,
        loop_handle: LoopHandle<'static, Self>,
        loop_signal: smithay::reexports::calloop::LoopSignal,
        identity_cache: DbusIdentityCache,
        governor: GovernorNotifier,
        shell_broadcaster: ShellStateBroadcaster,
    ) -> anyhow::Result<Self> {
        use smithay::wayland::{
            compositor::CompositorState,
            fractional_scale::FractionalScaleManagerState,
            output::OutputManagerState,
            presentation::PresentationState,
            selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
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
            primary_selection_state: PrimarySelectionState::new::<Self>(dh),
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
            super_tap: crate::keybindings::SuperTapTracker::default(),
            pending_super_reveal: None,

            space: Space::default(),
            popups: PopupManager::default(),
            windows: WindowIndex::new(),

            window_service: WindowService::new(identity_cache.clone(), Vec::new()),
            identity_cache,
            governor,
            shell_broadcaster,

            shell_protocol: crate::protocols_shell::ShellProtocolState::default(),
            recovery_protocol: crate::protocols_recovery::RecoveryProtocolState::default(),
            session_lock: crate::session_lock::SessionLockState::default(),
            foreign_toplevels: crate::foreign_toplevel::ForeignToplevelState::default(),
            screencopy: crate::screencopy::ScreencopyState::new::<B>(dh),
            show_all_windows: None,
            switcher: None,
            drop_target: None,
            previous_focus: None,
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
            tracing::info!("input after an idle blank; turning the outputs back on");
            B::set_blanked(self, false);
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
                "idle timeout reached; turning the outputs off"
            );
            B::set_blanked(self, true);
        }
    }

    /// The session's screen-off delay changed (`lege_shell_v1.set_idle_timeout`, ultimately
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
            B::set_blanked(self, false);
        }
        self.arm_idle_timer();
    }

    /// Create the two private protocol globals (`lege_shell_v1`, `lege_recovery_v1`).
    ///
    /// Kept beside [`LegeCompState::new`] for the same reason: both backends must advertise both.
    pub fn create_private_globals(dh: &DisplayHandle) {
        dh.create_global::<Self, crate::protocols::lege_shell_v1::lege_shell_manager_v1::LegeShellManagerV1, _>(
            crate::protocols_shell::SHELL_PROTOCOL_VERSION,
            crate::protocols_shell::ShellManagerGlobalData,
        );
        dh.create_global::<Self, crate::protocols::lege_recovery_v1::lege_recovery_manager_v1::LegeRecoveryManagerV1, _>(
            1,
            crate::protocols_recovery::RecoveryManagerGlobalData,
        );
    }
}

impl<B: Backend + 'static> smithay::wayland::idle_inhibit::IdleInhibitHandler for LegeCompState<B> {
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

smithay::delegate_dispatch2!(@<B: Backend + 'static> LegeCompState<B>);

/// Registers the two calloop sources every backend needs regardless of how it drives rendering:
/// new client connections on the main public socket, and the `Display`'s own fd (dispatching
/// already-connected clients' requests). Returns the socket name so the caller can put it in
/// `WAYLAND_DISPLAY` for spawned clients.
pub fn init_wayland_listener<B: Backend + 'static>(
    display: smithay::reexports::wayland_server::Display<LegeCompState<B>>,
    loop_handle: &LoopHandle<'static, LegeCompState<B>>,
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
            |client_stream, (), state: &mut LegeCompState<B>| {
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
            |_, display, state: &mut LegeCompState<B>| {
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
