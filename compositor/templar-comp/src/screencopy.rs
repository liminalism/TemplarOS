//! Window previews for the Home Field, over the standard capture protocols:
//! `ext-image-capture-source-v1` (a toplevel or an output as a source) and
//! `ext-image-copy-capture-v1` (a session over it, frames into client buffers). Every global here
//! is filtered to the trusted shell — the one connection that can already list every window — so
//! nothing lets any client see more than it could already ask `templar_shell_v2` about. The
//! output source is what `templar-shell --screenshot` uses (`shell/templar-shell/src/screenshot.rs`);
//! it is the whole screen, so it is the one capture that must stay unreachable by applications.
//!
//! The protocol handlers run inside Wayland dispatch, where there is no renderer to hand: each
//! backend owns its `GlesRenderer` and only lends it out inside its own render pass. So
//! [`ImageCopyCaptureHandler::frame`] never renders. It admits only demanded windows and queues;
//! each backend drains before its output render, then rebinds its output framebuffer. A changed
//! surface tree is downscaled into one reusable target and read back into the client's shm.
//! Unchanged frames stay pending. Demanded offscreen sources receive bounded frame callbacks,
//! without changing mapping, focus or resource-policy visibility.
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

use crate::state::{Backend, TemplarCompState, client_is_trusted_shell};
use crate::window_bridge::WindowIndex;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ErasedContextId, ExportMem, ImportAll, ImportMem, Offscreen,
            damage::OutputDamageTracker,
            element::{
                Element, Id, Kind, RenderElement, surface::render_elements_from_surface_tree,
            },
            gles::GlesRenderbuffer,
            utils::CommitCounter,
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
            BufferConstraints, CaptureFailureReason, Frame, FrameRef, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::with_buffer_contents_mut,
    },
};
use std::{collections::HashMap, time::Duration};
use templar_window::WindowId;

/// What an output capture starts from: opaque black, so a screenshot of a desktop with nothing
/// on it is black rather than transparent. A preview clears to nothing on purpose (a tile is
/// composited over the Home Field); a screenshot is a picture of a screen.
const CAPTURE_CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Longest side of a preview, in pixels. A 2×1 tile at the default cell size is ~240 px wide.
pub const MAX_PREVIEW_SIDE: i32 = 512;
/// The compositor's own floor between two frames of one window, whatever the client asks.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(250);
/// At most four changed previews per drain. A single context-tagged 512px render
/// target is reused; readback is sequential. Adaptive constraints bound all shared
/// preview pools to 4 MiB, independently of how many windows are visible.
pub const MAX_PREVIEW_FRAMES_PER_DRAIN: usize = 4;

/// Bytes one `Argb8888` frame of `w`×`h` costs per copy.
#[cfg(test)]
pub fn frame_bytes(w: i32, h: i32) -> usize {
    usize::try_from(w.max(0)).unwrap_or(0) * usize::try_from(h.max(0)).unwrap_or(0) * 4
}

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

/// The shell's capture demand: which windows have visible Home card wells, fenced by a
/// generation (`templar_shell_manager_v2.set_preview_demand`, version 14). Checked both at
/// capture admission ([`ImageCopyCaptureHandler::frame`]) and immediately before offscreen
/// render ([`drain_pending`]): a frame admitted while its card was visible is retired without
/// rendering once demand disappears. That re-check is the visibility-generation boundary — a
/// frame already executing when visibility changes may finish, but its result never schedules
/// another capture, and the next frame needs fresh demand.
#[derive(Debug, Default)]
pub struct CaptureDemand {
    generation: u32,
    windows: std::collections::HashSet<WindowId>,
}

