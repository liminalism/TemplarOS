//! Software cursor composition for the DRM backend.
//!
//! A nested compositor gets a host cursor for free, but a session compositor owns the scanout
//! and must paint one itself. This module handles both cursor surfaces supplied by clients and a
//! small built-in arrow for named/default cursors, so input remains usable even when no cursor
//! theme is installed.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
        },
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    render_elements,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Transform},
    wayland::compositor,
};
use std::sync::Mutex;

const CURSOR_WIDTH: i32 = 20;
const CURSOR_HEIGHT: i32 = 28;

render_elements! {
    pub CursorRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// Long-lived upload cache for the built-in cursor.
pub struct CursorRenderer {
    arrow: MemoryRenderBuffer,
}

impl std::fmt::Debug for CursorRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorRenderer").finish_non_exhaustive()
    }
}

impl Default for CursorRenderer {
    fn default() -> Self {
        let pixels = arrow_pixels();
        Self {
            arrow: MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Argb8888,
                (CURSOR_WIDTH, CURSOR_HEIGHT),
                1,
                Transform::Normal,
                None,
            ),
        }
    }
}

impl CursorRenderer {
    /// Cursor elements local to `output_geometry`, front-to-back.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        status: &mut CursorImageStatus,
        pointer_location: Point<f64, Logical>,
        output_geometry: Rectangle<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<CursorRenderElement> {
        if !output_geometry.to_f64().contains(pointer_location) {
            return Vec::new();
        }
        if matches!(status, CursorImageStatus::Surface(surface) if !surface.alive()) {
            *status = CursorImageStatus::default_named();
        }

        let hotspot = match status {
            CursorImageStatus::Surface(surface) => compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .and_then(|attributes| attributes.lock().ok().map(|value| value.hotspot))
                    .unwrap_or_default()
            }),
            CursorImageStatus::Named(_) | CursorImageStatus::Hidden => Point::from((0, 0)),
        };
        let local = pointer_location - output_geometry.loc.to_f64() - hotspot.to_f64();
        let physical: Point<i32, Physical> = local.to_physical(scale).to_i32_round();

        match status {
            CursorImageStatus::Hidden => Vec::new(),
            CursorImageStatus::Named(_) => MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                physical.to_f64(),
                &self.arrow,
                None,
                None,
                None,
                Kind::Cursor,
            )
            .map(CursorRenderElement::Memory)
            .into_iter()
            .collect(),
            CursorImageStatus::Surface(surface) => {
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    surface,
                    physical,
                    scale,
                    1.0,
                    Kind::Cursor,
                )
            }
        }
    }
}

/// A high-contrast 20×28 arrow: white outline, dark centre, no theme dependency.
fn arrow_pixels() -> Vec<u8> {
    let mut pixels = vec![0; (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize];
    for y in 0..CURSOR_HEIGHT {
        for x in 0..CURSOR_WIDTH {
            let arrow = y <= 20 && x <= (y / 2 + 1).min(11);
            let stem = (13..=25).contains(&y) && (6..=10).contains(&x);
            if !arrow && !stem {
                continue;
            }
            let outline = x == 0
                || y == 0
                || (arrow && (x == (y / 2 + 1).min(11) || y == 20))
                || (stem && (x == 6 || x == 10 || y == 25));
            let value = if outline { 0xff } else { 0x18 };
            let offset = ((y * CURSOR_WIDTH + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&[value, value, value, 0xff]);
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_cursor_has_visible_and_transparent_pixels() {
        let pixels = arrow_pixels();
        assert_eq!(pixels.len(), (CURSOR_WIDTH * CURSOR_HEIGHT * 4) as usize);
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 0xff));
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 0));
    }
}
