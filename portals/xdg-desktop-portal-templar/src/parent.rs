//! Request-local native transient parenting. No identity or activation authority is inferred.
#![allow(unsafe_code)]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::{
    io::Write,
    os::{fd::AsRawFd, unix::net::UnixStream},
    thread::JoinHandle,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    protocol::{
        wl_callback::{self, WlCallback},
        wl_registry::{self, WlRegistry},
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::foreign::zv2::client::{
    zxdg_imported_v2::{self, ZxdgImportedV2},
    zxdg_importer_v2::{self, ZxdgImporterV2},
};
use x11rb::{
    connection::Connection as _,
    protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask},
    wrapper::ConnectionExt as _,
};

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Parent {
    #[default]
    None,
    Invalid,
    X11(u32),
    Wayland(String),
}
impl Parent {
    pub fn parse(value: &str) -> Self {
        if value.is_empty() {
            return Self::None;
        }
        if value.len() > 264 {
            return Self::Invalid;
        }
        if let Some(id) = value.strip_prefix("x11:") {
            return if !id.is_empty() && id.len() <= 8 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                u32::from_str_radix(id, 16)
                    .ok()
                    .filter(|id| *id != 0)
                    .map_or(Self::Invalid, Self::X11)
            } else {
                Self::Invalid
            };
        }
        if let Some(handle) = value.strip_prefix("wayland:")
            && !handle.is_empty()
            && handle.len() <= 256
            && handle.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return Self::Wayland(handle.to_owned());
        }
        Self::Invalid
    }
}

