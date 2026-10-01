//! XWayland: `X11Wm::start_wm` plus the `XwmHandler` bridge from X11 windows into
//! `templar_window::WindowService`, following the same mapped/geometry/fullscreen/modal contract as
//! `src/xdg_shell.rs` does for native Wayland toplevels. `WM_CLASS` becomes the window's claim
//! (`window_bridge::claims_from_x11`); `modal_request`/`unmodal_request` map straight to
//! `WindowService::set_modal` — this Smithay revision derives X11 modality (transient-for plus
//! `_NET_WM_STATE_MODAL`) itself and calls these back directly, so no manual property inspection
//! is needed here at all.
//!
//! Override-redirect windows (menus, tooltips — COMPOSITOR.md doesn't mention them because
//! `templar_window` has no concept of them either: they are not "windows" in the recoverable-geometry
//! sense) are mapped straight into `Space` at whatever geometry the client itself chose and never
//! touch `WindowService`, matching every other Smithay-based compositor's treatment of them.
//!
//! Selection/clipboard bridging between XWayland and native Wayland clients is on: one
//! shared clipboard, whichever side a program lives on (`allow_selection_access` answers `true`
//! for every selection).

use crate::state::{Backend, TemplarCompState};
use crate::window_bridge::{claims_from_x11, client_kind};
use smithay::{
    desktop::Window,
    reexports::calloop::LoopHandle,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        selection::SelectionTarget, xwayland_keyboard_grab::XWaylandKeyboardGrabHandler,
        xwayland_shell::XWaylandShellHandler,
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmHandler, XwmId},
    },
};

impl<B: Backend + 'static> XWaylandShellHandler for TemplarCompState<B> {
    fn xwayland_shell_state(
        &mut self,
    ) -> &mut smithay::wayland::xwayland_shell::XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl<B: Backend + 'static> XWaylandKeyboardGrabHandler for TemplarCompState<B> {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<WlSurface> {
        Some(surface.clone())
    }
}

