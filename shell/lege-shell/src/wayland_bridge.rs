//! Reaching the compositor's private `lege_shell_v1` protocol from a window
//! `pixelkit_shell`/winit created — the piece M2's design doc calls out by name: "work out the
//! cleanest way for a winit-based client to also speak our protocol on that connection".
//!
//! # Why this needs `unsafe` at all
//!
//! `lege_shell_manager_v1.set_layer_surface`/`set_exclusive_zone` take a `wl_surface` argument.
//! Wayland objects are scoped to one client *connection* — an object created on one `wl_display`
//! connection cannot be named from a different one, even to the same socket, because the
//! compositor treats each accepted stream as a distinct client with its own object-id namespace
//! (`compositor/lege-comp/src/shell_socket.rs`). So the request has to be sent on **the same
//! connection that owns the surface** — which means on winit's own connection, since winit is
//! what created the `xdg_toplevel`/`wl_surface` in the first place.
//!
//! Winit never exposes that connection as a typed `wayland_client::Connection` (only as a raw
//! pointer, through `raw_window_handle`) — see this crate's `surface/window.rs` module doc for why
//! that also rules out `pixelkit_shell::run_app`. So this module rebuilds a `Connection` around
//! the same underlying `wl_display*` winit already opened, via
//! `wayland_backend::client::Backend::from_foreign_display` — precisely the entry point
//! `wayland-backend`'s own docs name for "writing a library that is expected to plug itself into
//! an existing Wayland connection", and the same mechanism EGL/Vulkan interop libraries use to
//! share a `wl_display` with a toolkit that opened it. It opens a **second, independent event
//! queue** on that shared display (the standard multi-consumer pattern libwayland-client's queue
//! system exists for): reading bytes off the socket is coordinated inside libwayland-client itself
//! for every queue on a display, so whichever party reads first — in practice winit's own event
//! loop, since it polls the fd continuously — distributes incoming messages to the right queue
//! regardless of who created it. This module's queue is pumped once per frame
//! ([`ShellBridge::pump`]) from [`crate::surface::app`]'s tick, which is enough: nothing here
//! needs sub-frame latency.
//!
//! Reconstructing the `wl_surface` itself needs one more unsafe call,
//! `wayland_backend::client::ObjectId::from_ptr`, which validates the raw pointer's interface name
//! against libwayland's own C bookkeeping before accepting it — the "sys"/dlopen backend
//! (`wayland-backend`'s `client_system` feature, which this crate enables to match winit's own
//! choice; see `Cargo.toml`) is what makes both `from_ptr` and `from_foreign_display` available at
//! all, since neither exists on `wayland-backend`'s pure-Rust reimplementation. Both calls are
//! read-only with respect to the object graph: `wayland_client::Proxy::from_id` (used to turn the
//! wrapped id into a typed `WlSurface`) only *reads* already-attached object data
//! (`conn.get_object_data`/`conn.object_info`) rather than registering anything new, so wrapping
//! winit's surface never fights winit for ownership of it.
//!
//! # Safety obligations this module actually has to uphold
//!
//! [`ShellBridge::connect`] is the only unsafe code. It requires:
//! - `window` is a live Wayland window (`raw_window_handle` reports a `Wayland` variant), and
//! - the `wl_display`/`wl_surface` pointers it hands out stay valid for as long as `window` does.
//!
//! Both hold by construction: winit owns the underlying connection for the lifetime of the
//! process (it is never closed early), and the surface pointer is only read while `window` is
//! still alive on the caller's stack/heap. `Backend::from_foreign_display` explicitly does not
//! take ownership ("guest mode" — it will not close the connection when dropped), so a
//! [`ShellBridge`] outliving `window` cannot double-close anything; it can only end up issuing
//! requests against a connection nothing is reading any more, which surfaces as an ordinary
//! `Err` the next time [`ShellBridge::pump`] is called, not as unsafety.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend as WBackend, ObjectData, ObjectId};
use wayland_client::protocol::{
    wl_registry::{self, WlRegistry},
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

use lege_types::{AppId, InstanceId};
use lege_window::{Layer, Rect, RevealHome, ShellToplevel, WindowId, geometry::Edge};

/// The highest `lege_shell_v1` version this shell speaks (`interfaces/wayland/lege-shell-v1.xml`).
/// Bound at the lower of this and what the compositor advertises, so a newer shell against an
/// older compositor still gets version 1's events rather than a protocol error.
const SHELL_PROTOCOL_VERSION: u32 = 2;

use crate::protocol::lege_shell_v1::{
    lege_shell_manager_v1::{self, LegeShellManagerV1},
    lege_shell_toplevel_list_v1::{self, LegeShellToplevelListV1},
    lege_shell_toplevel_v1::{self, LegeShellToplevelV1},
};

/// One toplevel as tracked from the wire, plus the object that lets the shell act on it.
///
/// The wire protocol never sends `lege_window::WindowId` — `lege_shell_toplevel_v1` objects carry
/// no such field (`interfaces/wayland/lege-shell-v1.xml`); the object's own identity on this
/// connection *is* the window's identity here. [`WindowId`] is reused anyway, rather than
/// inventing a parallel type, so the already-tested [`lege_window::grouping_key`],
/// [`crate::dock::group`] and [`crate::drawers::window_shelf`] keep working completely unmodified
/// against this data: its value is synthetic, [`ObjectId::protocol_id`] of this object, stable for
/// exactly as long as the object exists (see [`synthetic_window_id`]).
#[derive(Clone)]
struct TrackedToplevel {
    proxy: LegeShellToplevelV1,
    canonical_app_id: Option<AppId>,
    instance_id: Option<InstanceId>,
    title: String,
    verified_identity: bool,
    activated: bool,
    fullscreen: bool,
    shelved: bool,
    maximized: bool,
    geometry: Rect,
    /// `ext_foreign_toplevel_handle_v1.identifier`, relayed once by the compositor (version 2).
    identifier: Option<String>,
    layer: Layer,
}

impl TrackedToplevel {
    fn new(proxy: LegeShellToplevelV1) -> Self {
        TrackedToplevel {
            proxy,
            canonical_app_id: None,
            instance_id: None,
            title: String::new(),
            verified_identity: false,
            activated: false,
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: Rect::new(0, 0, 0, 0),
            identifier: None,
            // The `layer` event has not necessarily arrived yet for a window this connection just
            // learned about (see the module doc on `GetToplevelList` below); `Normal` is the
            // least surprising guess and self-corrects the moment any state changes.
            layer: Layer::Normal,
        }
    }

    fn snapshot(&self, window_id: WindowId) -> ShellToplevel {
        ShellToplevel {
            window_id,
            canonical_app_id: self.canonical_app_id.clone(),
            instance_id: self.instance_id,
            title: self.title.clone(),
            verified_identity: self.verified_identity,
            activated: self.activated,
            fullscreen: self.fullscreen,
            shelved: self.shelved,
            maximized: self.maximized,
            geometry: self.geometry,
            layer: self.layer,
        }
    }
}

/// The synthetic [`WindowId`] standing in for the server's real one — see [`TrackedToplevel`]'s
/// doc. `ObjectId`s are never reused within one connection's lifetime, so this is stable for as
/// long as the object is.
fn synthetic_window_id(id: &ObjectId) -> WindowId {
    WindowId(u64::from(id.protocol_id()))
}

/// Dispatch target for the trusted-shell connection's own event queue. Holds nothing but what
/// arrives over the wire — no D-Bus, no rendering state — on purpose: everything this crate does
/// with the result lives in [`ShellBridge`] instead, which is what stays free of `unsafe`.
#[derive(Default)]
pub struct BridgeState {
    manager: Option<LegeShellManagerV1>,
    toplevels: HashMap<ObjectId, TrackedToplevel>,
    /// `lege_shell_manager_v1.home_revealed`'s last word, whichever side started the reveal.
    home_revealed: bool,
    /// Window previews over the standard capture protocols (`crate::previews`).
    pub(crate) previews: crate::previews::PreviewState,
}

impl Dispatch<WlRegistry, ()> for BridgeState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "lege_shell_manager_v1" {
                let version = version.min(SHELL_PROTOCOL_VERSION);
                state.manager =
                    Some(registry.bind::<LegeShellManagerV1, _, _>(name, version, qh, ()));
            } else {
                state
                    .previews
                    .bind_global(registry, name, &interface, version, qh);
            }
        }
    }
}

