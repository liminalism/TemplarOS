//! Reaching the compositor's private `templar_shell_v1` protocol from a window
//! `pixelkit_shell`/winit created — the piece M2's design doc calls out by name: "work out the
//! cleanest way for a winit-based client to also speak our protocol on that connection".
//!
//! # Why this needs `unsafe` at all
//!
//! `templar_shell_manager_v1.set_layer_surface`/`set_exclusive_zone` take a `wl_surface` argument.
//! Wayland objects are scoped to one client *connection* — an object created on one `wl_display`
//! connection cannot be named from a different one, even to the same socket, because the
//! compositor treats each accepted stream as a distinct client with its own object-id namespace
//! (`compositor/templar-comp/src/shell_socket.rs`). So the request has to be sent on **the same
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
use std::os::fd::AsFd;

use wayland_backend::client::{Backend as WBackend, ObjectData, ObjectId};
use wayland_client::protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{self, WlDataDeviceManager},
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::{self, WlKeyboard},
    wl_pointer::{self, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

use crate::data_device::{ClipboardPayload, ForeignSelection, InputSerials, choose_target};
use templar_types::{AppId, InstanceId};
use templar_window::{
    GroupInfo, Layer, Rect, RevealHome, ShellToplevel, WindowId,
    geometry::{Alignment, Edge},
};

/// The highest `templar_shell_v1` version this shell speaks (`interfaces/wayland/templar-shell-v1.xml`).
/// Bound at the lower of this and what the compositor advertises, so a newer shell against an
/// older compositor still gets version 1's events rather than a protocol error.
///
/// Version 5 is the window-group and drop-down-terminal set. The manager's version is what gates
/// *every* interface in this protocol, including `templar_shell_toplevel_v1`'s own version 3
/// additions: a toplevel object is created by an event on a child of the manager, so libwayland
/// gives it the manager's bound version rather than the interface's own maximum. That is why the
/// group requests below check `self.manager.version()` and not `toplevel.version()` — against a
/// version-4 compositor the latter also reads 4, which is `>= 3` and would happily send a request
/// that compositor has never heard of.
const SHELL_PROTOCOL_VERSION: u32 = 7;

/// The manager version that carries `set_edge_alignment`. A version-5 compositor has never heard
/// of it, so the shell asks only when the manager it actually bound is new enough — the same rule
/// `GROUP_SINCE` follows, and for the same reason.
const ALIGNMENT_SINCE: u32 = 6;

/// The manager version that carries `templar_shell_toplevel_v1`'s window-group requests.
const GROUP_SINCE: u32 = 5;

use crate::protocol::templar_shell_v1::{
    templar_shell_manager_v1::{self, TemplarShellManagerV1},
    templar_shell_toplevel_list_v1::{self, TemplarShellToplevelListV1},
    templar_shell_toplevel_v1::{self, TemplarShellToplevelV1},
};

/// One toplevel as tracked from the wire, plus the object that lets the shell act on it.
///
/// The wire protocol never sends `templar_window::WindowId` — `templar_shell_toplevel_v1` objects carry
/// no such field (`interfaces/wayland/templar-shell-v1.xml`); the object's own identity on this
/// connection *is* the window's identity here. [`WindowId`] is reused anyway, rather than
/// inventing a parallel type, so the already-tested [`templar_window::grouping_key`],
/// [`crate::dock::group`] and [`crate::drawers::window_shelf`] keep working completely unmodified
/// against this data: its value is synthetic, [`ObjectId::protocol_id`] of this object, stable for
/// exactly as long as the object exists (see [`synthetic_window_id`]).
#[derive(Clone)]
struct TrackedToplevel {
    proxy: TemplarShellToplevelV1,
    state: ToplevelState,
}

/// Everything the wire has said about one toplevel, with no connection-scoped object in it.
///
/// Split out from [`TrackedToplevel`] so the event → [`ShellToplevel`] mapping is a pure function
/// of the events received, testable without a live compositor — which is the only part of this
/// module that has ever been testable at all (see the test module at the bottom for why).
#[derive(Clone, Debug)]
struct ToplevelState {
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
    /// The window group this window belongs to, as the `group` event last reported it
    /// (version 3). `None` is "in no group" — the wire says so with a `group_id` of 0.
    group: Option<GroupInfo>,
}

impl TrackedToplevel {
    fn new(proxy: TemplarShellToplevelV1) -> Self {
        TrackedToplevel {
            proxy,
            state: ToplevelState::default(),
        }
    }
}

impl Default for ToplevelState {
    fn default() -> Self {
        ToplevelState {
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
            group: None,
        }
    }
}

impl ToplevelState {
    /// Fold one `templar_shell_toplevel_v1` event in. `Closed` never reaches here — it removes the
    /// whole entry, which the dispatcher handles before looking one up.
    fn apply(&mut self, event: templar_shell_toplevel_v1::Event) {
        match event {
            templar_shell_toplevel_v1::Event::Identity {
                canonical_app_id,
                instance_id,
                verified,
            } => {
                // A string the compositor resolved that this side cannot parse is dropped, and
                // the window then has no application at all: no icon on its bar chip, and a
                // grouping key of `unknown-window-<n>`. That used to happen in silence, which is
                // one of the two things the all-initials bar could be; say it instead.
                self.canonical_app_id = canonical_app_id.as_deref().and_then(|id| {
                    let parsed = id.parse().ok();
                    if parsed.is_none() && !id.is_empty() {
                        tracing::warn!(
                            app_id = id,
                            "the compositor resolved an identity this shell cannot parse; the \
                             window will have no application"
                        );
                    }
                    parsed
                });
                self.instance_id = instance_id.as_deref().and_then(InstanceId::parse);
                self.verified_identity = verified != 0;
            }
            templar_shell_toplevel_v1::Event::Title { title } => self.title = title,
            templar_shell_toplevel_v1::Event::State {
                activated,
                fullscreen,
                shelved,
                shelf_rank: _,
            } => {
                // `shelf_rank` is the wire's authoritative shelve order (see
                // `interfaces/wayland/templar-shell-v1.xml`); `crate::drawers::window_shelf`
                // currently sorts by `window_id` instead (its own module doc explains why: the
                // sanitized `ShellToplevel` this crate renders against has no such field yet).
                // Wiring it through is the natural follow-up once that struct grows one — not a
                // silent loss, since the events themselves carry it either way.
                self.activated = activated != 0;
                self.fullscreen = fullscreen != 0;
                self.shelved = shelved != 0;
            }
            templar_shell_toplevel_v1::Event::Layer { layer } => {
                if let WEnum::Value(layer) = layer {
                    self.layer = layer_from_wire(layer);
                }
            }
            templar_shell_toplevel_v1::Event::Identifier { identifier } => {
                self.identifier = Some(identifier);
            }
            templar_shell_toplevel_v1::Event::Geometry {
                x,
                y,
                width,
                height,
            } => {
                self.geometry = Rect::new(x, y, width, height);
            }
            templar_shell_toplevel_v1::Event::Maximized { maximized } => {
                self.maximized = maximized != 0;
            }
            templar_shell_toplevel_v1::Event::Group {
                group_id,
                name,
                accent,
            } => {
                self.group = group_from_wire(group_id, name, accent);
            }
            templar_shell_toplevel_v1::Event::Closed => unreachable!("handled by the dispatcher"),
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
            group: self.group.clone(),
        }
    }
}

/// The `group` event's three arguments as the shell's own value.
///
/// `group_id` 0 is the wire's "in no group", and the protocol promises `name` is then empty and
/// `accent` 0 — but this does not check that, because a window in no group has no name and no
/// accent to disagree about: the `None` discards both either way.
fn group_from_wire(group_id: u32, name: String, accent: u32) -> Option<GroupInfo> {
    (group_id != 0).then(|| GroupInfo {
        group_id: u64::from(group_id),
        name,
        // `0x00RRGGBB` on the wire; any alpha byte a compositor sets is not ours to interpret.
        accent: accent & 0x00ff_ffff,
    })
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
    manager: Option<TemplarShellManagerV1>,
    toplevels: HashMap<ObjectId, TrackedToplevel>,
    /// `templar_shell_manager_v1.home_revealed`'s last word, whichever side started the reveal.
    home_revealed: bool,
    /// `templar_shell_manager_v1.bar_peeked`'s last word: whether the pointer is resting on the
    /// bottom edge of an output under a window that fills the screen. Empty means no.
    ///
    /// The compositor has to tell us, because under such a window the pointer never reaches our
    /// surface — whatever is under it owns it — so a reveal we cannot see is a reveal we cannot
    /// answer. Not compared against an output name here: this shell draws one bar.
    bar_peeked: bool,
    /// Presses of the terminal chord the compositor could not answer itself because no terminal
    /// window exists. Counted rather than flagged: two quick presses are two launches asked for,
    /// and a flag would silently drop the second.
    terminal_requests: u32,
    /// Bumped on every toplevel or reveal event, so a host can redraw when the window list
    /// changed and only then, instead of on a timer.
    generation: u64,
    /// Window previews over the standard capture protocols (`crate::previews`).
    pub(crate) previews: crate::previews::PreviewState,
    /// `ext-session-lock-v1` (`crate::session_lock`): what actually locks the session.
    pub(crate) session_lock: crate::session_lock::SessionLockState,
    /// The ordinary clipboard and drag-and-drop globals. The shell is a normal Wayland client as
    /// well as a privileged one, and this is the normal half.
    pub(crate) data: DataDeviceBridge,
}

/// Everything `wl_data_device` needs, on the same connection as the rest.
#[derive(Default)]
pub(crate) struct DataDeviceBridge {
    manager: Option<WlDataDeviceManager>,
    seat: Option<WlSeat>,
    device: Option<WlDataDevice>,
    /// Bound only to receive serials: `set_selection` and `start_drag` must name a real input
    /// event, and this connection has to have seen one to have a serial at all.
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    serials: InputSerials,
    /// What to write when a receiver asks. Held for as long as the selection is Home's.
    payload: Option<ClipboardPayload>,
    /// The source currently offered as the seat selection, and whether it is still live —
    /// `cancelled` arrives the moment another client takes the selection.
    source: Option<WlDataSource>,
    own_selection_live: bool,
    /// The offer the compositor last named as the selection, and the types it advertises.
    offer: Option<WlDataOffer>,
    offer_mime_types: Vec<String>,
    /// The offer being built up: `wl_data_offer.offer` events arrive before the
    /// `wl_data_device.selection` that names which offer they belonged to.
    pending_mime_types: HashMap<ObjectId, Vec<String>>,
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
            if interface == "templar_shell_manager_v1" {
                let version = version.min(SHELL_PROTOCOL_VERSION);
                state.manager =
                    Some(registry.bind::<TemplarShellManagerV1, _, _>(name, version, qh, ()));
            } else if interface == "wl_data_device_manager" {
                // Version 3 is what everything ships; the drag/selection requests this uses have
                // existed since version 1, so a lower advertisement still works.
                let version = version.min(3);
                state.data.manager =
                    Some(registry.bind::<WlDataDeviceManager, _, _>(name, version, qh, ()));
                state.data.attach(qh);
            } else if interface == "wl_seat" && state.data.seat.is_none() {
                // A second `wl_seat` binding beside winit's own. Two bindings of one seat is
                // ordinary Wayland — each gets its own keyboard and pointer objects receiving the
                // same events — and it is the only way to see serials without asking winit for
                // something it does not expose.
                let version = version.min(5);
                state.data.seat = Some(registry.bind::<WlSeat, _, _>(name, version, qh, ()));
                state.data.attach(qh);
            } else {
                state
                    .previews
                    .bind_global(registry, name, &interface, version, qh);
                state
                    .session_lock
                    .bind_global(registry, name, &interface, version, qh);
            }
        }
    }
}