struct Reader {
    stop: UnixStream,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.stop.write_all(&[1]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn ready(fd: i32, stop: i32) -> bool {
    let mut fds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: two initialized pollfds, borrowed open descriptors, no timeout/idle polling.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return result > 0 && fds[1].revents == 0 && fds[0].revents & libc::POLLIN != 0;
    }
}
struct Foreign {
    importer: Option<ZxdgImporterV2>,
    imported: Option<ZxdgImportedV2>,
    child: WlSurface,
    handle: String,
    barrier: bool,
    phase: u8,
    gone: bool,
}
impl Dispatch<WlRegistry, ()> for Foreign {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == "zxdg_importer_v2"
        {
            state.importer = Some(registry.bind(name, 1, qh, ()));
        }
    }
}
impl Dispatch<ZxdgImporterV2, ()> for Foreign {
    fn event(
        _: &mut Self,
        _: &ZxdgImporterV2,
        _: zxdg_importer_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZxdgImportedV2, ()> for Foreign {
    fn event(
        state: &mut Self,
        _: &ZxdgImportedV2,
        event: zxdg_imported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_imported_v2::Event::Destroyed = event {
            state.gone = true;
        }
    }
}
impl Dispatch<WlCallback, ()> for Foreign {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.barrier = true;
    }
}
pub struct Binding {
    native: Native,
    _reader: Reader,
}
enum Native {
    Wayland {
        connection: Connection,
        queue: EventQueue<Foreign>,
        state: Foreign,
        registry: WlRegistry,
    },
    X11(std::sync::Arc<std::sync::atomic::AtomicBool>),
}
impl Binding {
    /// All raw-pointer wrapping happens while the borrowed native window is live. The guest
    /// backend never closes winit's display; on_exit drops this binding before window retirement.
    pub fn connect(
        parent: &Parent,
        window: &winit::window::Window,
        wake: pixelkit_windowing::Waker,
    ) -> anyhow::Result<Option<Self>> {
        match parent {
            Parent::None => Ok(None),
            Parent::Invalid => {
                anyhow::bail!("Malformed parent handle; this dialog is independent.")
            }
            Parent::X11(parent) => {
                let raw = window.window_handle()?.as_raw();
                let child = match raw {
                    RawWindowHandle::Xlib(handle) => u32::try_from(handle.window)?,
                    RawWindowHandle::Xcb(handle) => handle.window.get(),
                    _ => anyhow::bail!(
                        "X11 parent on a different native backend; this dialog is independent."
                    ),
                };
                let (connection, _) = x11rb::connect(None)?;
                // Separate X11 connection isolates BadWindow errors; XIDs are server-global.
                connection.get_window_attributes(*parent)?.reply()?;
                connection
                    .change_window_attributes(
                        *parent,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
                    )?
                    .check()?;
                let atom = connection
                    .intern_atom(false, b"WM_TRANSIENT_FOR")?
                    .reply()?
                    .atom;
                connection
                    .change_property32(
                        x11rb::protocol::xproto::PropMode::REPLACE,
                        child,
                        atom,
                        AtomEnum::WINDOW,
                        &[*parent],
                    )?
                    .check()?;
                connection.flush()?;
                let readback = connection
                    .get_property(false, child, atom, AtomEnum::WINDOW, 0, 1)?
                    .reply()?;
                anyhow::ensure!(
                    readback.value32().and_then(|mut ids| ids.next()) == Some(*parent),
                    "Native transient parent readback failed; this dialog is independent."
                );
                let gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (stop, stopped) = UnixStream::pair()?;
                let lost = gone.clone();
                let parent = *parent;
                let thread = std::thread::Builder::new()
                    .name("portal-x11-parent".into())
                    .spawn(move || {
                        loop {
                            match connection.poll_for_event() {
                                Ok(Some(x11rb::protocol::Event::DestroyNotify(event)))
                                    if event.window == parent =>
                                {
                                    if let Ok(cookie) = connection.delete_property(child, atom) {
                                        let _ = cookie.check();
                                    }
                                    let _ = connection.flush();
                                    lost.store(true, std::sync::atomic::Ordering::Release);
                                    wake.wake();
                                    break;
                                }
                                Ok(Some(_)) => continue,
                                Ok(None) => {
                                    if !ready(connection.stream().as_raw_fd(), stopped.as_raw_fd())
                                    {
                                        break;
                                    }
                                }
                                Err(_) => {
                                    lost.store(true, std::sync::atomic::Ordering::Release);
                                    wake.wake();
                                    break;
                                }
                            }
                        }
                    })?;
                tracing::info!(
                    backend = "x11",
                    "native transient parent verified by WM_TRANSIENT_FOR readback; no activation inferred"
                );
                Ok(Some(Self {
                    native: Native::X11(gone),
                    _reader: Reader {
                        stop,
                        thread: Some(thread),
                    },
                }))
            }
            Parent::Wayland(handle) => {
                let RawDisplayHandle::Wayland(display) = window.display_handle()?.as_raw() else {
                    anyhow::bail!(
                        "Wayland parent on a different native backend; this dialog is independent."
                    )
                };
                let RawWindowHandle::Wayland(surface) = window.window_handle()?.as_raw() else {
                    anyhow::bail!("Native Wayland surface unavailable; this dialog is independent.")
                };
                // SAFETY: live winit display/surface; sys guest backend shares, never owns, display.
                let backend = unsafe {
                    wayland_backend::client::Backend::from_foreign_display(
                        display.display.as_ptr().cast(),
                    )
                };
                let connection = Connection::from_backend(backend);
                let queue = connection.new_event_queue::<Foreign>();
                let qh = queue.handle();
                let registry = connection.display().get_registry(&qh, ());
                connection.display().sync(&qh, ());
                // SAFETY: validated native wl_surface pointer borrowed from live window.
                let id = unsafe {
                    wayland_backend::client::ObjectId::from_ptr(
                        WlSurface::interface(),
                        surface.surface.as_ptr().cast(),
                    )
                }?;
                let child = WlSurface::from_id(&connection, id)?;
                let state = Foreign {
                    importer: None,
                    imported: None,
                    child,
                    handle: handle.clone(),
                    barrier: false,
                    phase: 0,
                    gone: false,
                };
                connection.flush()?;
                let (stop, stopped) = UnixStream::pair()?;
                let reader = connection.clone();
                let thread = std::thread::Builder::new()
                    .name("portal-wayland-parent".into())
                    .spawn(move || {
                        loop {
                            let count = if let Some(guard) = reader.prepare_read() {
                                if !ready(guard.connection_fd().as_raw_fd(), stopped.as_raw_fd()) {
                                    break;
                                }
                                match guard.read() {
                                    Ok(count) => count,
                                    Err(wayland_backend::client::WaylandError::Io(error))
                                        if error.kind() == std::io::ErrorKind::WouldBlock =>
                                    {
                                        0
                                    }
                                    Err(_) => {
                                        wake.wake();
                                        break;
                                    }
                                }
                            } else {
                                match reader.backend().dispatch_inner_queue() {
                                    Ok(count) => count,
                                    Err(_) => {
                                        wake.wake();
                                        break;
                                    }
                                }
                            };
                            if count > 0 {
                                wake.wake();
                            }
                        }
                    })?;
                Ok(Some(Self {
                    native: Native::Wayland {
                        connection,
                        queue,
                        state,
                        registry,
                    },
                    _reader: Reader {
                        stop,
                        thread: Some(thread),
                    },
                }))
            }
        }
    }
    /// Nonblocking registry/import barriers keep cancellation and the 300-second request
    /// deadline actionable even if the compositor stops replying. No per-frame allocation.
    pub fn poll(&mut self) -> Result<bool, &'static str> {
        match &mut self.native {
            Native::X11(gone) => {
                if gone.load(std::sync::atomic::Ordering::Acquire) {
                    Err("Requesting window closed; this dialog is independent.")
                } else {
                    Ok(true)
                }
            }
            Native::Wayland {
                connection,
                queue,
                state,
                ..
            } => {
                queue
                    .dispatch_pending(state)
                    .map_err(|_| "Parent connection lost; this dialog is independent.")?;
                if state.gone {
                    return Err(
                        "Parent handle expired, closed or rejected; this dialog is independent.",
                    );
                }
                if state.barrier {
                    state.barrier = false;
                    match state.phase {
                        0 => {
                            let Some(importer) = &state.importer else {
                                return Err(
                                    "Compositor has no xdg-foreign v2 importer; this dialog is independent.",
                                );
                            };
                            let qh = queue.handle();
                            state.imported = Some(importer.import_toplevel(
                                std::mem::take(&mut state.handle),
                                &qh,
                                (),
                            ));
                            connection.display().sync(&qh, ());
                            state.phase = 1;
                        }
                        1 => {
                            state.imported.as_ref().unwrap().set_parent_of(&state.child);
                            state.phase = 2;
                            tracing::info!(
                                backend = "wayland",
                                "live xdg-foreign parent imported and child submitted; no activation inferred"
                            );
                        }
                        _ => {}
                    }
                    connection
                        .flush()
                        .map_err(|_| "Parent connection lost; this dialog is independent.")?;
                }
                Ok(state.phase == 2)
            }
        }
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        if let Native::Wayland {
            connection,
            state,
            registry,
            ..
        } = &self.native
        {
            if let Some(imported) = &state.imported {
                imported.destroy();
            }
            if let Some(importer) = &state.importer {
                importer.destroy();
            }
            let _ = connection.backend().destroy_object(&registry.id());
            let _ = connection.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_is_bounded_and_does_not_cross_protocols() {
        assert_eq!(Parent::parse(""), Parent::None);
        assert_eq!(Parent::parse("x11:7f"), Parent::X11(127));
        assert_eq!(
            Parent::parse("wayland:opaque-token"),
            Parent::Wayland("opaque-token".into())
        );
        for bad in [
            "x11:0",
            "x11:fffffffff",
            "x11:-1",
            "wayland:",
            "wayland:a\n",
            "unknown:a",
        ] {
            assert_eq!(Parent::parse(bad), Parent::Invalid);
        }
        assert_eq!(
            Parent::parse(&format!("wayland:{}", "a".repeat(257))),
            Parent::Invalid
        );
    }
}