impl<B: Backend + 'static> XwmHandler for TemplarCompState<B> {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("XwmHandler called with no X11Wm installed")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        let Some(surface) = window.wl_surface() else {
            // XWayland associates the wl_surface before requesting the map in the ordinary case;
            // if it somehow hasn't yet, there is nothing to map into `WindowService` yet and the
            // window stays invisible until a property/configure notification gives us another
            // chance. Not observed in practice against the clients this crate was run against.
            return;
        };
        let geometry = window.geometry();
        // An X11 client may have placed itself, and that position is honoured; the size is
        // asked for as the frame around it, since the tag strip goes below the client.
        let requested = templar_window::frame_around(templar_window::Rect::new(
            geometry.loc.x,
            geometry.loc.y,
            geometry.size.w,
            geometry.size.h,
        ));
        let claims = claims_from_x11(Some(window.class()));
        let trusted_shell = surface
            .client()
            .is_some_and(|client| crate::state::client_is_trusted_shell(&client));
        let peer_pid = window.pid().unwrap_or(0);
        let x11_id = window.window_id();

        let object_id = smithay::reexports::wayland_server::Resource::id(&surface);
        if let Some(window_id) = self
            .windows
            .window_id_for_x11(x11_id)
            .or_else(|| self.windows.window_id_for(&object_id))
        {
            self.window_service.set_mapped(window_id, true);
            self.window_service.set_title(window_id, window.title());
            self.windows.mark_x11_mapped(x11_id, surface, window_id);
            let client = crate::space_sync::client_area(self, window_id);
            let _ = window.configure(Some(rect_to_smithay(client)));
            let element = Window::new_x11_window(window);
            self.windows.track_element(object_id, element.clone());
            self.space.map_element(element, (client.x, client.y), false);
            tracing::info!(?window_id, backend = "xwayland", "toplevel remapped");
            crate::space_sync::sync_after_change(self);
            return;
        }

        let window_id = self.window_service.on_toplevel_mapped(
            peer_pid,
            client_kind(trusted_shell),
            claims,
            window.title(),
            requested,
        );
        self.windows.mark_x11_mapped(x11_id, surface, window_id);

        let client = crate::space_sync::client_area(self, window_id);
        let _ = window.configure(Some(rect_to_smithay(client)));
        let element = Window::new_x11_window(window);
        self.windows.track_element(object_id, element.clone());
        self.space.map_element(element, (client.x, client.y), false);
        // Same rule as the xdg path (`xdg_shell.rs::handle_toplevel_commit`): a newly mapped
        // client window takes the keyboard.
        if self.window_service.should_activate_on_map(window_id) {
            crate::input::activate_window(self, window_id);
        }
        crate::space_sync::sync_after_change(self);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let geometry = window.geometry();
        self.space
            .map_element(Window::new_x11_window(window), geometry.loc, true);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if window.is_override_redirect() {
            if let Some(element) = window_element(self, &window) {
                self.space.unmap_elem(&element);
            }
            return;
        }
        let _ = window.set_mapped(false);
        if let Some(window_id) = self.windows.mark_x11_unmapped(window.window_id()) {
            let was_activated = self
                .window_service
                .window(window_id)
                .is_some_and(|record| record.activated);
            if let Some(element) = window_element(self, &window) {
                self.space.unmap_elem(&element);
            }
            self.window_service.set_mapped(window_id, false);
            tracing::info!(?window_id, backend = "xwayland", "toplevel unmapped");
            if was_activated {
                crate::input::refocus_after_window_hidden(self);
            } else {
                crate::space_sync::sync_after_change(self);
            }
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(element) = window_element(self, &window) {
            self.space.unmap_elem(&element);
        }
        self.remove_x11_window(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Position is always compositor-decided (`WindowService`'s geometry authority); only a
        // requested size is honored, matching the "floating X11 windows may resize themselves,
        // nothing else" lesson (cosmic-comp `src/xwayland.rs`).
        let current = window.geometry();
        let size = Size::from((
            w.map_or(current.size.w, |w| w as i32),
            h.map_or(current.size.h, |h| h as i32),
        ));
        // The frame follows the size the client chose, at the position the window has — the
        // X11 counterpart of `adopt_client_size` — so the tags stay below the client rather
        // than over a client that just grew over them.
        if let Some(window_id) = self.window_id_of(&window)
            && let Some(record) = self.window_service.window(window_id)
            && !record.maximized
            && !record.fullscreen
            && !self.window_service.is_resizing(window_id)
        {
            let client = crate::space_sync::client_area(self, window_id);
            let frame = if self.window_service.has_frame(window_id) {
                templar_window::frame_around(templar_window::Rect::new(
                    client.x, client.y, size.w, size.h,
                ))
            } else {
                templar_window::Rect::new(client.x, client.y, size.w, size.h)
            };
            self.window_service.set_geometry(window_id, frame);
            let client = crate::space_sync::client_area(self, window_id);
            let _ = window.configure(Some(rect_to_smithay(client)));
            crate::space_sync::sync_after_change(self);
            return;
        }
        let _ = window.configure(Some(Rectangle::new(current.loc, size)));
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<smithay::xwayland::xwm::X11Window>,
    ) {
        if window.is_override_redirect()
            && let Some(element) = window_element(self, &window)
        {
            self.space.map_element(element, geometry.loc, false);
        }
    }

    /// `_NET_WM_MOVERESIZE` with a resize direction: the X11 counterpart of
    /// `xdg_toplevel.resize`, and the same compositor-run grab
    /// (`XdgShellHandler::resize_request`). The X11 enum names a corner rather than a set of
    /// edges, so it is translated the same way the Wayland one is.
    fn resize_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _button: u32,
        resize_edge: ResizeEdge,
    ) {
        let Some(window_id) = self.window_id_of(&window) else {
            return;
        };
        let (left, right, top, bottom) = match resize_edge {
            ResizeEdge::Top => (false, false, true, false),
            ResizeEdge::Bottom => (false, false, false, true),
            ResizeEdge::Left => (true, false, false, false),
            ResizeEdge::Right => (false, true, false, false),
            ResizeEdge::TopLeft => (true, false, true, false),
            ResizeEdge::TopRight => (false, true, true, false),
            ResizeEdge::BottomLeft => (true, false, false, true),
            ResizeEdge::BottomRight => (false, true, false, true),
        };
        let seat = self.seat.clone();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        crate::input::start_resize(
            self,
            &seat,
            serial,
            window_id,
            templar_window::ResizeEdges {
                left,
                right,
                top,
                bottom,
            },
        );
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        let Some(surface) = window.wl_surface() else {
            return;
        };
        let Some(window_id) = self
            .windows
            .window_id_for(&smithay::reexports::wayland_server::Resource::id(&surface))
        else {
            return;
        };
        let seat = self.seat.clone();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        crate::input::start_drag(self, &seat, serial, window_id);
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Through the same path a Wayland client takes: `apply_fullscreen` sets the X11 property
        // *and* configures the surface to the output's rectangle, which is what was missing.
        if let Some(window_id) = self.window_id_of(&window) {
            crate::xdg_shell::apply_fullscreen(self, window_id, true);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(window_id) = self.window_id_of(&window) {
            crate::xdg_shell::apply_fullscreen(self, window_id, false);
        }
    }

    fn modal_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(window_id) = self.window_id_of(&window) {
            self.window_service.set_modal(window_id, true);
            crate::space_sync::sync_after_change(self);
        }
    }

    fn unmodal_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(window_id) = self.window_id_of(&window) {
            self.window_service.set_modal(window_id, false);
            crate::space_sync::sync_after_change(self);
        }
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        xwayland_selection_allowed()
    }

    /// `_NET_WM_NAME`/`WM_NAME` changing is delivered here rather than as its own callback —
    /// COMPOSITOR.md's `xdg_toplevel.set_title` row's XWayland counterpart.
    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if property != WmWindowProperty::Title {
            return;
        }
        if let Some(window_id) = self.window_id_of(&window) {
            self.window_service.set_title(window_id, window.title());
            crate::space_sync::sync_after_change(self);
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        self.xwayland_display = None;
    }
}

