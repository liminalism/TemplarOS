//! Window previews for visible Home work-card wells, over the standard capture protocols the compositor
//! offers the trusted shell alone (`compositor/templar-comp/src/screencopy.rs`):
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

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use pixelkit_raster::Bitmap;
use templar_window::WindowId;
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
    /// Row-local native byte packing scratch, reused across Home frames (not full-output coverage).
    write_row: Vec<u8>,
    /// `true` once `wl_buffer.release` has arrived, or before the first attach. Writing a buffer
    /// the compositor still holds is a protocol error.
    released: Arc<AtomicBool>,
}

impl ShmBuffer {
    pub(crate) fn destroy(&self) {
        self.buffer.destroy();
        self.pool.destroy();
    }

    pub(crate) fn released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }

    pub(crate) fn mark_busy(&self) {
        self.released.store(false, Ordering::Release);
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

    /// Write `pixels` with a per-pixel alpha taken from `coverage`, premultiplied.
    ///
    /// pixelkit's buffer is `0x00RRGGBB` and has nowhere to put coverage, so a surface that wants
    /// to be a shape rather than a rectangle has to render that shape a second time — white on
    /// black — and hand the result here. The blue channel is the one read: the mask is greyscale,
    /// so any channel would do, and blue is the low byte.
    ///
    /// `Argb8888` on Wayland is **premultiplied**, so the colour channels are scaled by the
    /// coverage. Skipping that is what makes a half-covered edge glow: the compositor would take
    /// the full-strength colour and blend it at half weight over the desktop *and* over itself.
    pub(crate) fn write_masked(&self, pixels: &[u32], coverage: &[u32]) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        let mut row = vec![0u8; self.stride as usize];
        for y in 0..self.height {
            let start = (y * self.width) as usize;
            for x in 0..self.width as usize {
                let colour = pixels.get(start + x).copied().unwrap_or(0);
                let alpha = u8::try_from(coverage.get(start + x).copied().unwrap_or(0) & 0xff)
                    .unwrap_or(0xff);
                let premultiply = |channel: u32| {
                    u32::from(
                        (u16::try_from(channel & 0xff).unwrap_or(0) * u16::from(alpha) / 255) as u8,
                    )
                };
                let pixel = (u32::from(alpha) << 24)
                    | (premultiply(colour >> 16) << 16)
                    | (premultiply(colour >> 8) << 8)
                    | premultiply(colour);
                row[x * 4..x * 4 + 4].copy_from_slice(&pixel.to_le_bytes());
            }
            self.file
                .write_all_at(&row, u64::from(y) * u64::from(self.stride))
                .context("writing a masked row")?;
        }
        Ok(())
    }

    /// Copy already-premultiplied `0xAARRGGBB` pixels without a second coverage buffer.
    pub(crate) fn write_premultiplied(&mut self, pixels: &[u32]) -> Result<()> {
        let expected = self.width as usize * self.height as usize;
        anyhow::ensure!(
            pixels.len() == expected,
            "premultiplied surface pixel count differs"
        );
        self.write_row.resize(self.stride as usize, 0);
        for (y, pixels) in pixels.chunks_exact(self.width as usize).enumerate() {
            for (bytes, pixel) in self.write_row.chunks_exact_mut(4).zip(pixels) {
                bytes.copy_from_slice(&pixel.to_le_bytes());
            }
            self.file
                .write_all_at(&self.write_row, y as u64 * u64::from(self.stride))
                .context("writing a premultiplied surface row")?;
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

/// One capture session, retired whenever the visible identifier set changes so both
/// its generation and adaptive aggregate-pool constraints remain current.
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
    /// Identifiers with capture demand, once computed. `None` files everything, as before;
    /// `Some` drops stale arrivals for retired windows instead of filing them.
    demanded_identifiers: Option<HashSet<String>>,
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

    pub fn pending(&self, identifier: &str) -> bool {
        self.sessions.get(identifier).is_some_and(|session| {
            !session.stopped && (!session.constrained || session.frame.is_some())
        })
    }

    /// Ask for one frame of this foreign-toplevel identifier. First use creates a
    /// session; its completed constraints start the request. An in-flight frame is
    /// never duplicated.
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

    /// Previews that arrived since the last call, keyed by identifier. Arrivals for
    /// windows whose demand retired while the frame was in flight are dropped, not filed:
    /// a stale result never repaints, never schedules another capture, and never retains
    /// a pool — its session was already torn down by [`Self::set_demand`].
    pub fn take_ready(&mut self) -> Vec<(String, Bitmap)> {
        let ready = std::mem::take(&mut self.ready);
        match &self.demanded_identifiers {
            None => ready,
            Some(demanded) => ready
                .into_iter()
                .filter(|(identifier, _)| demanded.contains(identifier))
                .collect(),
        }
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

    /// Install the Home card wells' capture demand, keyed by foreign-toplevel identifier.
    /// Sessions for windows that left the set are torn down at once — an in-flight frame is
    /// destroyed rather than awaited, and its shm pool goes with it — so retired demand keeps
    /// no protocol objects and no buffers. Stale arrivals for them are dropped by
    /// [`Self::take_ready`]: they never repaint, never schedule another capture, and never
    /// retain a pool. `None` is a shell that has not computed demand yet: file everything,
    /// as before.
    pub fn set_demand(&mut self, demanded: Option<HashSet<String>>) {
        if self.demanded_identifiers != demanded {
            // The aggregate pool allowance depends on the whole visible set. New sessions
            // receive fresh constraints, and proxy identity fences late events from old ones.
            for (_, mut session) in self.sessions.drain() {
                session.teardown();
            }
            self.ready.clear();
        }
        self.demanded_identifiers = demanded;
    }
}

// --- visible-only demand ---------------------------------------------------------------------

/// Aggregate retained-preview budget, in bytes: at most this across every Home bitmap and its
/// shm twin. A 512×512 RGBA bitmap alone costs 1 MiB; every copy counts, so the shell counts
/// each retained bitmap twice (pixels plus pool).
pub const AGGREGATE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// A rectangle in Home-surface buffer pixels: a card well, the field bounds, or an occluder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Well {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Well {
    fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }

    /// Whether any pixel of `self` survives clipping to `bounds`.
    fn clipped_visible(&self, bounds: &Well) -> bool {
        self.w > 0
            && self.h > 0
            && self.x < bounds.right()
            && self.right() > bounds.x
            && self.y < bounds.bottom()
            && self.bottom() > bounds.y
    }

    pub fn visible_in(&self, bounds: Well, occluders: impl Iterator<Item = Well> + Clone) -> bool {
        if !self.clipped_visible(&bounds) {
            return false;
        }
        let clipped = Well {
            x: self.x.max(bounds.x),
            y: self.y.max(bounds.y),
            w: self.right().min(bounds.right()) - self.x.max(bounds.x),
            h: self.bottom().min(bounds.bottom()) - self.y.max(bounds.y),
        };
        clipped.visible_after(occluders)
    }

    /// Test the union of occluders using vertical slabs, without allocating a region list.
    fn visible_after(&self, occluders: impl Iterator<Item = Well> + Clone) -> bool {
        let mut x = self.x;
        while x < self.right() {
            let next_x = occluders
                .clone()
                .flat_map(|rect| [rect.x, rect.right()])
                .filter(|edge| *edge > x)
                .fold(self.right(), i32::min);
            let mut y = self.y;
            while y < self.bottom() {
                let next_y = occluders
                    .clone()
                    .filter(|rect| rect.x <= x && rect.right() >= next_x && rect.y <= y)
                    .map(|rect| rect.bottom())
                    .fold(y, i32::max);
                if next_y == y {
                    return true;
                }
                y = next_y;
            }
            x = next_x;
        }
        false
    }
}

