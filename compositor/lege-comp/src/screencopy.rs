//! Window previews for the Home Field, over the standard capture protocols:
//! `ext-image-capture-source-v1` (a toplevel or an output as a source) and
//! `ext-image-copy-capture-v1` (a session over it, frames into client buffers). Every global here
//! is filtered to the trusted shell — the one connection that can already list every window — so
//! nothing lets any client see more than it could already ask `lege_shell_v1` about. The
//! output source is what `lege-shell --screenshot` uses (`shell/lege-shell/src/screenshot.rs`);
//! it is the whole screen, so it is the one capture that must stay unreachable by applications.
//!
//! The protocol handlers run inside Wayland dispatch, where there is no renderer to hand: each
//! backend owns its `GlesRenderer` and only lends it out inside its own render pass. So
//! [`ImageCopyCaptureHandler::frame`] never renders. It rate-limits and queues; right after the
//! backend has drawn the output it calls [`drain_pending`] with the renderer in hand, which draws
//! the window's surface tree into an offscreen buffer, reads it back and writes the client's shm
//! buffer. A window that is *hidden* (shelved, or behind a reveal) still previews: its surface
//! keeps committing out of sight (`src/space_sync.rs`), which is the whole point of a parked tile.
//!
//! An output frame is the same path over a different element list: the backend hands
//! [`drain_pending`] the very `SceneElement`s it is about to draw (cursor included), so a
//! screenshot is what the screen shows rather than a second, independently-assembled scene that
//! could disagree with it.
//!
//! Constraints are downscaled — the longer side capped at [`MAX_PREVIEW_SIDE`] — because a tile
//! is small and a full-size copy of every window would be the new source of instability the
//! design warns against; the compositor's own cap of [`MIN_FRAME_INTERVAL`] per window bounds the
//! rest, whatever a client asks for.

use crate::state::{Backend, LegeCompState, client_is_trusted_shell};
use crate::window_bridge::WindowIndex;
use lege_window::WindowId;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            element::{Kind, surface::render_elements_from_surface_tree},
            gles::{GlesRenderbuffer, GlesRenderer},
        },
    },
    output::Output,
    reexports::wayland_server::{DisplayHandle, protocol::wl_shm},
    utils::{Clock, Monotonic, Physical, Point, Rectangle, Size, Transform},
    wayland::{
        foreign_toplevel_list::ForeignToplevelHandle,
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, ImageCaptureSourceState,
            OutputCaptureSourceHandler, OutputCaptureSourceState, ToplevelCaptureSourceHandler,
            ToplevelCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::with_buffer_contents_mut,
    },
};
use std::{collections::HashMap, time::Duration};

/// What an output capture starts from: opaque black, so a screenshot of a desktop with nothing
/// on it is black rather than transparent. A preview clears to nothing on purpose (a tile is
/// composited over the Home Field); a screenshot is a picture of a screen.
const CAPTURE_CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Longest side of a preview, in pixels. A 2×1 tile at the default cell size is ~240 px wide.
pub const MAX_PREVIEW_SIDE: i32 = 512;
/// The compositor's own floor between two frames of one window, whatever the client asks.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(250);

/// Pure per-window pacing: the last time a frame was allowed, so `allow` is testable without a
/// display.
#[derive(Debug, Default)]
pub struct RateLimiter {
    last: HashMap<WindowId, Duration>,
}

impl RateLimiter {
    /// Whether a frame for `window` may be rendered at `now`, recording it if so.
    pub fn allow(&mut self, window: WindowId, now: Duration, min_interval: Duration) -> bool {
        match self.last.get(&window) {
            Some(last) if now.saturating_sub(*last) < min_interval => false,
            _ => {
                self.last.insert(window, now);
                true
            }
        }
    }

    pub fn forget(&mut self, window: WindowId) {
        self.last.remove(&window);
    }
}

/// The capture protocols' state plus the queue between dispatch and render.
#[derive(Debug)]
pub struct ScreencopyState {
    pub sources: ImageCaptureSourceState,
    pub toplevel_sources: ToplevelCaptureSourceState,
    pub output_sources: OutputCaptureSourceState,
    pub capture: ImageCopyCaptureState,
    pub sessions: Vec<SessionRef>,
    /// Frames accepted by [`ImageCopyCaptureHandler::frame`], rendered by [`drain_pending`].
    pub pending: Vec<(WindowId, Frame)>,
    /// Whole-output frames, answered by the backend pass that owns that output.
    pub pending_output: Vec<(Output, Frame)>,
    pub limiter: RateLimiter,
}

