//! The seam between `pixelkit-raster`'s CPU pixel buffer and Smithay's GPU-uploaded render
//! elements, and the per-widget cache that keeps a repaint (and, upstream of that, a
//! `pixelkit-text` shape/rasterize pass) from happening on every frame regardless of whether
//! anything actually changed — the task brief's "no text layout on the render thread per frame if
//! you can avoid it".
//!
//! [`CachedWidget::ensure`] is the whole contract: give it a [`WidgetKey`] describing what should
//! be on screen and a closure that draws it, and it only calls that closure when the key (or the
//! widget's size/scale) differs from what is already cached. Everything else — converting
//! `pixelkit-raster`'s pixels into a Smithay `MemoryRenderBuffer` and asking a renderer to import
//! them — happens the same way whether or not a repaint occurred, since uploading an unchanged
//! `MemoryRenderBuffer` a second time is a no-op internally (Smithay tracks that itself, module
//! doc of `smithay::backend::renderer::element::memory`).

use pixelkit_raster::{Painter, WindowBuffer};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    utils::{Physical, Point, Rectangle, Size, Transform},
};

/// Everything that determines a cached widget's pixels, compared by value against the previous
/// frame's key. Deliberately does *not* include the widget's size or output scale — those are
/// tracked separately by [`CachedWidget`] itself, so a caller building a [`WidgetKey`] only has to
/// describe *content* (what text, what colours, which button is selected) and can never
/// accidentally cache-bust or fail-to-cache-bust by forgetting to mirror a size change into it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WidgetKey {
    pub text: String,
    pub secondary: String,
    pub background: u32,
    pub variant: u32,
    /// Anything else about the widget that is a boolean — one bit per state. A window's name tag
    /// uses bit 0 for "pinned", which changes the face it is drawn in and nothing else, so
    /// without this the upright bitmap would stay cached across the toggle.
    pub flags: u32,
    pub tab_count: Option<u32>,
}

impl WidgetKey {
    pub fn new(text: impl Into<String>, background: u32) -> Self {
        Self {
            text: text.into(),
            secondary: String::new(),
            background,
            variant: 0,
            flags: 0,
            tab_count: None,
        }
    }

    #[must_use]
    pub fn with_secondary(mut self, secondary: impl Into<String>) -> Self {
        self.secondary = secondary.into();
        self
    }

    #[must_use]
    pub const fn with_variant(mut self, variant: u32) -> Self {
        self.variant = variant;
        self
    }

    #[must_use]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }
}

/// One compositor-drawn widget's cached bitmap — a window's recovery band, the pressure overlay,
/// one scatter-view label, a switcher row, the lock screen. Construct once and reuse across
/// frames (`chrome::mod`'s `ChromeState` keeps one per window/purpose); dropping it drops the
/// GPU texture too, so a widget that no longer applies (its window closed, the overlay hid)
/// should simply be discarded rather than kept "just in case".
pub struct CachedWidget {
    key: Option<WidgetKey>,
    logical_size: (i32, i32),
    buffer_scale: i32,
    format: Fourcc,
    buffer: MemoryRenderBuffer,
}

impl Default for CachedWidget {
    fn default() -> Self {
        Self::with_format(Fourcc::Xrgb8888)
    }
}

impl CachedWidget {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn new_transparent() -> Self {
        Self::with_format(Fourcc::Argb8888)
    }
    fn with_format(format: Fourcc) -> Self {
        Self {
            key: None,
            logical_size: (0, 0),
            buffer_scale: 1,
            format,
            buffer: MemoryRenderBuffer::new(format, (1, 1), 1, Transform::Normal, None),
        }
    }
    /// Keep the imported buffer allocation, but repaint it under the new shared appearance.
    pub fn invalidate(&mut self) {
        self.key = None;
    }