/// Which windows have card wells with visible pixels: clipped to `bounds` with nonzero area
/// and not fully covered by an occluder. Partial occlusion keeps demand — the well still has
/// visible pixels. Overflow-only windows never enter `wells`: a window with no card of its
/// own demands nothing, however many pixels the "+N windows" card has.
pub fn visible_wells(
    wells: &[(WindowId, Well)],
    bounds: Well,
    occluders: &[Well],
) -> Vec<WindowId> {
    wells
        .iter()
        .filter(|(_, well)| well.visible_in(bounds, occluders.iter().copied()))
        .map(|(window, _)| *window)
        .collect()
}

/// The shell's capture demand over window ids, fenced by a generation that moves on every
/// visibility change (and on lock). The compositor ignores a generation that moves backwards.
#[derive(Debug, Default)]
pub struct DemandTracker {
    generation: u32,
    demanded: HashSet<WindowId>,
}

impl DemandTracker {
    /// The windows currently demanded.
    pub fn demanded(&self) -> &HashSet<WindowId> {
        &self.demanded
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Install the visible set. Returns whether demand changed — only then does the shell
    /// owe the compositor a `set_preview_demand`.
    pub fn set_visible(&mut self, visible: &[WindowId]) -> bool {
        let visible: HashSet<WindowId> = visible.iter().copied().collect();
        if visible == self.demanded {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        self.demanded = visible;
        true
    }

    pub fn is_demanded(&self, window: WindowId) -> bool {
        self.demanded.contains(&window)
    }

    /// The wire payload for `set_preview_demand`: native-endian uint32 window ids, sorted so
    /// the same set always sends the same bytes. Ids outside uint32 range are withheld —
    /// only validated ids travel.
    pub fn encode(&self) -> Vec<u8> {
        encode_demand_ids(&self.demanded.iter().copied().collect::<Vec<_>>())
    }
}

/// Encode window ids for `set_preview_demand`: native-endian uint32, sorted, with ids
/// outside uint32 range withheld. The one encoder both the tracker and the bridge use.
pub fn encode_demand_ids(windows: &[WindowId]) -> Vec<u8> {
    let mut ids: Vec<u32> = windows
        .iter()
        .filter_map(|id| u32::try_from(id.0).ok())
        .collect();
    ids.sort_unstable();
    ids.iter().flat_map(|id| id.to_ne_bytes()).collect()
}

/// Request order for one refresh, stalest first: windows never captured lead, then the
/// least recently requested. Fair visible service with no extra state — the pacing map the
/// shell already keeps is the schedule.
pub fn stalest_first(
    windows: &[WindowId],
    last_request: &HashMap<WindowId, Instant>,
    now: Instant,
) -> Vec<WindowId> {
    let mut windows = windows.to_vec();
    windows.sort_by_key(|window| {
        std::cmp::Reverse(
            last_request
                .get(window)
                .and_then(|at| now.checked_duration_since(*at))
                .unwrap_or(Duration::MAX),
        )
    });
    windows
}

/// The shell's capture pacing for one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemandPacing {
    /// Icon and title only: never due.
    Off,
    /// One frame per window (re-armed by `force`, e.g. on parking).
    Static,
    /// Refreshed at `interval` while demanded.
    Live { interval: Duration },
}

/// Whether a demanded window has reached its visible-only deadline. The caller
/// excludes in-flight captures; the compositor holds an unchanged frame pending
/// rather than rendering/readback polling it again.
pub fn capture_due(
    pacing: DemandPacing,
    demanded: bool,
    has_bitmap: bool,
    last_request: Option<Instant>,
    now: Instant,
    force: bool,
) -> bool {
    if !demanded {
        return false;
    }
    match pacing {
        DemandPacing::Off => false,
        DemandPacing::Static => force || !has_bitmap,
        DemandPacing::Live { interval } => last_request.is_none_or(|at| {
            now.checked_duration_since(at)
                .is_none_or(|age| age >= interval)
        }),
    }
}

/// Charge twice the bitmap bytes: half the 16 MiB shell budget is retained pixels,
/// reserving the other half for at most 4 MiB of SHM pools plus 4 MiB of ready pixels.
pub fn retained_bitmap_bytes(bitmap: &Bitmap) -> usize {
    bitmap.width as usize * bitmap.height as usize * 4 * 2
}

/// Evict retained last frames over `budget`, undemanded first, then demanded, each in key
/// order so the same overage always evicts the same keys. On ordinary hide an already-built
/// last frame stays — under budget it is instant comeback, never a refresh — but memory
/// pressure, close and identity loss purge. Returns the evicted keys.
pub fn evict_over_budget(
    bitmaps: &mut HashMap<String, Bitmap>,
    demanded_keys: &HashSet<String>,
    budget: usize,
) -> Vec<String> {
    let retained: usize = bitmaps.values().map(retained_bitmap_bytes).sum();
    if retained <= budget {
        return Vec::new();
    }
    let mut keys: Vec<String> = bitmaps.keys().cloned().collect();
    keys.sort();
    let (undemanded, demanded): (Vec<String>, Vec<String>) = keys
        .into_iter()
        .partition(|key| !demanded_keys.contains(key));
    let mut evicted = Vec::new();
    let mut freed = 0;
    let overage = retained - budget;
    for key in undemanded.into_iter().chain(demanded) {
        if freed >= overage {
            break;
        }
        if let Some(bitmap) = bitmaps.remove(&key) {
            freed += retained_bitmap_bytes(&bitmap);
            evicted.push(key);
        }
    }
    evicted
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
    let released = Arc::new(AtomicBool::new(true));
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        Arc::clone(&released),
    );
    Ok(ShmBuffer {
        file,
        pool,
        buffer,
        width,
        height,
        stride,
        write_row: Vec::new(),
        released,
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
        proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        identifier: &String,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(session) = state.previews.sessions.get_mut(identifier) else {
            return;
        };
        if &session.proxy != proxy {
            return;
        }
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                session.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(format),
            } => {
                session.formats.push(format);
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                session.constrained = true;
                if state
                    .previews
                    .demanded_identifiers
                    .as_ref()
                    .is_some_and(|demand| demand.contains(identifier))
                {
                    if let Err(error) = state.previews.request(identifier, qh) {
                        tracing::debug!(%error, "starting constrained preview");
                    }
                }
            }
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
        if session.frame.as_ref() != Some(proxy) {
            return;
        }
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

impl Dispatch<WlBuffer, Arc<AtomicBool>> for BridgeState {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: wl_buffer::Event,
        released: &Arc<AtomicBool>,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            released.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn well(x: i32, y: i32, w: i32, h: i32) -> Well {
        Well { x, y, w, h }
    }

    #[test]
    fn hidden_wells_demand_nothing_visible_ones_demand() {
        let bounds = well(0, 0, 800, 600);
        // On-screen, partially clipped, fully offscreen, empty, and fully occluded.
        let wells = vec![
            (WindowId(1), well(10, 10, 100, 100)),
            (WindowId(2), well(750, 550, 100, 100)),
            (WindowId(3), well(900, 10, 100, 100)),
            (WindowId(4), well(10, 10, 0, 50)),
            (WindowId(5), well(10, 200, 100, 100)),
        ];
        let occluders = vec![well(0, 190, 800, 120)];
        let visible = visible_wells(&wells, bounds, &occluders);
        assert_eq!(visible, vec![WindowId(1), WindowId(2)]);
    }

    #[test]
    fn partial_occlusion_keeps_demand_only_full_coverage_removes_it() {
        let bounds = well(0, 0, 800, 600);
        let wells = vec![(WindowId(1), well(100, 100, 200, 200))];
        assert_eq!(
            visible_wells(&wells, bounds, &[well(150, 150, 50, 50)]),
            vec![WindowId(1)]
        );
        assert!(visible_wells(&wells, bounds, &[well(100, 100, 200, 200)]).is_empty());
    }

    #[test]
    fn combined_occluders_remove_demand_but_a_pixel_gap_preserves_it() {
        let bounds = well(0, 0, 800, 600);
        let wells = [(WindowId(1), well(100, 100, 200, 200))];
        assert!(
            visible_wells(
                &wells,
                bounds,
                &[well(100, 100, 100, 200), well(200, 100, 100, 200)]
            )
            .is_empty()
        );
        assert_eq!(
            visible_wells(
                &wells,
                bounds,
                &[well(100, 100, 99, 200), well(200, 100, 100, 200)]
            ),
            vec![WindowId(1)]
        );
    }

    #[test]
    fn hide_then_reveal_does_not_revive_an_undrained_old_frame() {
        let mut state = PreviewState::default();
        state.set_demand(Some(HashSet::from(["window".to_owned()])));
        state.ready.push(("window".to_owned(), bitmap(8)));
        state.set_demand(Some(HashSet::new()));
        state.set_demand(Some(HashSet::from(["window".to_owned()])));
        assert!(state.take_ready().is_empty());
    }

    #[test]
    fn demand_generation_moves_only_on_change_and_encodes_sorted_validated_ids() {
        let mut tracker = DemandTracker::default();
        assert_eq!(tracker.generation(), 0);
        assert!(tracker.set_visible(&[WindowId(2), WindowId(1)]));
        assert_eq!(tracker.generation(), 1);
        assert!(!tracker.set_visible(&[WindowId(1), WindowId(2)]));
        assert_eq!(tracker.generation(), 1);
        assert!(tracker.set_visible(&[]));
        assert_eq!(tracker.generation(), 2);
        assert!(!tracker.is_demanded(WindowId(1)));

        let mut tracker = DemandTracker::default();
        tracker.set_visible(&[WindowId(2), WindowId(1), WindowId(u64::from(u32::MAX) + 1)]);
        let encoded = tracker.encode();
        assert_eq!(encoded.len(), 8);
        let mut ids = [0u32; 2];
        ids[0] = u32::from_ne_bytes(encoded[0..4].try_into().expect("four bytes"));
        ids[1] = u32::from_ne_bytes(encoded[4..8].try_into().expect("four bytes"));
        assert_eq!(ids, [1, 2]);
    }

    #[test]
    fn hidden_is_zero_work_under_every_pacing() {
        let now = Instant::now();
        for pacing in [
            DemandPacing::Off,
            DemandPacing::Static,
            DemandPacing::Live {
                interval: Duration::from_millis(250),
            },
        ] {
            assert!(
                !capture_due(pacing, false, false, None, now, true),
                "undemanded is never due, even forced"
            );
        }
        assert!(!capture_due(
            DemandPacing::Off,
            true,
            false,
            None,
            now,
            true
        ));
        assert!(capture_due(
            DemandPacing::Static,
            true,
            false,
            None,
            now,
            false
        ));
        assert!(!capture_due(
            DemandPacing::Static,
            true,
            true,
            None,
            now,
            false
        ));
        assert!(capture_due(
            DemandPacing::Static,
            true,
            true,
            None,
            now,
            true
        ));
        let live = DemandPacing::Live {
            interval: Duration::from_secs(1),
        };
        assert!(capture_due(live, true, true, None, now, false));
        let recent = now.checked_sub(Duration::from_millis(100)).unwrap_or(now);
        assert!(!capture_due(live, true, true, Some(recent), now, false));
        let stale = now.checked_sub(Duration::from_secs(2)).unwrap_or(now);
        assert!(capture_due(live, true, true, Some(stale), now, false));
    }

    #[test]
    fn fair_order_serves_never_captured_first_then_least_recent() {
        let now = Instant::now();
        let windows = vec![WindowId(1), WindowId(2), WindowId(3)];
        let mut last = HashMap::new();
        last.insert(
            WindowId(1),
            now.checked_sub(Duration::from_secs(5)).unwrap_or(now),
        );
        last.insert(
            WindowId(2),
            now.checked_sub(Duration::from_secs(1)).unwrap_or(now),
        );
        assert_eq!(
            stalest_first(&windows, &last, now),
            vec![WindowId(3), WindowId(1), WindowId(2)]
        );
    }

    fn bitmap(side: u32) -> Bitmap {
        Bitmap {
            width: side,
            height: side,
            pixels: vec![0; (side * side) as usize],
        }
    }

    #[test]
    fn budget_evicts_undemanded_first_and_keeps_last_frames_under_budget() {
        let mut bitmaps = HashMap::from([
            ("a".to_owned(), bitmap(512)),
            ("b".to_owned(), bitmap(512)),
            ("c".to_owned(), bitmap(512)),
        ]);
        // 3 × 2 MiB retained (bitmap plus shm twin) against a 2 MiB budget: both
        // undemanded frames go before the demanded one is even considered.
        let demanded = HashSet::from(["c".to_owned()]);
        let evicted = evict_over_budget(&mut bitmaps, &demanded, 2 * 1024 * 1024);
        assert_eq!(evicted, vec!["a".to_owned(), "b".to_owned()]);
        assert!(bitmaps.contains_key("c"));

        // Under budget nothing goes, demanded or not: ordinary hide keeps its last frame.
        let evicted = evict_over_budget(&mut bitmaps, &HashSet::new(), 4 * 1024 * 1024);
        assert!(evicted.is_empty());
        assert!(bitmaps.contains_key("c"));
    }
}