impl Dispatch<LegeShellManagerV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &LegeShellManagerV1,
        event: lege_shell_manager_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let lege_shell_manager_v1::Event::HomeRevealed { revealed } = event;
        state.home_revealed = revealed != 0;
    }
}

impl Dispatch<LegeShellToplevelListV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &LegeShellToplevelListV1,
        event: lege_shell_toplevel_list_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let lege_shell_toplevel_list_v1::Event::Toplevel { id } = event;
        state.toplevels.insert(id.id(), TrackedToplevel::new(id));
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        // `toplevel` is this interface's only event, so it is unconditionally opcode 0
        // (`interfaces/wayland/lege-shell-v1.xml`'s `lege_shell_toplevel_list_v1`).
        match opcode {
            0 => qh.make_data::<LegeShellToplevelV1, ()>(()),
            other => panic!("lege_shell_toplevel_list_v1: unexpected event opcode {other}"),
        }
    }
}

impl Dispatch<LegeShellToplevelV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &LegeShellToplevelV1,
        event: lege_shell_toplevel_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Handled before looking the object up: once closed, there is nothing left to update,
        // and holding a `get_mut` borrow across this arm would fight the removal below it.
        if matches!(event, lege_shell_toplevel_v1::Event::Closed) {
            state.toplevels.remove(&proxy.id());
            return;
        }
        let Some(tracked) = state.toplevels.get_mut(&proxy.id()) else {
            return;
        };
        match event {
            lege_shell_toplevel_v1::Event::Identity {
                canonical_app_id,
                instance_id,
                verified,
            } => {
                tracked.canonical_app_id =
                    canonical_app_id.as_deref().and_then(|id| id.parse().ok());
                tracked.instance_id = instance_id.as_deref().and_then(InstanceId::parse);
                tracked.verified_identity = verified != 0;
            }
            lege_shell_toplevel_v1::Event::Title { title } => tracked.title = title,
            lege_shell_toplevel_v1::Event::State {
                activated,
                fullscreen,
                shelved,
                shelf_rank: _,
            } => {
                // `shelf_rank` is the wire's authoritative shelve order (see
                // `interfaces/wayland/lege-shell-v1.xml`); `crate::drawers::window_shelf`
                // currently sorts by `window_id` instead (its own module doc explains why: the
                // sanitized `ShellToplevel` this crate renders against has no such field yet).
                // Wiring it through is the natural follow-up once that struct grows one — not a
                // silent loss, since the events themselves carry it either way.
                tracked.activated = activated != 0;
                tracked.fullscreen = fullscreen != 0;
                tracked.shelved = shelved != 0;
            }
            lege_shell_toplevel_v1::Event::Layer { layer } => {
                if let WEnum::Value(layer) = layer {
                    tracked.layer = layer_from_wire(layer);
                }
            }
            lege_shell_toplevel_v1::Event::Identifier { identifier } => {
                tracked.identifier = Some(identifier);
            }
            lege_shell_toplevel_v1::Event::Geometry {
                x,
                y,
                width,
                height,
            } => {
                tracked.geometry = Rect::new(x, y, width, height);
            }
            lege_shell_toplevel_v1::Event::Maximized { maximized } => {
                tracked.maximized = maximized != 0;
            }
            lege_shell_toplevel_v1::Event::Closed => unreachable!("handled above"),
        }
    }
}

