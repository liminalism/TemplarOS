//! Window previews for parked-session tiles, over the standard capture protocols the compositor
//! offers the trusted shell alone (`compositor/lege-comp/src/screencopy.rs`):
//! `ext-foreign-toplevel-list-v1` for a handle per window, `ext-image-capture-source-v1` to make
//! a capture source of one, and `ext-image-copy-capture-v1` for a session over it that fills shm
//! buffers this process allocates. All of it rides the trusted-shell connection's own event queue
//! (`crate::wayland_bridge`), which is why the `Dispatch` impls here are on `BridgeState`.
//!
//! A preview's buffer is an anonymous file (`tempfile`) the compositor writes into and this
//! process reads back with `pread` after `ready` — no mmap, so this module, like everything but
//! `wayland_bridge`, is free of `unsafe`. The pacing policy (which windows, how often) is the
//! shell app's (`surface/app.rs`); this module only knows how to ask for one frame and hand the
//! pixels back keyed by the window's foreign-toplevel identifier.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use pixelkit_raster::Bitmap;
use wayland_backend::client::ObjectData;
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_shm::{self, WlShm},
    wl_shm_pool::{self, WlShmPool},
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::{
        self, ExtForeignToplevelImageCaptureSourceManagerV1,
    },
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use crate::wayland_bridge::BridgeState;

/// One window's handle from `ext-foreign-toplevel-list-v1`.
#[derive(Debug)]
struct Handle {
    proxy: ExtForeignToplevelHandleV1,
    identifier: Option<String>,
}

/// An shm buffer shared with the compositor: a pool file, the `wl_buffer` over it, and its
/// dimensions. Captures read it (`read`); the lock screen writes it (`write`).
#[derive(Debug)]
pub(crate) struct ShmBuffer {
    file: File,
    pool: WlShmPool,
    pub(crate) buffer: WlBuffer,
    pub(crate) width: u32,
    pub(crate) height: u32,
    stride: u32,
}

impl ShmBuffer {
    pub(crate) fn destroy(&self) {
        self.buffer.destroy();
        self.pool.destroy();
    }

    /// Read the buffer back as a bitmap: the compositor wrote `Argb8888`, little-endian, which
    /// is `0xAARRGGBB` per `u32` — pixelkit's own packing — with alpha forced opaque, since a
    /// window preview is a thumbnail, not a compositing source.
    /// Fill the pool with `pixels`, which are pixelkit's `0x00RRGGBB` — written out as
    /// `Argb8888` little-endian (`0xAARRGGBB` per `u32`) with alpha forced opaque, because a
    /// lock screen that lets the desktop show through is not a lock screen.
    pub(crate) fn write(&self, pixels: &[u32]) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        let mut row = vec![0u8; self.stride as usize];
        for y in 0..self.height {
            let start = (y * self.width) as usize;
            for x in 0..self.width as usize {
                let pixel = pixels.get(start + x).copied().unwrap_or(0) | 0xff00_0000;
                row[x * 4..x * 4 + 4].copy_from_slice(&pixel.to_le_bytes());
            }
            self.file
                .write_all_at(&row, u64::from(y) * u64::from(self.stride))
                .context("writing a lock-screen row")?;
        }
        Ok(())
    }

    fn read(&self) -> Result<Bitmap> {
        let mut row = vec![0u8; self.stride as usize];
        let mut pixels = Vec::with_capacity((self.width * self.height) as usize);
        for y in 0..self.height {
            self.file
                .read_exact_at(&mut row, u64::from(y) * u64::from(self.stride))
                .context("reading a preview row")?;
            for x in 0..self.width as usize {
                let b = &row[x * 4..x * 4 + 4];
                pixels.push(0xff00_0000 | u32::from_le_bytes([b[0], b[1], b[2], 0]));
            }
        }
        Ok(Bitmap {
            width: self.width,
            height: self.height,
            pixels,
        })
    }
}