    /// Repaint only if `key`, `logical_size` or `output_scale` differ from the last call's, then
    /// return a render element positioned at `location` (physical pixels). `paint` receives a
    /// [`Painter`] over a pixel buffer exactly `logical_size` (upsampled to
    /// `ceil(output_scale)` physical texels, so chrome text stays crisp under fractional scale
    /// rather than being stretched from a 1x buffer) — it must fill the whole area, since nothing
    /// clears it beforehand.
    ///
    /// `renderer` is only ever used to *import* the already-painted pixels as a texture — drawing
    /// itself happens entirely on the CPU beforehand, through `pixelkit-raster`. A failed import
    /// (e.g. an unsupported format on unusual hardware) is logged and skipped rather than
    /// propagated: a missing chrome element for one frame is recoverable, a panicked render
    /// thread is not.
    pub fn ensure<R: ImportMem>(
        &mut self,
        key: WidgetKey,
        logical_size: (i32, i32),
        output_scale: f64,
        location: Point<f64, Physical>,
        scratch: &mut WindowBuffer,
        renderer: &mut R,
        paint: impl FnOnce(&mut Painter<'_>, i32, i32),
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R::TextureId: Clone + Send + 'static,
    {
        if logical_size.0 <= 0 || logical_size.1 <= 0 {
            return None;
        }
        let buffer_scale = output_scale.ceil().max(1.0) as i32;

        let stale = self.key.as_ref() != Some(&key)
            || self.logical_size != logical_size
            || self.buffer_scale != buffer_scale;
        if stale {
            self.repaint(&key, logical_size, buffer_scale, scratch, paint);
            self.key = Some(key);
            self.logical_size = logical_size;
            self.buffer_scale = buffer_scale;
        }

        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            Ok(element) => Some(element),
            Err(error) => {
                tracing::warn!(%error, "chrome: failed to import a painted widget as a texture");
                None
            }
        }
    }

    fn repaint(
        &mut self,
        _key: &WidgetKey,
        logical_size: (i32, i32),
        buffer_scale: i32,
        scratch: &mut WindowBuffer,
        paint: impl FnOnce(&mut Painter<'_>, i32, i32),
    ) {
        let pixel_w = (logical_size.0 * buffer_scale).max(1);
        let pixel_h = (logical_size.1 * buffer_scale).max(1);

        scratch.resize(pixel_w as u32, pixel_h as u32);
        {
            let mut painter = Painter::new(scratch);
            paint(&mut painter, pixel_w, pixel_h);
        }

        // Scale is immutable buffer metadata; appearance and size changes keep its identity.
        if self.buffer_scale != buffer_scale {
            self.buffer = MemoryRenderBuffer::new(
                self.format,
                (pixel_w, pixel_h),
                buffer_scale,
                Transform::Normal,
                None,
            );
        }
        let mut context = self.buffer.render();
        if self.buffer_scale == buffer_scale && self.logical_size != logical_size {
            context.resize((pixel_w, pixel_h));
        }
        let whole = Rectangle::from_size(Size::from((pixel_w, pixel_h)));
        context
            .draw(|bytes| {
                for (destination, pixel) in bytes.chunks_exact_mut(4).zip(&scratch.pixels) {
                    destination.copy_from_slice(&pixel.to_le_bytes());
                }
                Ok::<_, std::convert::Infallible>(vec![whole])
            })
            .unwrap();
        // Calibration is ARGB and must not occlude the scene behind its transparent center.
        context.update_opaque_regions((self.format == Fourcc::Xrgb8888).then(|| vec![whole]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::{
        Bind, ExportMem, Offscreen,
        damage::OutputDamageTracker,
        element::{Element, Id},
        pixman::PixmanRenderer,
    };

    fn draw_calibration(
        cache: &mut CachedWidget,
        renderer: &mut PixmanRenderer,
        size: (i32, i32),
        palette: &templar_types::appearance::Palette,
    ) -> (u32, u32, Id) {
        let mut scratch = WindowBuffer::new(size.0 as u32, size.1 as u32);
        let element = cache
            .ensure(
                WidgetKey::new("calibration", super::super::paint::opaque(palette.text)),
                size,
                1.0,
                (0.0, 0.0).into(),
                &mut scratch,
                renderer,
                |painter, width, height| {
                    super::super::identification::paint(
                        painter,
                        width,
                        height,
                        1,
                        "",
                        true,
                        templar_types::display::Insets::symmetric(20, 10),
                        None,
                        palette,
                    )
                },
            )
            .expect("native Pixman import");
        let id = element.id().clone();
        let mut target = renderer
            .create_buffer(Fourcc::Argb8888, size.into())
            .unwrap();
        let mut framebuffer = renderer.bind(&mut target).unwrap();
        let mut tracker = OutputDamageTracker::new(size, 1.0, Transform::Normal);
        tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &[element],
                [0.0, 0.0, 1.0, 1.0],
            )
            .unwrap();
        let mapping = renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size(size.into()),
                Fourcc::Argb8888,
            )
            .unwrap();
        let bytes = renderer.map_texture(&mapping).unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = (y * size.0 as usize + x) * 4;
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
        };
        (pixel(80, 100), pixel(20, 10), id)
    }

    #[test]
    fn calibration_cache_preserves_scene_and_visible_marks_across_appearance_and_resize() {
        let mut renderer = PixmanRenderer::new().unwrap();
        let mut cache = CachedWidget::new_transparent();
        let mut first_id = None;
        for (size, palette) in [
            ((320, 200), &templar_types::appearance::LIGHT),
            ((320, 200), &templar_types::appearance::DARK),
            ((400, 240), &templar_types::appearance::DARK),
        ] {
            let (scene, mark, id) = draw_calibration(&mut cache, &mut renderer, size, palette);
            assert_eq!(
                scene, 0xff00_00ff,
                "transparent calibration must preserve the scene"
            );
            assert_eq!(mark >> 24, 0xff, "calibration marks must remain opaque");
            assert_ne!(mark, scene, "a corner mark must be visible over the scene");
            assert_eq!(
                &id,
                first_id.get_or_insert(id.clone()),
                "appearance and resize must retain the imported buffer"
            );
        }
    }
}