impl std::fmt::Debug for BridgeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeState")
            .field("toplevels", &self.toplevels.len())
            .field("home_revealed", &self.home_revealed)
            .field("previews", &self.previews)
            .finish_non_exhaustive()
    }
}

/// `lege_window::Layer` <-> the wire enum. Mirrors
/// `compositor/lege-comp/src/protocols_shell.rs::wire_layer` exactly, on the client's own copy of
/// the generated enum — the two are separate Rust types (client- vs server-generated bindings)
/// even though the wire encoding is one contract, so each side keeps its own explicit match rather
/// than share a conversion function across a client/server boundary that cannot itself share code.
fn wire_layer(layer: Layer) -> lege_shell_manager_v1::Layer {
    match layer {
        Layer::SecureLock => lege_shell_manager_v1::Layer::SecureLock,
        Layer::Recovery => lege_shell_manager_v1::Layer::Recovery,
        Layer::SystemModal => lege_shell_manager_v1::Layer::SystemModal,
        Layer::ShellOverlay => lege_shell_manager_v1::Layer::ShellOverlay,
        Layer::Notification => lege_shell_manager_v1::Layer::Notification,
        Layer::AppOnTop => lege_shell_manager_v1::Layer::AppOnTop,
        Layer::Normal => lege_shell_manager_v1::Layer::Normal,
        Layer::HomeSurface => lege_shell_manager_v1::Layer::HomeSurface,
    }
}