impl DataDeviceBridge {
    /// Create the data device once both halves have arrived.
    ///
    /// Called from each global's arm rather than after the round trip because the registry does
    /// not promise an order: whichever of the manager and the seat is advertised second is the
    /// one that completes the pair.
    fn attach(&mut self, qh: &QueueHandle<BridgeState>) {
        if self.device.is_some() {
            return;
        }
        let (Some(manager), Some(seat)) = (self.manager.as_ref(), self.seat.as_ref()) else {
            return;
        };
        self.device = Some(manager.get_data_device(seat, qh, ()));
    }

    /// Forget the selection Home was offering, whether it was taken away or replaced.
    fn drop_own_source(&mut self) {
        if let Some(source) = self.source.take() {
            source.destroy();
        }
        self.payload = None;
        self.own_selection_live = false;
    }
}

impl Dispatch<WlDataDeviceManager, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: wl_data_device_manager::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `wl_data_device_manager` has no events.
    }
}

impl Dispatch<WlSeat, ()> for BridgeState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        (): &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // Keyboard and pointer are taken purely for their serials — nothing here interprets a
        // key or a click, which is winit's job on its own objects.
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.data.keyboard.is_none()
            {
                state.data.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Pointer) && state.data.pointer.is_none() {
                state.data.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match &event {
            wl_keyboard::Event::Enter { serial, .. }
            | wl_keyboard::Event::Leave { serial, .. }
            | wl_keyboard::Event::Key { serial, .. }
            | wl_keyboard::Event::Modifiers { serial, .. } => state.data.serials.record(*serial),
            _ => {}
        }
        // The same keyboard is where the password arrives: while locked the compositor focuses
        // the lock surface, and this connection's keyboard is the one that sees it.
        crate::session_lock::keyboard_event(state, &event);
    }
}

