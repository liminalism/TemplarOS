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
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
            gles::GlesRenderer,
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
}

impl WidgetKey {
    pub fn new(text: impl Into<String>, background: u32) -> Self {
        Self {
            text: text.into(),
            secondary: String::new(),
            background,
            variant: 0,
            flags: 0,
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
    buffer: MemoryRenderBuffer,
}

impl Default for CachedWidget {
    fn default() -> Self {
        Self {
            key: None,
            logical_size: (0, 0),
            buffer_scale: 1,
            buffer: MemoryRenderBuffer::new(Fourcc::Xrgb8888, (1, 1), 1, Transform::Normal, None),
        }
    }
}

impl CachedWidget {
    pub fn new() -> Self {
        Self::default()
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
    pub fn ensure(
        &mut self,
        key: WidgetKey,
        logical_size: (i32, i32),
        output_scale: f64,
        location: Point<f64, Physical>,
        scratch: &mut WindowBuffer,
        renderer: &mut GlesRenderer,
        paint: impl FnOnce(&mut Painter<'_>, i32, i32),
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
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

        self.buffer = MemoryRenderBuffer::from_slice(
            &xrgb_pixels_to_bytes(scratch),
            Fourcc::Xrgb8888,
            (pixel_w, pixel_h),
            buffer_scale,
            Transform::Normal,
            // Every chrome widget is drawn fully opaque by design (`chrome/paint.rs`'s module
            // doc: legibility over client content it may sit above must never depend on alpha
            // blending against unknown pixels) — declaring the whole buffer opaque up front is
            // both correct and a real hint to the renderer's damage/occlusion tracking.
            Some(vec![Rectangle::from_size(Size::from((pixel_w, pixel_h)))]),
        );
    }
}

/// `WindowBuffer`'s pixels are `u32` `0x00RRGGBB` (native-endian) — the exact in-memory byte
/// layout DRM's `Fourcc::Xrgb8888` names on the little-endian hardware this compositor targets
/// (every desktop/laptop TemplarOS runs on): `to_le_bytes()` on each pixel gives `[B, G, R, X]`,
/// which is precisely what `GlesRenderer::import_memory` expects for that format
/// (`gles/format.rs`'s `fourcc_to_gl_formats`: `Xrgb8888` imports via `GL_BGRA`/`UNSIGNED_BYTE`).
/// No `unsafe` reinterpretation needed — this is a plain, safe byte-order conversion.
fn xrgb_pixels_to_bytes(buffer: &WindowBuffer) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(buffer.pixels.len() * 4);
    for &pixel in &buffer.pixels {
        bytes.extend_from_slice(&pixel.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb_conversion_preserves_channel_order_for_bgra_upload() {
        let mut buffer = WindowBuffer::new(2, 1);
        buffer.pixels[0] = 0x00_11_22_33; // R=0x11 G=0x22 B=0x33
        buffer.pixels[1] = 0x00_AA_BB_CC;
        let bytes = xrgb_pixels_to_bytes(&buffer);
        assert_eq!(bytes, vec![0x33, 0x22, 0x11, 0x00, 0xCC, 0xBB, 0xAA, 0x00]);
    }

    #[test]
    fn widget_key_builder_round_trips_fields() {
        let key = WidgetKey::new("Waterfox", 0x00_20_20_20)
            .with_secondary("waterfox")
            .with_variant(2);
        assert_eq!(key.text, "Waterfox");
        assert_eq!(key.secondary, "waterfox");
        assert_eq!(key.background, 0x00_20_20_20);
        assert_eq!(key.variant, 2);
    }

    #[test]
    fn identical_keys_are_equal_and_distinct_keys_are_not() {
        let a = WidgetKey::new("Same", 1);
        let b = WidgetKey::new("Same", 1);
        let c = WidgetKey::new("Different", 1);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