impl ScreencopyState {
    /// Create both globals, visible to the trusted shell only.
    pub fn new<B: Backend + 'static>(dh: &DisplayHandle) -> Self {
        Self {
            sources: ImageCaptureSourceState::new(),
            toplevel_sources: ToplevelCaptureSourceState::new_with_filter::<LegeCompState<B>, _>(
                dh,
                client_is_trusted_shell,
            ),
            output_sources: OutputCaptureSourceState::new_with_filter::<LegeCompState<B>, _>(
                dh,
                client_is_trusted_shell,
            ),
            capture: ImageCopyCaptureState::new_with_filter::<LegeCompState<B>, _>(
                dh,
                client_is_trusted_shell,
            ),
            sessions: Vec::new(),
            pending: Vec::new(),
            pending_output: Vec::new(),
            limiter: RateLimiter::default(),
        }
    }
}

/// The preview size for a window of `geometry` pixels: the same aspect, longest side capped.
pub fn preview_size(width: i32, height: i32) -> (i32, i32) {
    let (w, h) = (width.max(1), height.max(1));
    let longest = w.max(h);
    if longest <= MAX_PREVIEW_SIDE {
        return (w, h);
    }
    let scale = f64::from(MAX_PREVIEW_SIDE) / f64::from(longest);
    (
        (f64::from(w) * scale).round().max(1.0) as i32,
        (f64::from(h) * scale).round().max(1.0) as i32,
    )
}

/// The window a capture source stands for, resolved when the source was created.
#[derive(Clone, Copy, Debug)]
struct SourceWindow(WindowId);

/// The output a capture source stands for, resolved when the source was created. A source
/// carries one or the other, never both, which is what the two handlers below branch on.
#[derive(Clone, Debug)]
struct SourceOutput(Output);

impl<B: Backend + 'static> ImageCaptureSourceHandler for LegeCompState<B> {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {}
}

impl<B: Backend + 'static> ToplevelCaptureSourceHandler for LegeCompState<B> {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.screencopy.toplevel_sources
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        if let Some(window_id) = self
            .foreign_toplevels
            .window_for_identifier(&toplevel.identifier())
        {
            source
                .user_data()
                .insert_if_missing(|| SourceWindow(window_id));
        }
    }
}

impl<B: Backend + 'static> OutputCaptureSourceHandler for LegeCompState<B> {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.screencopy.output_sources
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source
            .user_data()
            .insert_if_missing(|| SourceOutput(output.clone()));
    }
}

/// The framebuffer size of `output`: its mode, through its transform — the same pixels the
/// backend draws into, so a screenshot is the screen at its own resolution and never a rescale.
fn output_size(output: &Output) -> Option<Size<i32, Physical>> {
    let mode = output.current_mode()?;
    Some(output.current_transform().transform_size(mode.size))
}

impl<B: Backend + 'static> ImageCopyCaptureHandler for LegeCompState<B> {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.screencopy.capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        if let Some(SourceOutput(output)) = source.user_data().get::<SourceOutput>() {
            let size = output_size(output)?;
            return Some(BufferConstraints {
                size: Size::from((size.w, size.h)),
                shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
                dma: None,
            });
        }
        let window_id = source.user_data().get::<SourceWindow>()?.0;
        let record = self.window_service.window(window_id)?;
        let (w, h) = preview_size(record.geometry.width, record.geometry.height);
        Some(BufferConstraints {
            size: Size::from((w, h)),
            shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
            dma: None,
        })
    }

    fn new_session(&mut self, session: Session) {
        // Kept as a reference: the owned `Session` stops the protocol object on drop, and the
        // client, not the compositor, decides when a preview session ends.
        let reference = session.as_ref();
        std::mem::forget(session);
        self.screencopy.sessions.push(reference);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        // ponytail: an output frame is not rate-limited. `RateLimiter` is keyed by `WindowId`,
        // only the trusted shell can reach this global at all, and a screenshot is a one-shot.
        // Upgrade path if that ever stops holding: a second limiter keyed by output name.
        if let Some(SourceOutput(output)) = session.source().user_data().get::<SourceOutput>() {
            self.screencopy.pending_output.push((output.clone(), frame));
            // Nothing on a still screen would otherwise draw the frame this is waiting for.
            B::request_redraw(self);
            return;
        }
        let Some(window_id) = session
            .source()
            .user_data()
            .get::<SourceWindow>()
            .map(|w| w.0)
        else {
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        if self.window_service.window(window_id).is_none() {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        }
        let now = Duration::from(self.clock.now());
        if !self
            .screencopy
            .limiter
            .allow(window_id, now, MIN_FRAME_INTERVAL)
        {
            // Too soon: the client gets an honest failure and tries again later. Buffer
            // constraints are unchanged, so a client that keeps its buffer loses nothing.
            frame.fail(CaptureFailureReason::Unknown);
            return;
        }
        self.screencopy.pending.push((window_id, frame));
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.screencopy.sessions.retain(|s| *s != session);
    }
}