impl Dispatch<WlPointer, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. }
            | wl_pointer::Event::Leave { serial, .. }
            | wl_pointer::Event::Button { serial, .. } => state.data.serials.record(serial),
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // A new offer object, whose types arrive as `wl_data_offer.offer` events *before* the
            // `selection` that says which offer they described. Collected by object id until then.
            wl_data_device::Event::DataOffer { id } => {
                state.data.pending_mime_types.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(previous) = state.data.offer.take() {
                    previous.destroy();
                }
                match id {
                    Some(offer) => {
                        state.data.offer_mime_types = state
                            .data
                            .pending_mime_types
                            .remove(&offer.id())
                            .unwrap_or_default();
                        state.data.offer = Some(offer);
                    }
                    // A null selection means the seat holds nothing at all.
                    None => {
                        state.data.offer_mime_types.clear();
                    }
                }
                state.data.pending_mime_types.clear();
            }
            _ => {}
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        match opcode {
            // `wl_data_device.data_offer` is opcode 0 and the only event carrying a new id.
            0 => qh.make_data::<WlDataOffer, ()>(()),
            other => panic!("wl_data_device: unexpected new-id event opcode {other}"),
        }
    }
}

impl Dispatch<WlDataOffer, ()> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            // Before `selection` names the offer it belongs to, and afterwards for an offer that
            // is already the selection; both end up in the right place.
            if let Some(pending) = state.data.pending_mime_types.get_mut(&proxy.id()) {
                pending.push(mime_type);
            } else if state
                .data
                .offer
                .as_ref()
                .is_some_and(|offer| offer == proxy)
            {
                state.data.offer_mime_types.push(mime_type);
            }
        }
    }
}

impl Dispatch<WlDataSource, ()> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &WlDataSource,
        event: wl_data_source::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Only the source currently offered may answer; a cancelled one that has not been dropped
        // yet must not still be writing files to whoever asks.
        if state
            .data
            .source
            .as_ref()
            .is_none_or(|source| source != proxy)
        {
            return;
        }
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let Some(payload) = state.data.payload.as_ref() else {
                    return;
                };
                let Some(bytes) = payload.bytes_for(&mime_type) else {
                    // Asked for something never offered: close the pipe rather than writing an
                    // empty file, which a receiver would read as "this really is empty".
                    return;
                };
                // The fd is ours to close, which is what tells the receiver the transfer ended.
                let mut file = std::fs::File::from(fd);
                if let Err(error) = std::io::Write::write_all(&mut file, bytes) {
                    // A receiver that went away mid-transfer is ordinary, not a fault here.
                    tracing::debug!(%error, %mime_type, "selection transfer ended early");
                }
            }
            // Another client took the selection. Home's clipboard is no longer what the seat is
            // offering, and `data_device::paste_source` reads exactly this fact.
            wl_data_source::Event::Cancelled => {
                state.data.own_selection_live = false;
                state.data.payload = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<TemplarShellManagerV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &TemplarShellManagerV1,
        event: templar_shell_manager_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            templar_shell_manager_v1::Event::HomeRevealed { revealed } => {
                state.home_revealed = revealed != 0;
                state.generation = state.generation.wrapping_add(1);
            }
            // Version 7.
            templar_shell_manager_v1::Event::BarPeeked { output } => {
                state.bar_peeked = !output.is_empty();
                state.generation = state.generation.wrapping_add(1);
            }
            // The chord was pressed with no terminal window to toggle, so the launch is ours:
            // the compositor cannot start an application, and identity comes from the cgroup of
            // the unit the launcher starts.
            templar_shell_manager_v1::Event::TerminalRequested => {
                state.terminal_requests = state.terminal_requests.saturating_add(1);
            }
            // Version 5: bound only once `SHELL_PROTOCOL_VERSION` reaches 5, with the
            // window-group work.
            templar_shell_manager_v1::Event::GroupCycleBinding { .. } => {}
        }
    }
}

impl Dispatch<TemplarShellToplevelListV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &TemplarShellToplevelListV1,
        event: templar_shell_toplevel_list_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let templar_shell_toplevel_list_v1::Event::Toplevel { id } = event;
        state.toplevels.insert(id.id(), TrackedToplevel::new(id));
        state.generation = state.generation.wrapping_add(1);
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        // `toplevel` is this interface's only event, so it is unconditionally opcode 0
        // (`interfaces/wayland/templar-shell-v1.xml`'s `templar_shell_toplevel_list_v1`).
        match opcode {
            0 => qh.make_data::<TemplarShellToplevelV1, ()>(()),
            other => panic!("templar_shell_toplevel_list_v1: unexpected event opcode {other}"),
        }
    }
}