/// One capture session: created per window on first request, kept until the compositor stops
/// it or the window closes.
#[derive(Debug)]
struct Session {
    source: ExtImageCaptureSourceV1,
    proxy: ExtImageCopyCaptureSessionV1,
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    /// `done` arrived: the constraints are complete and a frame may be requested.
    constrained: bool,
    buffer: Option<ShmBuffer>,
    /// A frame in flight, until `ready` or `failed`.
    frame: Option<ExtImageCopyCaptureFrameV1>,
    stopped: bool,
}

impl Session {
    fn teardown(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        self.proxy.destroy();
        self.source.destroy();
        self.stopped = true;
    }
}

/// Everything the previews need from the registry, and every live session.
#[derive(Debug, Default)]
pub struct PreviewState {
    shm: Option<WlShm>,
    list: Option<ExtForeignToplevelListV1>,
    source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    handles: HashMap<wayland_backend::client::ObjectId, Handle>,
    sessions: HashMap<String, Session>,
    ready: Vec<(String, Bitmap)>,
}

impl PreviewState {
    /// The `wl_shm` this connection bound, shared with the lock screen: one binding of a global
    /// is enough, and a second would be two objects for one thing.
    pub(crate) fn shm(&self) -> Option<&WlShm> {
        self.shm.as_ref()
    }

    /// Bind whichever of the four globals `interface` is, if it is one of them.
    pub fn bind_global(
        &mut self,
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        qh: &QueueHandle<BridgeState>,
    ) {
        match interface {
            "wl_shm" => {
                self.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ()));
            }
            "ext_foreign_toplevel_list_v1" => {
                self.list = Some(registry.bind::<ExtForeignToplevelListV1, _, _>(
                    name,
                    version.min(1),
                    qh,
                    (),
                ));
            }
            "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                self.source_manager = Some(
                    registry.bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ),
                );
            }
            "ext_image_copy_capture_manager_v1" => {
                self.capture_manager = Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                    name,
                    version.min(1),
                    qh,
                    (),
                ));
            }
            _ => {}
        }
    }

    /// Whether the compositor offered the capture globals at all.
    pub fn available(&self) -> bool {
        self.shm.is_some()
            && self.list.is_some()
            && self.source_manager.is_some()
            && self.capture_manager.is_some()
    }

    /// Ask for one frame of the window whose foreign-toplevel identifier is `identifier`. A
    /// session is created on first request; a request while one frame is in flight, or before
    /// the session's constraints arrived, is dropped — the caller asks again next time.
    pub fn request(&mut self, identifier: &str, qh: &QueueHandle<BridgeState>) -> Result<()> {
        if !self.available() {
            return Err(anyhow!(
                "the compositor offers no capture globals on this connection"
            ));
        }
        if !self.sessions.contains_key(identifier) {
            let handle = self
                .handles
                .values()
                .find(|handle| handle.identifier.as_deref() == Some(identifier))
                .ok_or_else(|| anyhow!("no foreign toplevel handle for {identifier}"))?;
            let source_manager = self.source_manager.as_ref().expect("checked above");
            let capture_manager = self.capture_manager.as_ref().expect("checked above");
            let source = source_manager.create_source(&handle.proxy, qh, ());
            let session = capture_manager.create_session(
                &source,
                ext_image_copy_capture_manager_v1::Options::empty(),
                qh,
                identifier.to_owned(),
            );
            self.sessions.insert(
                identifier.to_owned(),
                Session {
                    source,
                    proxy: session,
                    size: None,
                    formats: Vec::new(),
                    constrained: false,
                    buffer: None,
                    frame: None,
                    stopped: false,
                },
            );
            return Ok(());
        }
        let shm = self.shm.as_ref().expect("checked above");
        let session = self.sessions.get_mut(identifier).expect("inserted above");
        if session.stopped {
            self.sessions.remove(identifier);
            return Err(anyhow!(
                "the compositor stopped the session for {identifier}"
            ));
        }
        if !session.constrained || session.frame.is_some() {
            return Ok(());
        }
        let (width, height) = session
            .size
            .ok_or_else(|| anyhow!("no buffer size for {identifier}"))?;
        let format = if session.formats.contains(&wl_shm::Format::Argb8888) {
            wl_shm::Format::Argb8888
        } else if session.formats.contains(&wl_shm::Format::Xrgb8888) {
            wl_shm::Format::Xrgb8888
        } else {
            return Err(anyhow!(
                "no shm format this process can read for {identifier}"
            ));
        };
        if session
            .buffer
            .as_ref()
            .is_none_or(|b| (b.width, b.height) != (width, height))
        {
            if let Some(old) = session.buffer.take() {
                old.destroy();
            }
            session.buffer = Some(allocate(shm, width, height, format, qh)?);
        }
        let buffer = session.buffer.as_ref().expect("allocated above");
        let frame = session.proxy.create_frame(qh, identifier.to_owned());
        frame.attach_buffer(&buffer.buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        session.frame = Some(frame);
        Ok(())
    }

    /// Previews that arrived since the last call, keyed by identifier.
    pub fn take_ready(&mut self) -> Vec<(String, Bitmap)> {
        std::mem::take(&mut self.ready)
    }

    pub fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Drop the session of a window that closed.
    pub fn forget(&mut self, identifier: &str) {
        if let Some(mut session) = self.sessions.remove(identifier) {
            session.teardown();
        }
    }
}