/// Install the shell's demand, fenced by generation: a generation that moves backwards
/// is ignored, so a reordered request never resurrects retired demand. Pure over the
/// enforceable half of `ScreencopyState`, so it is testable without a display; the method
/// on the state is a thin wrapper.
pub fn install_demand(
    current: &mut Option<CaptureDemand>,
    generation: u32,
    windows: Vec<WindowId>,
) -> bool {
    if let Some(demand) = &current
        && (generation.wrapping_sub(demand.generation) as i32) < 0
    {
        return false;
    }
    let windows: std::collections::HashSet<WindowId> = windows.into_iter().collect();
    let changed = current
        .as_ref()
        .is_none_or(|demand| demand.windows != windows);
    *current = Some(CaptureDemand {
        generation,
        windows,
    });
    changed
}

impl CaptureDemand {
    /// Whether `window` is currently demanded.
    pub fn contains(&self, window: WindowId) -> bool {
        self.windows.contains(&window)
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

/// Fair service across visible windows: each drain starts one slot further round the queue,
/// so a window at the back of a 20-window workload is not starved by the batch bound.
#[derive(Debug, Default)]
pub struct FairCursor {
    cursor: usize,
}

impl FairCursor {
    /// The rotation for a queue of `len`, advancing one slot per drain.
    pub fn next_start(&mut self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let start = self.cursor % len;
        self.cursor = self.cursor.wrapping_add(1);
        start
    }
}

#[derive(Debug, PartialEq)]
struct PreviewContent {
    geometry: Rectangle<i32, smithay::utils::Logical>,
    elements: Vec<(Id, CommitCounter, Rectangle<i32, Physical>)>,
}

#[derive(Debug)]
struct PreviewTarget {
    context: ErasedContextId,
    buffer: GlesRenderbuffer,
}

/// The capture protocols' state plus the queue between dispatch and render.
#[derive(Debug)]
pub struct ScreencopyState {
    pub sources: ImageCaptureSourceState,
    pub toplevel_sources: ToplevelCaptureSourceState,
    pub output_sources: OutputCaptureSourceState,
    pub capture: ImageCopyCaptureState,
    pub sessions: Vec<Session>,
    /// Frames accepted by [`ImageCopyCaptureHandler::frame`], rendered by [`drain_pending`].
    pub pending: Vec<(WindowId, Frame)>,
    /// Whole-output frames, answered by the backend pass that owns that output.
    pub pending_output: Vec<(Output, Frame)>,
    pub limiter: RateLimiter,
    /// The active Home owner's visible-well demand. Missing or empty demand denies
    /// preview capture; output screenshots use their independent one-shot route.
    pub demand: Option<CaptureDemand>,
    pub fair: FairCursor,
    content: HashMap<WindowId, PreviewContent>,
    target: Option<PreviewTarget>,
    pub rendered_previews: u64,
}

impl ScreencopyState {
    /// Create both globals, visible to the trusted shell only.
    pub fn new<B: Backend + 'static>(dh: &DisplayHandle) -> Self {
        Self {
            sources: ImageCaptureSourceState::new(),
            toplevel_sources: ToplevelCaptureSourceState::new_with_filter::<TemplarCompState<B>, _>(
                dh,
                client_is_trusted_shell,
            ),
            output_sources: OutputCaptureSourceState::new_with_filter::<TemplarCompState<B>, _>(
                dh,
                client_is_trusted_shell,
            ),
            capture: ImageCopyCaptureState::new_with_filter::<TemplarCompState<B>, _>(
                dh,
                client_is_trusted_shell,
            ),
            sessions: Vec::new(),
            pending: Vec::new(),
            pending_output: Vec::new(),
            limiter: RateLimiter::default(),
            demand: Some(CaptureDemand::default()),
            fair: FairCursor::default(),
            content: HashMap::new(),
            target: None,
            rendered_previews: 0,
        }
    }

    /// Install the shell's demand (`set_preview_demand`). A generation that moves backwards
    /// is ignored — a reordered request must not resurrect retired demand. Returns whether
    /// the enforced set changed.
    pub fn set_shell_demand(&mut self, generation: u32, windows: Vec<WindowId>) -> bool {
        let changed = install_demand(&mut self.demand, generation, windows);
        if changed {
            let demand = self.demand.as_ref().expect("installed demand");
            self.content.retain(|id, _| demand.contains(*id));
            let mut pending = std::mem::take(&mut self.pending);
            for (id, frame) in pending.drain(..) {
                if demand.contains(id) {
                    self.pending.push((id, frame));
                } else {
                    frame.fail(CaptureFailureReason::Unknown);
                }
            }
            if demand.is_empty() {
                self.target = None;
            }
        }
        changed
    }

    /// Retire every queued preview frame without rendering it — the lock/privacy path. Each
    /// frame is answered exactly once, with failure, through the protocol's own lifecycle;
    /// output captures are one-shots the caller owns and are left alone.
    pub fn retire_all_previews(&mut self) {
        self.content.clear();
        self.target = None;
        for (_, frame) in std::mem::take(&mut self.pending) {
            frame.fail(CaptureFailureReason::Unknown);
        }
    }

    /// Fail and drop queued preview frames for windows `keep` rejects (close, identity loss,
    /// demand gone), without rendering them. Returns how many were retired.
    pub fn prune_pending(&mut self, keep: impl Fn(WindowId) -> bool) -> usize {
        let pending = std::mem::take(&mut self.pending);
        let mut kept = Vec::with_capacity(pending.len());
        let mut retired = 0;
        for (window_id, frame) in pending {
            if keep(window_id) {
                kept.push((window_id, frame));
            } else {
                frame.fail(CaptureFailureReason::Unknown);
                retired += 1;
            }
        }
        self.pending = kept;
        retired
    }

    /// Drop demand membership for windows `keep` rejects (close, identity loss). Queued
    /// frames for them are retired unrendered by the pre-render check on the next drain;
    /// callers that need them gone now use [`Self::prune_pending`] as well.
    pub fn retain_demand(&mut self, keep: impl Fn(WindowId) -> bool) {
        if let Some(demand) = &mut self.demand {
            demand.windows.retain(|id| keep(*id));
        }
    }

    /// Forget a window wholesale: its demand, its pacing and its queued frames. The capture
    /// session objects themselves die with the client's handles; this stops the compositor
    /// doing any more work for the window.
    pub fn forget_window(&mut self, window_id: WindowId) {
        self.content.remove(&window_id);
        if let Some(demand) = &mut self.demand {
            demand.windows.remove(&window_id);
        }
        self.limiter.forget(window_id);
        self.prune_pending(|id| id != window_id);
    }
}

/// The preview size for a window of `geometry` pixels: the same aspect, longest side capped.
pub fn preview_size(width: i32, height: i32) -> (i32, i32) {
    preview_size_for_demand(width, height, 1)
}

fn preview_size_for_demand(width: i32, height: i32, demanded: usize) -> (i32, i32) {
    // Reserve 4 MiB total shared pools; shell separately bounds ready and retained pixels.
    let pixels = (4 * 1024 * 1024 - 64 * 1024) / 4 / demanded.max(1);
    let side = (pixels as f64).sqrt().floor().max(1.0) as i32;
    let side = side.min(MAX_PREVIEW_SIDE);
    let (w, h) = (width.max(1), height.max(1));
    let longest = w.max(h);
    if longest <= side {
        return (w, h);
    }
    let scale = f64::from(side) / f64::from(longest);
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

impl<B: Backend + 'static> ImageCaptureSourceHandler for TemplarCompState<B> {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {}
}

impl<B: Backend + 'static> ToplevelCaptureSourceHandler for TemplarCompState<B> {
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

impl<B: Backend + 'static> OutputCaptureSourceHandler for TemplarCompState<B> {
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

impl<B: Backend + 'static> ImageCopyCaptureHandler for TemplarCompState<B> {
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
        let count = self
            .screencopy
            .demand
            .as_ref()
            .map_or(1, CaptureDemand::len);
        let (w, h) = if count <= 1 {
            preview_size(record.geometry.width, record.geometry.height)
        } else {
            preview_size_for_demand(record.geometry.width, record.geometry.height, count)
        };
        Some(BufferConstraints {
            size: Size::from((w, h)),
            shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
            dma: None,
        })
    }

    fn new_session(&mut self, session: Session) {
        if let Some(SourceWindow(window)) = session.source().user_data().get::<SourceWindow>() {
            self.screencopy.content.remove(window);
        }
        self.screencopy.sessions.push(session);
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
        if self.session_lock.locked || self.window_service.window(window_id).is_none() {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        }
        // Admission requires explicit visible-well demand. A trusted-shell request
        // alone is insufficient; `drain_pending` checks again before rendering.
        if self
            .screencopy
            .demand
            .as_ref()
            .is_none_or(|demand| !demand.contains(window_id))
        {
            frame.fail(CaptureFailureReason::Unknown);
            return;
        }
        // One in-flight capture per demanded window: a second frame while one waits for the
        // render pass is refused, not queued.
        if self
            .screencopy
            .pending
            .iter()
            .any(|(id, _)| *id == window_id)
        {
            frame.fail(CaptureFailureReason::Unknown);
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
        B::request_redraw(self);
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.screencopy
            .pending
            .retain(|(_, pending)| *pending != frame);
        self.screencopy
            .pending_output
            .retain(|(_, pending)| *pending != frame);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.screencopy.sessions.retain(|s| *s != session);
    }
}