impl Dispatch<TemplarShellToplevelV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &TemplarShellToplevelV1,
        event: templar_shell_toplevel_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Handled before looking the object up: once closed, there is nothing left to update,
        // and holding a `get_mut` borrow across this arm would fight the removal below it.
        state.generation = state.generation.wrapping_add(1);
        if matches!(event, templar_shell_toplevel_v1::Event::Closed) {
            state.toplevels.remove(&proxy.id());
            return;
        }
        let Some(tracked) = state.toplevels.get_mut(&proxy.id()) else {
            return;
        };
        tracked.state.apply(event);
    }
}

impl std::fmt::Debug for BridgeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeState")
            .field("toplevels", &self.toplevels.len())
            .field("home_revealed", &self.home_revealed)
            .field("bar_peeked", &self.bar_peeked)
            .field("previews", &self.previews)
            .finish_non_exhaustive()
    }
}

/// `templar_window::Layer` <-> the wire enum. Mirrors
/// `compositor/templar-comp/src/protocols_shell.rs::wire_layer` exactly, on the client's own copy of
/// the generated enum — the two are separate Rust types (client- vs server-generated bindings)
/// even though the wire encoding is one contract, so each side keeps its own explicit match rather
/// than share a conversion function across a client/server boundary that cannot itself share code.
fn wire_layer(layer: Layer) -> templar_shell_manager_v1::Layer {
    match layer {
        Layer::SecureLock => templar_shell_manager_v1::Layer::SecureLock,
        Layer::Recovery => templar_shell_manager_v1::Layer::Recovery,
        Layer::SystemModal => templar_shell_manager_v1::Layer::SystemModal,
        Layer::ShellOverlay => templar_shell_manager_v1::Layer::ShellOverlay,
        Layer::Notification => templar_shell_manager_v1::Layer::Notification,
        Layer::AppOnTop => templar_shell_manager_v1::Layer::AppOnTop,
        Layer::Normal => templar_shell_manager_v1::Layer::Normal,
        Layer::HomeSurface => templar_shell_manager_v1::Layer::HomeSurface,
    }
}

fn layer_from_wire(wire: templar_shell_manager_v1::Layer) -> Layer {
    match wire {
        templar_shell_manager_v1::Layer::SecureLock => Layer::SecureLock,
        templar_shell_manager_v1::Layer::Recovery => Layer::Recovery,
        templar_shell_manager_v1::Layer::SystemModal => Layer::SystemModal,
        templar_shell_manager_v1::Layer::ShellOverlay => Layer::ShellOverlay,
        templar_shell_manager_v1::Layer::Notification => Layer::Notification,
        templar_shell_manager_v1::Layer::AppOnTop => Layer::AppOnTop,
        templar_shell_manager_v1::Layer::Normal => Layer::Normal,
        templar_shell_manager_v1::Layer::HomeSurface => Layer::HomeSurface,
    }
}

fn wire_preset(
    preset: templar_window::ArrangementPreset,
) -> templar_shell_manager_v1::ArrangementPreset {
    match preset {
        templar_window::ArrangementPreset::Focus => {
            templar_shell_manager_v1::ArrangementPreset::Focus
        }
        templar_window::ArrangementPreset::Halves => {
            templar_shell_manager_v1::ArrangementPreset::Halves
        }
        templar_window::ArrangementPreset::Thirds => {
            templar_shell_manager_v1::ArrangementPreset::Thirds
        }
        templar_window::ArrangementPreset::Quarters => {
            templar_shell_manager_v1::ArrangementPreset::Quarters
        }
        templar_window::ArrangementPreset::MainAndStack => {
            templar_shell_manager_v1::ArrangementPreset::MainAndStack
        }
        templar_window::ArrangementPreset::Cushion => {
            templar_shell_manager_v1::ArrangementPreset::Cushion
        }
    }
}

fn wire_reveal(reveal: RevealHome) -> templar_shell_manager_v1::Reveal {
    match reveal {
        RevealHome::Show => templar_shell_manager_v1::Reveal::Show,
        RevealHome::Hide => templar_shell_manager_v1::Reveal::Hide,
        RevealHome::Toggle => templar_shell_manager_v1::Reveal::Toggle,
    }
}

fn wire_alignment(alignment: Alignment) -> templar_shell_manager_v1::Alignment {
    match alignment {
        Alignment::Start => templar_shell_manager_v1::Alignment::Start,
        Alignment::Centre => templar_shell_manager_v1::Alignment::Centre,
        Alignment::End => templar_shell_manager_v1::Alignment::End,
    }
}

fn wire_edge(edge: Edge) -> templar_shell_manager_v1::Edge {
    match edge {
        Edge::Top => templar_shell_manager_v1::Edge::Top,
        Edge::Bottom => templar_shell_manager_v1::Edge::Bottom,
        Edge::Left => templar_shell_manager_v1::Edge::Left,
        Edge::Right => templar_shell_manager_v1::Edge::Right,
    }
}

/// A live connection to the compositor's trusted-shell socket, speaking `templar_shell_v1` on the
/// exact same connection winit opened for a given window. See the module doc for the whole design
/// and its safety argument.
pub struct ShellBridge {
    connection: Connection,
    queue: EventQueue<BridgeState>,
    manager: TemplarShellManagerV1,
    state: BridgeState,
    /// The `Argb8888` buffer behind [`Self::present_masked`], one per surface, reused while its
    /// size holds. Per surface rather than one shared: the bar and the cluster are never the same
    /// size, so a single buffer meant every frame of either destroyed a `wl_buffer` still attached
    /// to the other and allocated a fresh pool.
    masked: HashMap<ObjectId, crate::previews::ShmBuffer>,
}

impl std::fmt::Debug for ShellBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellBridge")
            .field("toplevels", &self.state.toplevels.len())
            .finish_non_exhaustive()
    }
}