pub(crate) fn allocate(
    shm: &WlShm,
    width: u32,
    height: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<BridgeState>,
) -> Result<ShmBuffer> {
    let stride = width * 4;
    let size = u64::from(stride) * u64::from(height);
    let file = tempfile::tempfile().context("creating a preview pool file")?;
    file.set_len(size).context("sizing the preview pool")?;
    let pool = shm.create_pool(
        std::os::fd::AsFd::as_fd(&file),
        i32::try_from(size).context("preview pool too large")?,
        qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        (),
    );
    Ok(ShmBuffer {
        file,
        pool,
        buffer,
        width,
        height,
        stride,
    })
}

// --- dispatch: the foreign toplevel list ---------------------------------------------------

impl Dispatch<ExtForeignToplevelListV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.previews.handles.insert(
                toplevel.id(),
                Handle {
                    proxy: toplevel,
                    identifier: None,
                },
            );
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        match opcode {
            ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => {
                qh.make_data::<ExtForeignToplevelHandleV1, ()>(())
            }
            other => panic!("ext_foreign_toplevel_list_v1: unexpected event opcode {other}"),
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                if let Some(handle) = state.previews.handles.get_mut(&proxy.id()) {
                    handle.identifier = Some(identifier);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                if let Some(handle) = state.previews.handles.remove(&proxy.id()) {
                    if let Some(identifier) = handle.identifier {
                        state.previews.forget(&identifier);
                    }
                    handle.proxy.destroy();
                }
            }
            _ => {}
        }
    }
}

// --- dispatch: capture sources and sessions -------------------------------------------------

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _event: ext_foreign_toplevel_image_capture_source_manager_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: ext_image_capture_source_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: ext_image_copy_capture_manager_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, String> for BridgeState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        identifier: &String,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(session) = state.previews.sessions.get_mut(identifier) else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                session.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(format),
            } => {
                session.formats.push(format);
            }
            ext_image_copy_capture_session_v1::Event::Done => session.constrained = true,
            ext_image_copy_capture_session_v1::Event::Stopped => session.teardown(),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, String> for BridgeState {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        identifier: &String,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(session) = state.previews.sessions.get_mut(identifier) else {
            return;
        };
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                if let Some(buffer) = &session.buffer {
                    match buffer.read() {
                        Ok(bitmap) => state.previews.ready.push((identifier.clone(), bitmap)),
                        Err(error) => tracing::warn!(%error, "reading a preview back"),
                    }
                }
                proxy.destroy();
                session.frame = None;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                tracing::debug!(?reason, %identifier, "preview frame failed");
                proxy.destroy();
                session.frame = None;
            }
            _ => {}
        }
    }
}

// --- dispatch: shm objects (no events this process acts on) ---------------------------------

impl Dispatch<WlShm, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: wl_shm::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: wl_shm_pool::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: wl_buffer::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