/// Render every queued frame. Called by each backend right after its own render pass, with the
/// renderer that pass used. Each frame is answered exactly once: success after the client buffer
/// holds the pixels, failure otherwise.
///
/// Takes the pieces it needs individually, so a backend can call it while its own renderer — a
/// field of `backend_data` — is borrowed mutably (the `ChromeInputs` pattern).
pub fn drain_pending(
    screencopy: &mut ScreencopyState,
    windows: &WindowIndex,
    clock: &Clock<Monotonic>,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[crate::scene::SceneElement],
) {
    // Only this pass's own output: on udev every CRTC calls this with its own renderer, and a
    // frame for another output's session belongs to that output's pass, not this one's.
    let (mine, theirs) = std::mem::take(&mut screencopy.pending_output)
        .into_iter()
        .partition::<Vec<_>, _>(|(target, _)| target == output);
    screencopy.pending_output = theirs;
    for (target, frame) in mine {
        match render_output_frame(renderer, &target, elements, &frame) {
            Ok(()) => frame.success(Transform::Normal, None, Duration::from(clock.now())),
            Err(error) => {
                tracing::debug!(%error, "output capture frame failed");
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    let pending = std::mem::take(&mut screencopy.pending);
    for (window_id, frame) in pending {
        match render_one(windows, renderer, window_id, &frame) {
            Ok(()) => {
                frame.success(Transform::Normal, None, Duration::from(clock.now()));
            }
            Err(error) => {
                tracing::debug!(?window_id, %error, "preview frame failed");
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
}

fn render_one(
    windows: &WindowIndex,
    renderer: &mut GlesRenderer,
    window_id: WindowId,
    frame: &Frame,
) -> anyhow::Result<()> {
    let surface = windows
        .surface_for(window_id)
        .ok_or_else(|| anyhow::anyhow!("window has no surface"))?
        .clone();
    let window = windows
        .element_of(window_id)
        .ok_or_else(|| anyhow::anyhow!("window has no element"))?
        .clone();
    let geometry = window.geometry();
    let (w, h) = preview_size(geometry.size.w, geometry.size.h);
    let scale = f64::from(w) / f64::from(geometry.size.w.max(1));
    let size: Size<i32, Physical> = Size::from((w, h));

    // The surface tree at the window's content origin, scaled to the preview.
    let origin: Point<f64, Physical> = Point::from((
        -f64::from(geometry.loc.x) * scale,
        -f64::from(geometry.loc.y) * scale,
    ));
    let elements: Vec<
        smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>,
    > = render_elements_from_surface_tree(
        renderer,
        &surface,
        origin.to_i32_round(),
        scale,
        1.0,
        Kind::Unspecified,
    );

    let mut target: GlesRenderbuffer = renderer.create_buffer(
        Fourcc::Abgr8888,
        size.to_logical(1).to_buffer(1, Transform::Normal),
    )?;
    let mut tracker = OutputDamageTracker::new(size, 1.0, Transform::Normal);
    let mapping = {
        let mut framebuffer = renderer.bind(&mut target)?;
        tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                [0.0, 0.0, 0.0, 0.0],
            )
            .map_err(|error| anyhow::anyhow!("rendering the preview: {error}"))?;
        renderer.copy_framebuffer(
            &framebuffer,
            Rectangle::from_size(size.to_logical(1).to_buffer(1, Transform::Normal)),
            Fourcc::Argb8888,
        )?
    };
    let pixels = renderer.map_texture(&mapping)?;
    write_shm(frame, pixels, w, h, false)
}

/// One whole output, into the client's shm buffer: the element list the backend is about to draw
/// (cursor included), rendered again into an offscreen buffer at the output's own size and read
/// back. Not the backend's framebuffer — that one is owned by DRM or by winit and is not readable
/// here — but the same elements through the same renderer, which is as close as this gets without
/// a second scanout path.
#[allow(unsafe_code)]
fn render_output_frame(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[crate::scene::SceneElement],
    frame: &Frame,
) -> anyhow::Result<()> {
    let size = output_size(output).ok_or_else(|| anyhow::anyhow!("the output has no mode"))?;
    let (w, h) = (size.w, size.h);
    let mut target: GlesRenderbuffer = renderer.create_buffer(
        Fourcc::Abgr8888,
        size.to_logical(1).to_buffer(1, Transform::Normal),
    )?;
    let mut tracker = OutputDamageTracker::from_output(output);
    let mapping = {
        let mut framebuffer = renderer.bind(&mut target)?;
        tracker
            .render_output(renderer, &mut framebuffer, 0, elements, CAPTURE_CLEAR)
            .map_err(|error| anyhow::anyhow!("rendering the output capture: {error}"))?;
        renderer.copy_framebuffer(
            &framebuffer,
            Rectangle::from_size(size.to_logical(1).to_buffer(1, Transform::Normal)),
            Fourcc::Argb8888,
        )?
    };
    let pixels = renderer.map_texture(&mapping)?;
    write_shm(frame, pixels, w, h, true)
}

/// The pixels into the client's shm pool — the one raw write in this crate outside the renderer,
/// shared by the preview and the output paths so the bounds check exists once.
///
/// `flip` writes the readback back to front. An output capture needs it — GL's framebuffer origin
/// is the bottom-left corner while row 0 of a `wl_shm` buffer is the *top* of the image, so an
/// unflipped output capture hands the client an upside-down screen, which is what the first
/// working screenshot showed. A preview does not: its surface tree is rendered at an origin of its
/// own and comes back the right way up, which the parked-session tiles have always relied on.
#[allow(unsafe_code)]
fn write_shm(frame: &Frame, pixels: &[u8], w: i32, h: i32, flip: bool) -> anyhow::Result<()> {
    let row_bytes = (w * 4) as usize;
    if pixels.len() < row_bytes * h as usize {
        anyhow::bail!("readback shorter than the capture");
    }

    let buffer = frame.buffer();
    with_buffer_contents_mut(&buffer, |ptr, len, data| {
        if data.width < w || data.height < h {
            return Err(anyhow::anyhow!(
                "client buffer smaller than the constraints"
            ));
        }
        if !matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        ) {
            return Err(anyhow::anyhow!("client buffer has an unsupported format"));
        }
        let stride = data.stride as usize;
        let offset = data.offset.max(0) as usize;
        if offset + stride * h as usize > len {
            return Err(anyhow::anyhow!(
                "client buffer smaller than its own geometry"
            ));
        }
        // SAFETY: `ptr..ptr+len` is the pool mapping `with_buffer_contents_mut` guarantees for
        // the duration of this closure; every write stays inside `offset + stride * h <= len`,
        // checked just above, and nothing reads the region back as a Rust slice.
        for row in 0..h as usize {
            let source_row = if flip { h as usize - 1 - row } else { row };
            let source = &pixels[source_row * row_bytes..source_row * row_bytes + row_bytes];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    ptr.add(offset + row * stride),
                    row_bytes,
                );
            }
        }
        Ok(())
    })
    .map_err(|error| anyhow::anyhow!("accessing the client buffer: {error:?}"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_keep_their_aspect_and_cap_the_longest_side() {
        assert_eq!(preview_size(1920, 1080), (512, 288));
        assert_eq!(preview_size(300, 200), (300, 200));
        assert_eq!(preview_size(200, 1000), (102, 512));
        assert_eq!(preview_size(0, 0), (1, 1));
    }

    #[test]
    fn the_rate_limiter_allows_one_frame_per_interval_per_window() {
        let mut limiter = RateLimiter::default();
        let a = WindowId(1);
        let b = WindowId(2);
        assert!(limiter.allow(a, Duration::from_millis(0), MIN_FRAME_INTERVAL));
        assert!(!limiter.allow(a, Duration::from_millis(100), MIN_FRAME_INTERVAL));
        assert!(limiter.allow(b, Duration::from_millis(100), MIN_FRAME_INTERVAL));
        assert!(limiter.allow(a, Duration::from_millis(250), MIN_FRAME_INTERVAL));
        limiter.forget(a);
        assert!(limiter.allow(a, Duration::from_millis(251), MIN_FRAME_INTERVAL));
    }
}