fn rect_to_smithay(rect: templar_window::Rect) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((rect.x, rect.y)),
        Size::from((rect.width, rect.height)),
    )
}

fn window_element<B: Backend + 'static>(
    state: &TemplarCompState<B>,
    window: &X11Surface,
) -> Option<Window> {
    state
        .space
        .elements()
        .find(|element| element.x11_surface() == Some(window))
        .cloned()
}

impl<B: Backend + 'static> TemplarCompState<B> {
    fn window_id_of(&self, window: &X11Surface) -> Option<templar_window::WindowId> {
        let surface = window.wl_surface()?;
        self.windows
            .window_id_for(&smithay::reexports::wayland_server::Resource::id(&surface))
    }

    fn remove_x11_window(&mut self, window: &X11Surface) {
        if let Some(window_id) = self.windows.remove_x11(window.window_id()) {
            crate::space_sync::on_window_destroyed(self, window_id);
        }
    }

    /// Spawn XWayland and attach the X11 window manager once it reports ready. Called once from
    /// `main.rs` after the Wayland socket and event loop are already up — matching every backend
    /// in every Smithay-based compositor this crate was modelled on (`anvil`, `cosmic-comp`).
    pub fn start_xwayland(&mut self, loop_handle: &LoopHandle<'static, Self>) {
        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            std::iter::empty::<String>(),
            true,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            |_| (),
        ) {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, "failed to spawn XWayland; XWayland apps will not work");
                return;
            }
        };

        let display_handle = self.display_handle.clone();
        let result =
            loop_handle.insert_source(xwayland, move |event, (), state: &mut Self| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => match X11Wm::start_wm(
                    state.loop_handle.clone(),
                    &display_handle,
                    x11_socket,
                    client.clone(),
                ) {
                    Ok(mut wm) => {
                        let cursor = solid_cursor_pixels();
                        let _ = wm.set_cursor(
                            &cursor,
                            Size::from((1u16, 1u16)),
                            Point::from((0u16, 0u16)),
                        );
                        state.xwm = Some(wm);
                        state.xwayland_display = Some(display_number);
                    }
                    Err(error) => tracing::warn!(%error, "failed to attach the X11 window manager"),
                },
                XWaylandEvent::Error => tracing::warn!("XWayland crashed on startup"),
            });
        if let Err(error) = result {
            tracing::error!(%error, "failed to insert the XWayland event source");
        }
    }
}

/// One fully-opaque black pixel — the minimal valid cursor image, standing in for a real cursor
/// theme (`src/chrome.rs`'s "no font/raster dependency yet" scope applies to cursors too).
fn solid_cursor_pixels() -> [u8; 4] {
    [0, 0, 0, 255]
}

/// Whether XWayland selections bridge to native Wayland ones. One clipboard for the whole
/// session: an X11 program's copy is a Wayland client's paste, and the reverse.
fn xwayland_selection_allowed() -> bool {
    true
}

#[cfg(test)]
mod tests {
    /// The bridge stays on: X11 programs share the session clipboard rather than a private
    /// X11-only one. (`allow_selection_access` answers through this.)
    #[test]
    fn xwayland_selections_bridge_to_wayland() {
        assert!(super::xwayland_selection_allowed());
    }
}