/// Render queued frames in a fair, bounded batch. Called by each backend right after its own
/// render pass, with the renderer that pass used. Each frame is answered exactly once: success
/// after the client buffer holds the pixels, failure otherwise — a frame deferred by the batch
/// bound waits for the next pass rather than failing. Preview capture never touches activation,
/// mapping or the governor: a parked window keeps its last submitted content and is never
/// reported foreground or visible to resource policy just to animate its thumbnail.
///
/// Takes the pieces it needs individually, so a backend can call it while its own renderer — a
/// field of `backend_data` — is borrowed mutably (the `ChromeInputs` pattern).
pub fn drain_pending<
    R: ImportAll + ImportMem + Offscreen<GlesRenderbuffer> + ExportMem,
    E: RenderElement<R>,
>(
    screencopy: &mut ScreencopyState,
    windows: &WindowIndex,
    clock: &Clock<Monotonic>,
    frame_time: Duration,
    renderer: &mut R,
    output: &Output,
    elements: &[E],
    previews: bool,
) where
    R::TextureId: Clone + Send + 'static,
    R::Error: Send + Sync + 'static,
{
    // Only this pass's own output. Native transport passes share the primary GPU
    // renderer; Home window previews run only when the logical-primary pass opts in.
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
    if !previews {
        return;
    }

    // Keep the queue allocation across unchanged frames. Visibility is checked at
    // render time as well as admission: retirement must not render one last frame.
    let mut pending = std::mem::take(&mut screencopy.pending);
    if !pending.is_empty() {
        let start = screencopy.fair.next_start(pending.len());
        pending.rotate_left(start);
    }
    let mut rendered = 0;
    let mut index = 0;
    while index < pending.len() && rendered < MAX_PREVIEW_FRAMES_PER_DRAIN {
        let (window_id, frame) = &pending[index];
        let window_id = *window_id;
        if screencopy
            .demand
            .as_ref()
            .is_none_or(|demand| !demand.contains(window_id))
        {
            pending
                .swap_remove(index)
                .1
                .fail(CaptureFailureReason::Unknown);
            continue;
        }
        // A demanded offscreen client still needs frame callbacks to submit changed
        // content. Treat it as offscreen (no primary output), throttle to the capture
        // cap, and never map/activate it or change governor visibility.
        if let Some(window) = windows.element_of(window_id) {
            window.send_frame(output, frame_time, Some(MIN_FRAME_INTERVAL), |_, _| None);
        }
        match render_one(
            windows,
            renderer,
            window_id,
            frame,
            &mut screencopy.content,
            &mut screencopy.target,
        ) {
            Ok(true) => {
                rendered += 1;
                screencopy.rendered_previews += 1;
                pending.swap_remove(index).1.success(
                    Transform::Normal,
                    None,
                    Duration::from(clock.now()),
                );
            }
            Ok(false) => index += 1,
            Err(error) => {
                tracing::debug!(%error, ?window_id, "preview frame failed");
                pending
                    .swap_remove(index)
                    .1
                    .fail(CaptureFailureReason::Unknown);
            }
        }
    }
    screencopy.pending = pending;
}