impl ShellBridge {
    /// Reach `templar_shell_v1` on the same Wayland connection `window` was created on.
    ///
    /// Fails cleanly (never panics) when: the window is not on Wayland at all, the process is not
    /// connected to the compositor's trusted-shell socket (so `templar_shell_manager_v1` was never
    /// advertised), or the initial registry round trip cannot complete. All three are ordinary,
    /// expected conditions — running nested for development without the trusted socket wired up,
    /// say — and the caller is expected to keep running with no protocol surface rather than
    /// treat this as fatal, the same way every drawer degrades when its D-Bus service is absent.
    pub fn connect(window: &winit::window::Window) -> Result<ShellBridge> {
        let display_handle = window
            .display_handle()
            .context("reading the window's display handle")?;
        let RawDisplayHandle::Wayland(wayland_display) = display_handle.as_raw() else {
            bail!("templar_shell_v1 is a Wayland-only protocol, and this window is not on Wayland");
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
                "templar_shell_manager_v1 was not advertised on this connection — this process is \
                 probably not connected to the compositor's trusted-shell socket \
                 (WAYLAND_DISPLAY should name the `*-shell` socket logged by templar-comp, not the \
                 ordinary one)"
            )
        })?;

        let _list = manager.get_toplevel_list(&qh, ());
        // One more round trip so the toplevel objects that already existed are in hand before the
        // first frame draws. Their identity/title/state/layer can still be default-empty
        // afterwards — `GetToplevelList`'s handler creates the objects but does not itself force
        // a `sync_toplevels` (`compositor/templar-comp/src/protocols_shell.rs`), so a window that was
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
            masked: HashMap::new(),
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
    /// (`interfaces/wayland/templar-shell-v1.xml`'s `set_exclusive_zone` doc).
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

    /// Sit at `alignment` along `edge` rather than at the edge's start
    /// (`interfaces/wayland/templar-shell-v1.xml`'s `set_edge_alignment`). Placement only: this
    /// reserves nothing and moves no window, so it may be sent as freely as a redraw.
    ///
    /// Silently does nothing against a compositor too old to have the request — which is the
    /// running one until the user logs out. The surface then sits at the start of its edge, which
    /// is where it sat before any of this, so an un-centred island is the worst of it.
    /// Returns whether the compositor was actually told. `false` means it is too old to have the
    /// request, so the surface stays at the start of its edge and the caller has to reason about
    /// it sitting there — the shell's own "is anything covering the bar?" depends on knowing
    /// which of the two happened.
    pub fn set_edge_alignment(
        &self,
        window: &winit::window::Window,
        edge: Edge,
        alignment: Alignment,
    ) -> Result<bool> {
        if self.manager.version() < ALIGNMENT_SINCE {
            return Ok(false);
        }
        let surface = self.wrap_surface(window)?;
        self.manager
            .set_edge_alignment(&surface, wire_edge(edge), wire_alignment(alignment));
        self.connection
            .flush()
            .context("flushing set_edge_alignment")?;
        Ok(true)
    }

    /// Process whatever has already arrived on this connection's own event queue, without
    /// blocking. Call once per frame/tick; see the module doc for why that cadence is enough.
    pub fn pump(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatching the trusted-shell connection's event queue")?;
        Ok(())
    }

    /// Offer `payload` to the rest of the desktop as the seat's selection.
    ///
    /// Whether this lands is the compositor's call, and Smithay's rule is keyboard focus rather
    /// than the serial (see [`crate::data_device`]): a copy made while Home has focus — which is
    /// every copy a person makes in Home — is accepted. `Ok(false)` means the transport was not
    /// available at all, which is the nested-development case and not a fault.
    pub fn offer_selection(&mut self, payload: ClipboardPayload) -> Result<bool> {
        let (Some(manager), Some(device)) = (
            self.state.data.manager.clone(),
            self.state.data.device.clone(),
        ) else {
            return Ok(false);
        };
        let Some(serial) = self.state.data.serials.latest() else {
            // No input event has ever reached this connection, so there is no serial to name. A
            // made-up one is worse than not offering: it is a request the protocol says is
            // malformed, and a stricter compositor is right to kill the client for it.
            return Ok(false);
        };

        self.state.data.drop_own_source();
        let qh = self.queue.handle();
        let source = manager.create_data_source(&qh, ());
        for mime_type in ClipboardPayload::offered_mime_types() {
            source.offer(mime_type.to_owned());
        }
        device.set_selection(Some(&source), serial);
        self.state.data.payload = Some(payload);
        self.state.data.source = Some(source);
        self.state.data.own_selection_live = true;
        self.connection
            .flush()
            .context("flushing the clipboard selection")?;
        Ok(true)
    }

    /// Whether Home still owns the seat selection.
    pub fn owns_selection(&self) -> bool {
        self.state.data.own_selection_live
    }

    /// Whether some other application is currently offering something Home could paste.
    pub fn foreign_selection_available(&self) -> bool {
        self.state.data.offer.is_some()
            && choose_target(&self.state.data.offer_mime_types).is_some()
    }

    /// Read the current foreign selection, or `None` when there is nothing pasteable on offer.
    ///
    /// Synchronous, because a paste is a thing a person just asked for and the answer is wanted
    /// now — bounded in time and size by [`crate::data_device::read_transfer`] so a badly behaved
    /// sender costs one slow paste rather than the frame loop.
    pub fn read_foreign_selection(&mut self) -> Result<Option<ForeignSelection>> {
        let Some(offer) = self.state.data.offer.clone() else {
            return Ok(None);
        };
        let Some(target) = choose_target(&self.state.data.offer_mime_types) else {
            return Ok(None);
        };
        let (read_end, write_end) = std::io::pipe().context("pipe for a selection transfer")?;
        offer.receive(target.mime_type().to_owned(), write_end.as_fd());
        self.connection
            .flush()
            .context("flushing a selection transfer request")?;
        // The sender only sees the request once the compositor has forwarded it, and only starts
        // writing after that. Dropping our copy of the write end first is what makes their close
        // the end of the read rather than a wait for our own descriptor.
        drop(write_end);
        let payload = crate::data_device::read_transfer(std::os::fd::OwnedFd::from(read_end))
            .context("reading a selection transfer")?;
        Ok(
            crate::data_device::clipboard_from_transfer(target, &payload)
                .as_ref()
                .map(ForeignSelection::of),
        )
    }

    /// Start a drag carrying `payload` from `window`'s surface.
    ///
    /// The export half of typed drag and drop. `Ok(false)` when the transport or a serial is
    /// missing, exactly as [`Self::offer_selection`].
    pub fn start_file_drag(
        &mut self,
        window: &winit::window::Window,
        payload: ClipboardPayload,
    ) -> Result<bool> {
        let (Some(manager), Some(device)) = (
            self.state.data.manager.clone(),
            self.state.data.device.clone(),
        ) else {
            return Ok(false);
        };
        let Some(serial) = self.state.data.serials.latest() else {
            return Ok(false);
        };
        let surface = self.wrap_surface(window)?;
        let qh = self.queue.handle();
        let source = manager.create_data_source(&qh, ());
        for mime_type in ClipboardPayload::offered_mime_types() {
            source.offer(mime_type.to_owned());
        }
        // No icon surface: the compositor draws its own drag feedback, and a shell-drawn icon
        // would need a second surface on winit's connection that nothing here owns.
        device.start_drag(Some(&source), &surface, None, serial);
        self.state.data.payload = Some(payload);
        self.state.data.source = Some(source);
        self.connection.flush().context("flushing a drag start")?;
        Ok(true)
    }

    /// Every toplevel this connection currently knows about, for [`crate::dock`] and
    /// [`crate::drawers::window_shelf`] to consume exactly as they already do against fixtures.
    pub fn toplevels(&self) -> Vec<ShellToplevel> {
        self.state
            .toplevels
            .iter()
            .map(|(id, tracked)| tracked.state.snapshot(synthetic_window_id(id)))
            .collect()
    }

    fn toplevel_object(&self, window_id: WindowId) -> Option<&TemplarShellToplevelV1> {
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
        toplevel.activate("templar-shell-ui".to_owned());
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
            bail!("the compositor speaks templar_shell_v1 version 1, which has no set_maximized");
        }
        toplevel.set_maximized(u32::from(maximized));
        self.connection.flush().context("flushing set_maximized")
    }

    /// The `templar_shell_v1` version actually negotiated with this compositor. Window groups need
    /// version 5; anything below that is a compositor from before they existed, and a request it
    /// has never heard of is a protocol error that kills the connection — so every group
    /// operation refuses here rather than on the wire.
    fn group_requests_available(&self) -> Result<()> {
        if self.manager.version() < GROUP_SINCE {
            bail!(
                "the compositor speaks templar_shell_v1 version {}, which has no window groups",
                self.manager.version()
            );
        }
        Ok(())
    }

    /// Put `window` in `other`'s window group, making one of the two when `other` has none
    /// (version 5). The compositor also sets `window` down beside `other` — grouping places, it
    /// never merges — so this is the pointer half of dropping one title band on another.
    pub fn join_group(&self, window_id: WindowId, other_id: WindowId) -> Result<()> {
        self.group_requests_available()?;
        if window_id == other_id {
            bail!("a window cannot be grouped with itself: {window_id:?}");
        }
        let Some(other) = self.toplevel_object(other_id).cloned() else {
            bail!("no such window on this connection: {other_id:?}");
        };
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.join_group(&other);
        self.connection.flush().context("flushing join_group")
    }

    /// Take `window` out of whatever group it is in (version 5). A group left with one member
    /// dissolves on its own in the compositor; there is no request for that and none is needed.
    pub fn leave_group(&self, window_id: WindowId) -> Result<()> {
        self.group_requests_available()?;
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.leave_group();
        self.connection.flush().context("flushing leave_group")
    }

    /// Raise every member of `window`'s group together, `window` on top, and focus it
    /// (version 5) — what pressing a group tile's title on Home means.
    ///
    /// The token is handled exactly as [`ShellBridge::activate`]'s is: the compositor grants a
    /// trusted-shell activation whatever string arrives, because this connection *is* the
    /// evidence. The argument exists for the ordinary application-to-application handoff, which
    /// this is not.
    pub fn activate_group(&self, window_id: WindowId) -> Result<()> {
        self.group_requests_available()?;
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.activate_group("templar-shell-ui".to_owned());
        self.connection.flush().context("flushing activate_group")
    }

    /// Park every member of `window`'s group as one (version 5): one shelf entry for the set,
    /// so the group returns the way it left.
    pub fn shelve_group(&self, window_id: WindowId) -> Result<()> {
        self.group_requests_available()?;
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.shelve_group();
        self.connection.flush().context("flushing shelve_group")
    }

    /// Bring every member of `window`'s group back (version 5).
    pub fn unshelve_group(&self, window_id: WindowId) -> Result<()> {
        self.group_requests_available()?;
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.unshelve_group();
        self.connection.flush().context("flushing unshelve_group")
    }

    /// Name `window`'s group (version 5). The name is the group's, not the window's: every
    /// member's next `group` event carries it, and the Home tile takes its title from there.
    pub fn set_group_name(&self, window_id: WindowId, name: &str) -> Result<()> {
        self.group_requests_available()?;
        let Some(toplevel) = self.toplevel_object(window_id) else {
            bail!("no such window on this connection: {window_id:?}");
        };
        toplevel.set_group_name(name.to_owned());
        self.connection.flush().context("flushing set_group_name")
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

    /// Home (version 2): the field's Home button reaches the same reveal as a bare Super
    /// tap and the compositor's Ctrl+Alt+H / Super+D fallbacks.
    pub fn reveal_home(&self, reveal: RevealHome) -> Result<()> {
        if self.manager.version() < 2 {
            bail!("the compositor speaks templar_shell_v1 version 1, which has no reveal_home");
        }
        self.manager.reveal_home(wire_reveal(reveal));
        self.connection.flush().context("flushing reveal_home")
    }

    /// Arrange every eligible open window per the named preset (version 3). The compositor is
    /// the sole geometry authority; this only names the preset and lets the baseline semantics
    /// live in `templar_window::WindowService::arrange_windows`.
    pub fn arrange_windows(&self, preset: templar_window::ArrangementPreset) -> Result<()> {
        if self.manager.version() < 3 {
            bail!("the compositor speaks templar_shell_v1 < 3, which has no arrange_windows");
        }
        self.manager.arrange_windows(wire_preset(preset));
        self.connection.flush().context("flushing arrange_windows")
    }

    /// Return every surviving window exactly to the retained pre-arrangement baseline
    /// (version 3).
    pub fn restore_arrangement(&self) -> Result<()> {
        if self.manager.version() < 3 {
            bail!("the compositor speaks templar_shell_v1 < 3, which has no restore_arrangement");
        }
        self.manager.restore_arrangement();
        self.connection
            .flush()
            .context("flushing restore_arrangement")
    }

    /// Tell the compositor how long the session may be idle before the screen turns off
    /// (version 4). `seconds` of 0 is never.
    ///
    /// The value is `org.templaros.Power1`'s `screen_off_after` for the current power source. The
    /// shell is the relay and nothing more: the policy is sessiond's, and the enforcement is the
    /// compositor's, because the compositor is the only process that can see the seat go quiet.
    pub fn set_idle_timeout(&self, seconds: u32) -> Result<()> {
        if self.manager.version() < 4 {
            bail!("the compositor speaks templar_shell_v1 < 4, which has no set_idle_timeout");
        }
        self.manager.set_idle_timeout(seconds);
        self.connection.flush().context("flushing set_idle_timeout")
    }

    /// Tell the compositor which application is the drop-down terminal (version 5): the
    /// canonical id of the selected `terminal` provider, or the empty string when none is.
    ///
    /// Sent on connect and on every change of the selection. The chord itself is the
    /// compositor's — it has to keep toggling an existing terminal with this process dead — so
    /// this is the one thing the shell has to tell it: *which* window that is.
    pub fn set_terminal(&self, app_id: &str) -> Result<()> {
        if self.manager.version() < 5 {
            bail!("the compositor speaks templar_shell_v1 < 5, which has no set_terminal");
        }
        self.manager.set_terminal(app_id.to_owned());
        self.connection.flush().context("flushing set_terminal")
    }

    /// How many times the chord asked for a terminal to be *started* since this was last called.
    ///
    /// Draining rather than reading: each press is one launch, and a request left in place would
    /// be answered again on the next frame.
    pub fn take_terminal_requests(&mut self) -> u32 {
        std::mem::take(&mut self.state.terminal_requests)
    }

    /// Whether a "go home" reveal is in effect, as last reported by the compositor.
    /// A counter that moves whenever the toplevel list or the reveal state changed.
    pub fn generation(&self) -> u64 {
        self.state.generation
    }

    pub fn bar_peeked(&self) -> bool {
        self.state.bar_peeked
    }

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

    /// Whether this connection can lock the session at all (`crate::session_lock`).
    pub fn lock_available(&self) -> bool {
        self.state.session_lock.available()
    }

    /// Whether a lock is up.
    pub fn lock_engaged(&self) -> bool {
        self.state.session_lock.engaged()
    }

    /// Whether the compositor confirmed it, or finished it without granting it.
    pub fn lock_locked(&self) -> bool {
        self.state.session_lock.locked()
    }

    pub fn lock_finished(&self) -> bool {
        self.state.session_lock.finished()
    }

    /// Whether a lock surface changed size since the last paint.
    pub fn lock_needs_paint(&self) -> bool {
        self.state.session_lock.dirty
    }

    /// Lock the session: one surface per output, then paint them.
    pub fn lock(&mut self) -> Result<()> {
        let qh = self.queue.handle();
        self.state.session_lock.lock(&qh)?;
        self.connection.flush().context("flushing the session lock")
    }

    /// Paint every lock surface with `render`, which fills `width * height` pixels in pixelkit's
    /// `0x00RRGGBB` for one surface.
    pub fn paint_lock(&mut self, render: impl FnMut(u32, u32) -> Vec<u32>) -> Result<()> {
        let qh = self.queue.handle();
        let shm = self
            .state
            .previews
            .shm()
            .cloned()
            .ok_or_else(|| anyhow!("the compositor offers no wl_shm on this connection"))?;
        self.state.session_lock.paint(&shm, &qh, render)?;
        self.connection
            .flush()
            .context("flushing the lock screen's frames")
    }

    /// Unlock and tear the surfaces down. Only this clears the compositor's lock.
    pub fn unlock(&mut self) -> Result<()> {
        self.state.session_lock.unlock();
        self.connection.flush().context("flushing the unlock")
    }

    /// The keys the lock surfaces received since the last call.
    pub fn take_lock_keys(&mut self) -> Vec<crate::session_lock::LockKey> {
        self.state.session_lock.take_keys()
    }

    /// The foreign-toplevel identifier for `window_id`, once the compositor has relayed it —
    /// the key a preview of this window is filed under.
    pub fn identifier_of(&self, window_id: WindowId) -> Option<&str> {
        self.state
            .toplevels
            .iter()
            .find(|(id, _)| synthetic_window_id(id) == window_id)
            .and_then(|(_, tracked)| tracked.state.identifier.as_deref())
    }

    /// Attach an `Argb8888` buffer of `colour`, with its alpha taken from `coverage`, to
    /// `window`'s own surface, so a shell surface can be a shape rather than a rectangle.
    ///
    /// The window stays winit's: it keeps its xdg role, its configure handling and every pointer
    /// and keyboard event, and only the pixels come from here. That is deliberate rather than
    /// convenient — `templar_shell_v1.set_layer_surface` only *tags* a surface with a layer and
    /// grants no role of its own, so a surface created on this connection instead of winit's
    /// would never map. The alternative was a compositor-side role, which is a much larger
    /// change for the same pixels.
    ///
    /// The opaque region is cleared on every attach: without it the compositor is entitled to
    /// skip everything behind the surface, which is precisely the desktop the transparency is
    /// there to show.
    ///
    // ponytail: one buffer, reused by size, attached without waiting for `wl_buffer.release` —
    // the same simplification `session_lock` makes, and the same reason (one surface, one frame
    // in flight, redrawn only when something changed). A double buffer is the upgrade if a
    // frame ever tears.
    pub fn present_masked(
        &mut self,
        window: &winit::window::Window,
        colour: &[u32],
        coverage: &[u32],
        shapes: &[(i32, i32, i32, i32)],
        width: u32,
        height: u32,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            bail!("a surface with no pixels");
        }
        let surface = self.wrap_surface(window)?;
        let qh = self.queue.handle();
        let shm = self
            .state
            .previews
            .shm()
            .cloned()
            .ok_or_else(|| anyhow!("the compositor offers no wl_shm on this connection"))?;
        let key = surface.id();
        let reuse = self
            .masked
            .get(&key)
            .is_some_and(|buffer| buffer.width == width && buffer.height == height);
        if !reuse {
            if let Some(old) = self.masked.remove(&key) {
                old.destroy();
            }
            self.masked.insert(
                key.clone(),
                crate::previews::allocate(
                    &shm,
                    width,
                    height,
                    wayland_client::protocol::wl_shm::Format::Argb8888,
                    &qh,
                )
                .context("allocating the masked surface buffer")?,
            );
        }
        let buffer = self.masked.get(&key).expect("allocated above");
        buffer.write_masked(colour, coverage)?;
        surface.set_opaque_region(None);
        // And the input region is the shapes, for the same reason the alpha is. Without it the
        // surface's whole rectangle takes the pointer: a click on the desktop *between* two chips
        // would be swallowed by a bar that is not there, and the compositor would read the pointer
        // as being on the shell's own chrome — which is what suppresses the hot corner and holds a
        // fullscreen peek open.
        if let Some(compositor) = self.state.session_lock.compositor() {
            let region = compositor.create_region(&qh, ());
            for (x, y, width, height) in shapes {
                region.add(*x, *y, *width, *height);
            }
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        surface.attach(Some(&buffer.buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        self.connection
            .flush()
            .context("flushing a masked surface frame")
    }

    fn wrap_surface(&self, window: &winit::window::Window) -> Result<WlSurface> {
        let window_handle = window
            .window_handle()
            .context("reading the window's window handle")?;
        let RawWindowHandle::Wayland(handle) = window_handle.as_raw() else {
            bail!("templar_shell_v1 is a Wayland-only protocol, and this window is not on Wayland");
        };
        // SAFETY: `handle.surface` is winit's own `wl_surface*` for `window`, valid for as long as
        // `window` is alive (the borrow of `window` above ties this call's lifetime to it).
        // `ObjectId::from_ptr` only reads the pointer's already-established libwayland bookkeeping
        // (interface name, liveness flag); it registers nothing new (see the module doc).
        let object_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }
                .context("wrapping the window's wl_surface for templar_shell_v1")?;
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

    fn group_event(group_id: u32, name: &str, accent: u32) -> templar_shell_toplevel_v1::Event {
        templar_shell_toplevel_v1::Event::Group {
            group_id,
            name: name.to_owned(),
            accent,
        }
    }

    #[test]
    fn a_group_event_reaches_the_shell_toplevel() {
        let mut state = ToplevelState::default();
        state.apply(group_event(7, "Writing", 0x0033_88ff));
        assert_eq!(
            state.snapshot(WindowId(1)).group,
            Some(GroupInfo {
                group_id: 7,
                name: "Writing".to_owned(),
                accent: 0x0033_88ff,
            })
        );
    }

    #[test]
    fn group_id_zero_is_no_group() {
        let mut state = ToplevelState::default();
        assert_eq!(state.snapshot(WindowId(1)).group, None);
        state.apply(group_event(7, "Writing", 0x0033_88ff));
        // Leaving a group is the same event with a zero id, and it must clear the name and the
        // accent too — a stale accent would tint a tile that is no longer in any group.
        state.apply(group_event(0, "", 0));
        assert_eq!(state.snapshot(WindowId(1)).group, None);
    }

    #[test]
    fn a_group_accent_keeps_only_its_colour_bytes() {
        let mut state = ToplevelState::default();
        state.apply(group_event(2, "Reading", 0xff11_2233));
        assert_eq!(
            state.snapshot(WindowId(1)).group.map(|group| group.accent),
            Some(0x0011_2233)
        );
    }

    #[test]
    fn a_group_event_disturbs_nothing_else_the_wire_said() {
        let mut state = ToplevelState::default();
        state.apply(templar_shell_toplevel_v1::Event::Title {
            title: "Notes".to_owned(),
        });
        state.apply(templar_shell_toplevel_v1::Event::State {
            activated: 1,
            fullscreen: 0,
            shelved: 1,
            shelf_rank: 3,
        });
        state.apply(templar_shell_toplevel_v1::Event::Geometry {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        });
        state.apply(group_event(4, "Work", 0x0000_ff00));
        let snapshot = state.snapshot(WindowId(9));
        assert_eq!(snapshot.window_id, WindowId(9));
        assert_eq!(snapshot.title, "Notes");
        assert!(snapshot.activated && snapshot.shelved && !snapshot.fullscreen);
        assert_eq!(snapshot.geometry, Rect::new(10, 20, 300, 200));
        assert_eq!(snapshot.group.map(|group| group.group_id), Some(4));
    }

    // `synthetic_window_id`, `wrap_surface`, `connect` and every `ShellBridge` method beyond this
    // need a live Wayland connection to exercise — there is no fixture for "an `ObjectId` handed
    // out by a real libwayland connection" the way `dock.rs`/`window_shelf.rs` fixture
    // `ShellToplevel`s directly. That is why this module keeps the wire decoding (testable, and
    // tested above, now that `ToplevelState` holds no proxy) strictly separate from the
    // connection plumbing around it (exercised only by actually running the shell against a
    // compositor — see the crate's top module doc and the milestone report for what that run did
    // and did not cover).
}