fn layer_from_wire(wire: lege_shell_manager_v1::Layer) -> Layer {
    match wire {
        lege_shell_manager_v1::Layer::SecureLock => Layer::SecureLock,
        lege_shell_manager_v1::Layer::Recovery => Layer::Recovery,
        lege_shell_manager_v1::Layer::SystemModal => Layer::SystemModal,
        lege_shell_manager_v1::Layer::ShellOverlay => Layer::ShellOverlay,
        lege_shell_manager_v1::Layer::Notification => Layer::Notification,
        lege_shell_manager_v1::Layer::AppOnTop => Layer::AppOnTop,
        lege_shell_manager_v1::Layer::Normal => Layer::Normal,
        lege_shell_manager_v1::Layer::HomeSurface => Layer::HomeSurface,
    }
}

fn wire_reveal(reveal: RevealHome) -> lege_shell_manager_v1::Reveal {
    match reveal {
        RevealHome::Show => lege_shell_manager_v1::Reveal::Show,
        RevealHome::Hide => lege_shell_manager_v1::Reveal::Hide,
        RevealHome::Toggle => lege_shell_manager_v1::Reveal::Toggle,
    }
}

fn wire_edge(edge: Edge) -> lege_shell_manager_v1::Edge {
    match edge {
        Edge::Top => lege_shell_manager_v1::Edge::Top,
        Edge::Bottom => lege_shell_manager_v1::Edge::Bottom,
        Edge::Left => lege_shell_manager_v1::Edge::Left,
        Edge::Right => lege_shell_manager_v1::Edge::Right,
    }
}

/// A live connection to the compositor's trusted-shell socket, speaking `lege_shell_v1` on the
/// exact same connection winit opened for a given window. See the module doc for the whole design
/// and its safety argument.
pub struct ShellBridge {
    connection: Connection,
    queue: EventQueue<BridgeState>,
    manager: LegeShellManagerV1,
    state: BridgeState,
}

impl std::fmt::Debug for ShellBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellBridge")
            .field("toplevels", &self.state.toplevels.len())
            .finish_non_exhaustive()
    }
}