fn render_one<R: ImportAll + ImportMem + Offscreen<GlesRenderbuffer> + ExportMem>(
    windows: &WindowIndex,
    renderer: &mut R,
    window_id: WindowId,
    frame: &Frame,
    content: &mut HashMap<WindowId, PreviewContent>,
    target: &mut Option<PreviewTarget>,
) -> anyhow::Result<bool>
where
    R::TextureId: Clone + Send + 'static,
    R::Error: Send + Sync + 'static,
{
    let surface = windows
        .surface_for(window_id)
        .ok_or_else(|| anyhow::anyhow!("window has no surface"))?
        .clone();
    let window = windows
        .element_of(window_id)
        .ok_or_else(|| anyhow::anyhow!("window has no element"))?
        .clone();
    let geometry = window.geometry();
    let (w, h) = with_buffer_contents_mut(&frame.buffer(), |_, _, data| (data.width, data.height))
        .map_err(|error| anyhow::anyhow!("preview buffer dimensions: {error:?}"))?;
    anyhow::ensure!(
        w > 0 && h > 0 && w <= MAX_PREVIEW_SIDE && h <= MAX_PREVIEW_SIDE,
        "preview buffer exceeds capture bounds"
    );
    let scale = (f64::from(w) / f64::from(geometry.size.w.max(1)))
        .min(f64::from(h) / f64::from(geometry.size.h.max(1)));
    let size: Size<i32, Physical> = Size::from((w, h));

    // The surface tree at the window's content origin, scaled to the preview.
    let origin: Point<f64, Physical> = Point::from((
        -f64::from(geometry.loc.x) * scale,
        -f64::from(geometry.loc.y) * scale,
    ));
    let elements: Vec<
        smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>,
    > = render_elements_from_surface_tree(
        renderer,
        &surface,
        origin.to_i32_round(),
        scale,
        1.0,
        Kind::Unspecified,
    );
    anyhow::ensure!(
        !elements.is_empty(),
        "window has no committed content to capture"
    );

    if content.get(&window_id).is_some_and(|old| {
        old.geometry == geometry
            && old.elements.len() == elements.len()
            && old
                .elements
                .iter()
                .zip(&elements)
                .all(|((id, commit, rect), element)| {
                    id == element.id()
                        && *commit == element.current_commit()
                        && *rect == element.geometry(scale.into())
                })
    }) {
        return Ok(false);
    }
    let context = renderer.context_id().erased();
    if target
        .as_ref()
        .is_none_or(|target| target.context != context)
    {
        *target = Some(PreviewTarget {
            context,
            buffer: renderer.create_buffer(
                Fourcc::Abgr8888,
                (MAX_PREVIEW_SIDE, MAX_PREVIEW_SIDE).into(),
            )?,
        });
    }
    // Surface-tree construction scales positions; the damage tracker supplies the
    // scale for element dimensions. Using 1 here crops large clients instead of fitting.
    let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    let mapping = {
        let mut framebuffer =
            renderer.bind(&mut target.as_mut().expect("created target").buffer)?;
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
    write_shm(frame, pixels, w, h, false)?;
    let current = PreviewContent {
        geometry,
        elements: elements
            .iter()
            .map(|element| {
                (
                    element.id().clone(),
                    element.current_commit(),
                    element.geometry(scale.into()),
                )
            })
            .collect(),
    };
    content.insert(window_id, current);
    Ok(true)
}

/// One whole output, into the client's shm buffer: the element list the backend is about to draw
/// (cursor included), rendered again into an offscreen buffer at the output's own size and read
/// back. Not the backend's framebuffer — that one is owned by DRM or by winit and is not readable
/// here — but the same elements through the same renderer, which is as close as this gets without
/// a second scanout path.
#[allow(unsafe_code)]
fn render_output_frame<
    R: ImportAll + ImportMem + Offscreen<GlesRenderbuffer> + ExportMem,
    E: RenderElement<R>,
>(
    renderer: &mut R,
    output: &Output,
    elements: &[E],
    frame: &Frame,
) -> anyhow::Result<()>
where
    R::TextureId: Clone + Send + 'static,
    R::Error: Send + Sync + 'static,
{
    let size = output_size(output).ok_or_else(|| anyhow::anyhow!("the output has no mode"))?;
    let (w, h) = (size.w, size.h);
    let mut target: GlesRenderbuffer = renderer.create_buffer(
        Fourcc::Abgr8888,
        size.to_logical(1).to_buffer(1, Transform::Normal),
    )?;
    let mut tracker = OutputDamageTracker::from_output(output);
    let mapping = {
        let mut framebuffer = renderer.bind(&mut target)?;
        let clear = if crate::viewport::insets(output).is_zero() {
            CAPTURE_CLEAR
        } else {
            [0.0, 0.0, 0.0, 1.0]
        };
        if crate::viewport::hardware(output) && !crate::viewport::insets(output).is_zero() {
            let captured: Vec<_> =
                crate::viewport::hardware_capture_elements(output, elements.iter())
                    .ok_or_else(|| anyhow::anyhow!("the calibrated output has no physical mode"))?
                    .collect();
            tracker
                .render_output(renderer, &mut framebuffer, 0, &captured, clear)
                .map(|_| ())
        } else {
            tracker
                .render_output(renderer, &mut framebuffer, 0, elements, clear)
                .map(|_| ())
        }
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

    #[test]
    fn demand_fences_backwards_generations_and_reports_changes() {
        let mut demand: Option<CaptureDemand> = None;
        assert!(install_demand(
            &mut demand,
            1,
            vec![WindowId(1), WindowId(2)]
        ));
        assert!(!install_demand(
            &mut demand,
            1,
            vec![WindowId(2), WindowId(1)]
        ));
        assert!(!install_demand(&mut demand, 0, vec![WindowId(9)])); // backwards: ignored
        let demand = demand.expect("installed");
        assert!(demand.contains(WindowId(1)));
        assert!(!demand.contains(WindowId(9)));
        assert_eq!(demand.generation(), 1);
        assert_eq!(demand.len(), 2);
        // Empty demand still enforces: no demand, no render.
        assert!(install_demand(&mut Some(demand_state()), 2, vec![]));
    }

    #[test]
    fn wrapped_visibility_generation_cannot_restore_old_demand() {
        let mut demand = Some(CaptureDemand {
            generation: u32::MAX,
            windows: [WindowId(1)].into_iter().collect(),
        });
        assert!(install_demand(&mut demand, 0, vec![]));
        assert!(!install_demand(&mut demand, u32::MAX, vec![WindowId(1)]));
        assert!(demand.unwrap().is_empty());
    }

    fn demand_state() -> CaptureDemand {
        CaptureDemand {
            generation: 1,
            windows: [WindowId(1)].into_iter().collect(),
        }
    }

    #[test]
    fn fair_service_rotates_the_batch_start_each_drain() {
        let mut fair = FairCursor::default();
        assert_eq!(fair.next_start(0), 0);
        // A 6-window queue served 4 at a time: three drains cover every window first.
        assert_eq!(fair.next_start(6), 0);
        assert_eq!(fair.next_start(6), 1);
        assert_eq!(fair.next_start(6), 2);
        // Shrinking the queue re-clamps rather than over-rotating.
        assert_eq!(fair.next_start(2), 1);
    }

    #[test]
    fn aggregate_pool_constraints_bound_many_visible_windows() {
        for count in [1, 4, 20, 100, 1_000] {
            let (w, h) = preview_size_for_demand(4096, 4096, count);
            assert!(w > 0 && h > 0 && w <= MAX_PREVIEW_SIDE);
            assert!(frame_bytes(w, h) * count <= 4 * 1024 * 1024);
        }
    }
}