impl ShellBridge {
    /// Reach `lege_shell_v1` on the same Wayland connection `window` was created on.
    ///
    /// Fails cleanly (never panics) when: the window is not on Wayland at all, the process is not
    /// connected to the compositor's trusted-shell socket (so `lege_shell_manager_v1` was never
    /// advertised), or the initial registry round trip cannot complete. All three are ordinary,
    /// expected conditions — running nested for development without the trusted socket wired up,
    /// say — and the caller is expected to keep running with no protocol surface rather than
    /// treat this as fatal, the same way every drawer degrades when its D-Bus service is absent.
    pub fn connect(window: &winit::window::Window) -> Result<ShellBridge> {
        let display_handle = window
            .display_handle()
            .context("reading the window's display handle")?;
        let RawDisplayHandle::Wayland(wayland_display) = display_handle.as_raw() else {
            bail!("lege_shell_v1 is a Wayland-only protocol, and this window is not on Wayland");
        };

        // SAFETY: `wayland_display.display` is winit's own `wl_display*`, guaranteed non-null and
        // valid by `raw_window_handle`'s contract for as long as the window it came from is alive;
        // `window` outlives this call. `from_foreign_display` does not take ownership (see the
        // module doc's safety section), so this cannot double-free winit's connection.
        let backend =
            unsafe { WBackend::from_foreign_display(wayland_display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let mut queue: EventQueue<BridgeState> = connection.new_event_queue();
        let qh = queue.handle();

        let display = connection.display();
        let _registry = display.get_registry(&qh, ());
        let mut state = BridgeState::default();
        queue
            .roundtrip(&mut state)
            .context("initial registry round trip on the trusted-shell connection")?;

        let manager = state.manager.take().ok_or_else(|| {
            anyhow!(
                "lege_shell_manager_v1 was not advertised on this connection — this process is \
                 probably not connected to the compositor's trusted-shell socket \
                 (WAYLAND_DISPLAY should name the `*-shell` socket logged by lege-comp, not the \
                 ordinary one)"
            )
        })?;

        let _list = manager.get_toplevel_list(&qh, ());
        // One more round trip so the toplevel objects that already existed are in hand before the
        // first frame draws. Their identity/title/state/layer can still be default-empty
        // afterwards — `GetToplevelList`'s handler creates the objects but does not itself force
        // a `sync_toplevels` (`compositor/lege-comp/src/protocols_shell.rs`), so a window that was
        // already mapped and has not changed since arrives blank until it does. Cosmetic, not a
        // correctness problem: the object and its later events are still exactly right.
        queue
            .roundtrip(&mut state)
            .context("initial toplevel-list round trip")?;

        Ok(ShellBridge {
            connection,
            queue,
            manager,
            state,
        })
    }

    /// Declare `window`'s surface into `layer`. `window` must be the same window [`connect`] was
    /// called with (or another window on the same connection) — see the module doc for why a
    /// surface from a different connection could never work here regardless.
    ///
    /// [`connect`]: Self::connect
    pub fn set_layer(&self, window: &winit::window::Window, layer: Layer) -> Result<()> {
        let surface = self.wrap_surface(window)?;
        self.manager.set_layer_surface(&surface, wire_layer(layer));
        self.connection
            .flush()
            .context("flushing set_layer_surface")
    }

    /// Reserve `size` logical pixels along `edge` of the output `window` is on, so ordinary
    /// windows are placed beside it. `size` of `0` releases a previous reservation
    /// (`interfaces/wayland/lege-shell-v1.xml`'s `set_exclusive_zone` doc).
    pub fn set_exclusive_zone(
        &self,
        window: &winit::window::Window,
        edge: Edge,
        size: u32,
    ) -> Result<()> {
        let surface = self.wrap_surface(window)?;
        self.manager
            .set_exclusive_zone(&surface, wire_edge(edge), size);
        self.connection
            .flush()
            .context("flushing set_exclusive_zone")
    }

    /// Process whatever has already arrived on this connection's own event queue, without
    /// blocking. Call once per frame/tick; see the module doc for why that cadence is enough.
    pub fn pump(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatching the trusted-shell connection's event queue")?;
        Ok(())
    }

    /// Every toplevel this connection currently knows about, for [`crate::dock`] and
    /// [`crate::drawers::window_shelf`] to consume exactly as they already do against fixtures.
    pub fn toplevels(&self) -> Vec<ShellToplevel> {
        self.state
            .toplevels
            .iter()
            .map(|(id, tracked)| tracked.snapshot(synthetic_window_id(id)))
            .collect()
    }

    fn toplevel_object(&self, window_id: WindowId) -> Option<&LegeShellToplevelV1> {
        self.state
            .toplevels
            .iter()
            .find(|(id, _)| synthetic_window_id(id) == window_id)
            .map(|(_, tracked)| &tracked.proxy)
    }

    /// Ask the compositor to activate (raise and focus) one window — a dock click, or "restore"
    /// from the Window Shelf.
    pub fn activate(&self, window_id: WindowId) -> Result<()> {
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        // The compositor does not cryptographically verify this token for a trusted-shell-issued
        // activation (`protocols_shell.rs`'s `Activate` handler always grants
        // `had_recent_input_evidence: true` for whatever string arrives here); it exists on the
        // wire for the ordinary application-to-application activation handoff, which this is not.
        toplevel.activate("lege-shell-ui".to_owned());
        self.connection.flush().context("flushing activate")
    }

    pub fn close(&self, window_id: WindowId) -> Result<()> {
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.close();
        self.connection.flush().context("flushing close")
    }

    pub fn unshelve(&self, window_id: WindowId) -> Result<()> {
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.unshelve();
        self.connection.flush().context("flushing unshelve")
    }

    /// Park one window: explicit minimisation onto the shelf, the Home Field's "collapse".
    pub fn shelve(&self, window_id: WindowId) -> Result<()> {
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.shelve();
        self.connection.flush().context("flushing shelve")
    }

    /// `xdg_toplevel.set_maximized`'s effect, requested on the window's behalf (version 2).
    pub fn set_maximized(&self, window_id: WindowId, maximized: bool) -> Result<()> {
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        if toplevel.version() < 2 {
            bail!("the compositor speaks lege_shell_v1 version 1, which has no set_maximized");
        }
        toplevel.set_maximized(u32::from(maximized));
        self.connection.flush().context("flushing set_maximized")
    }

    /// The compositor's "show every window" recovery view, from the shell's own UI rather than
    /// from `Ctrl+Alt+Space` (`window.show-all` in the action manifest). The compositor computes
    /// the scatter itself and does not consult this connection for it — the request exists
    /// precisely so the chord is not the only way in.
    pub fn show_all_windows(&self) -> Result<()> {
        self.manager.show_all_windows();
        self.connection.flush().context("flushing show_all_windows")
    }

    /// `window.escape-fullscreen` by pointer. `window_id` of `None` targets whichever window
    /// currently holds activation, exactly as `Ctrl+Alt+Escape` does.
    pub fn escape_fullscreen_or_modal(&self, window_id: Option<WindowId>) -> Result<()> {
        let toplevel = match window_id {
            None => None,
            Some(window_id) => match self.toplevel_object(window_id) {
                Some(toplevel) => Some(toplevel.clone()),
                None => bail!("no such window on this connection: {window_id:?}"),
            },
        };
        self.manager.escape_fullscreen_or_modal(toplevel.as_ref());
        self.connection
            .flush()
            .context("flushing escape_fullscreen_or_modal")
    }

    /// "Go home" (version 2): the field's HOME button reaches the same reveal as a bare Super
    /// tap and the compositor's Ctrl+Alt+H / Super+D fallbacks.
    pub fn reveal_home(&self, reveal: RevealHome) -> Result<()> {
        if self.manager.version() < 2 {
            bail!("the compositor speaks lege_shell_v1 version 1, which has no reveal_home");
        }
        self.manager.reveal_home(wire_reveal(reveal));
        self.connection.flush().context("flushing reveal_home")
    }

    /// Whether a "go home" reveal is in effect, as last reported by the compositor.
    pub fn home_revealed(&self) -> bool {
        self.state.home_revealed
    }

    /// Ask for one preview frame of the window with foreign-toplevel `identifier`
    /// (`crate::previews`). Pacing is the caller's; a request while one is in flight is dropped.
    pub fn request_preview(&mut self, identifier: &str) -> Result<()> {
        let qh = self.queue.handle();
        self.state.previews.request(identifier, &qh)?;
        self.connection
            .flush()
            .context("flushing a preview request")
    }

    /// Previews that arrived since the last call, keyed by foreign-toplevel identifier.
    pub fn take_previews(&mut self) -> Vec<(String, pixelkit_raster::Bitmap)> {
        self.state.previews.take_ready()
    }

    pub fn has_previews_ready(&self) -> bool {
        self.state.previews.has_ready()
    }

    /// Whether the compositor offered the capture globals on this connection.
    pub fn previews_available(&self) -> bool {
        self.state.previews.available()
    }

    /// The foreign-toplevel identifier for `window_id`, once the compositor has relayed it —
    /// the key a preview of this window is filed under.
    pub fn identifier_of(&self, window_id: WindowId) -> Option<&str> {
        self.state
            .toplevels
            .iter()
            .find(|(id, _)| synthetic_window_id(id) == window_id)
            .and_then(|(_, tracked)| tracked.identifier.as_deref())
    }

    fn wrap_surface(&self, window: &winit::window::Window) -> Result<WlSurface> {
        let window_handle = window
            .window_handle()
            .context("reading the window's window handle")?;
        let RawWindowHandle::Wayland(handle) = window_handle.as_raw() else {
            bail!("lege_shell_v1 is a Wayland-only protocol, and this window is not on Wayland");
        };
        // SAFETY: `handle.surface` is winit's own `wl_surface*` for `window`, valid for as long as
        // `window` is alive (the borrow of `window` above ties this call's lifetime to it).
        // `ObjectId::from_ptr` only reads the pointer's already-established libwayland bookkeeping
        // (interface name, liveness flag); it registers nothing new (see the module doc).
        let object_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }
                .context("wrapping the window's wl_surface for lege_shell_v1")?;
        WlSurface::from_id(&self.connection, object_id)
            .context("constructing a WlSurface proxy from the wrapped object id")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_layer_round_trips_through_the_client_wire_encoding() {
        for layer in Layer::ALL {
            assert_eq!(layer_from_wire(wire_layer(layer)), layer);
        }
    }

    // `synthetic_window_id`, `wrap_surface`, `connect` and every `ShellBridge` method beyond this
    // need a live Wayland connection to exercise — there is no fixture for "an `ObjectId` handed
    // out by a real libwayland connection" the way `dock.rs`/`window_shelf.rs` fixture
    // `ShellToplevel`s directly. That is why this module keeps the wire-encoding arithmetic
    // (testable, and tested above) strictly separate from the connection plumbing around it
    // (exercised only by actually running the shell against a compositor — see the crate's top
    // module doc and the milestone report for what that run did and did not cover).
}
