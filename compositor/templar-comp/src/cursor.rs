//! Software cursor composition for the DRM backend.
//!
//! A nested compositor gets a host cursor for free, but a session compositor owns the scanout
//! and must paint one itself. This module handles both cursor surfaces supplied by clients and a
//! small set of built-in shapes for named cursors, so input remains usable even when no cursor
//! theme is installed.
//!
//! The shapes are drawn here rather than loaded, because the one thing a compositor must never
//! depend on to stay usable is a theme being installed. There are five: the arrow, and the four
//! resize directions the window perimeter asks for (`templar_window::ResizeEdges::cursor`). Before
//! those existed every named cursor rendered as the arrow, so dragging a window's corner on real
//! hardware looked exactly like doing nothing.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportAll, ImportMem,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::WaylandSurfaceRenderElement,
            },
        },
    },
    input::pointer::{CursorIcon, CursorImageAttributes, CursorImageStatus},
    render_elements,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Transform},
    wayland::compositor,
};
use std::sync::Mutex;

const CURSOR_WIDTH: i32 = 20;
const CURSOR_HEIGHT: i32 = 28;
/// Resize cursors are square and point both ways, so they are hot in the middle rather than at a
/// tip: the pixel under the pointer is the centre of the double arrow.
const RESIZE_SIZE: i32 = 24;

/// The shapes this module can draw without a theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Builtin {
    Arrow,
    /// `nwse-resize`: the top-left/bottom-right diagonal.
    Nwse,
    /// `nesw-resize`: the top-right/bottom-left diagonal.
    Nesw,
    /// `ew-resize`: horizontal.
    Ew,
    /// `ns-resize`: vertical.
    Ns,
}

impl Builtin {
    /// Which shape a named cursor gets. Every unrecognised name is the arrow — the previous
    /// behaviour for *all* of them, and still the right answer for a name with no drawing here.
    fn for_icon(icon: CursorIcon) -> Self {
        match icon {
            CursorIcon::NwseResize | CursorIcon::NwResize | CursorIcon::SeResize => Self::Nwse,
            CursorIcon::NeswResize | CursorIcon::NeResize | CursorIcon::SwResize => Self::Nesw,
            CursorIcon::EwResize
            | CursorIcon::EResize
            | CursorIcon::WResize
            | CursorIcon::ColResize => Self::Ew,
            CursorIcon::NsResize
            | CursorIcon::NResize
            | CursorIcon::SResize
            | CursorIcon::RowResize => Self::Ns,
            _ => Self::Arrow,
        }
    }

    /// The direction the double arrow runs in, or `None` for the arrow, which is not one.
    fn direction(self) -> Option<(f32, f32)> {
        match self {
            Self::Arrow => None,
            Self::Nwse => Some((1.0, 1.0)),
            Self::Nesw => Some((1.0, -1.0)),
            Self::Ew => Some((1.0, 0.0)),
            Self::Ns => Some((0.0, 1.0)),
        }
    }

    fn pixels(self) -> (Vec<u8>, i32, i32) {
        match self.direction() {
            None => (arrow_pixels(), CURSOR_WIDTH, CURSOR_HEIGHT),
            Some((dx, dy)) => (double_arrow_pixels(dx, dy), RESIZE_SIZE, RESIZE_SIZE),
        }
    }

    /// Where the pointer actually is within the image. The arrow points from its own top-left
    /// corner; a double arrow is symmetric, so it is hot in the middle.
    fn hotspot(self) -> Point<i32, Logical> {
        match self.direction() {
            None => Point::from((0, 0)),
            Some(_) => Point::from((RESIZE_SIZE / 2, RESIZE_SIZE / 2)),
        }
    }

    const ALL: [Self; 5] = [Self::Arrow, Self::Nwse, Self::Nesw, Self::Ew, Self::Ns];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|shape| *shape == self)
            .expect("ALL lists every shape")
    }
}

render_elements! {
    pub CursorRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

/// Long-lived upload cache for the built-in cursors, one buffer per [`Builtin`].
pub struct CursorRenderer {
    shapes: [MemoryRenderBuffer; Builtin::ALL.len()],
}

impl std::fmt::Debug for CursorRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorRenderer").finish_non_exhaustive()
    }
}

impl Default for CursorRenderer {
    fn default() -> Self {
        Self {
            shapes: Builtin::ALL.map(|shape| {
                let (pixels, width, height) = shape.pixels();
                MemoryRenderBuffer::from_slice(
                    &pixels,
                    Fourcc::Argb8888,
                    (width, height),
                    1,
                    Transform::Normal,
                    None,
                )
            }),
        }
    }
}

impl CursorRenderer {
    /// Cursor elements local to `output_geometry`, front-to-back.
    pub fn elements<R: ImportAll + ImportMem>(
        &mut self,
        renderer: &mut R,
        status: &mut CursorImageStatus,
        pointer_location: Point<f64, Logical>,
        output_geometry: Rectangle<i32, Logical>,
        scale: Scale<f64>,
    ) -> Vec<CursorRenderElement<R>>
    where
        R::TextureId: Clone + Send + 'static,
    {
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
            CursorImageStatus::Named(icon) => Builtin::for_icon(*icon).hotspot(),
            CursorImageStatus::Hidden => Point::from((0, 0)),
        };
        let local = pointer_location - output_geometry.loc.to_f64() - hotspot.to_f64();
        let physical: Point<i32, Physical> = local.to_physical(scale).to_i32_round();

        match status {
            CursorImageStatus::Hidden => Vec::new(),
            // Matched on the name, not ignored: every named cursor used to render as the arrow,
            // so the compositor's own resize gestures showed nothing on real hardware.
            CursorImageStatus::Named(icon) => MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                physical.to_f64(),
                &self.shapes[Builtin::for_icon(*icon).index()],
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

/// A double-headed arrow running along `(dx, dy)`: a shaft with a head at each end, drawn as a
/// dark core inside a white outline so it is legible over any wallpaper — the same two values the
/// arrow uses, for the same reason.
///
/// Rasterised from the line rather than from a sprite table: four directions from one function is
/// less code than four hand-plotted bitmaps, and the shape stays symmetric about the hotspot by
/// construction, which is the property the pointer's accuracy depends on.
fn double_arrow_pixels(dx: f32, dy: f32) -> Vec<u8> {
    let size = RESIZE_SIZE;
    let centre = (size as f32 - 1.0) / 2.0;
    let length = dx.hypot(dy);
    let (ux, uy) = (dx / length, dy / length);
    // The tips stop one pixel short of the buffer so the outline is never clipped.
    let reach = centre - 1.0;
    let head = 6.0;
    let shaft = 1.6;

    let mut pixels = vec![0; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 - centre, y as f32 - centre);
            let along = px * ux + py * uy;
            let across = (py * ux - px * uy).abs();
            let from_tip = reach - along.abs();
            if from_tip < 0.0 {
                continue;
            }
            // Inside a head the arrow widens towards the shaft; beyond one it is the shaft.
            let half = if from_tip < head {
                from_tip * 0.75
            } else {
                shaft
            };
            if across > half {
                continue;
            }
            let value = if across <= (half - 1.2).max(0.0) {
                0x18
            } else {
                0xff
            };
            let offset = ((y * size + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&[value, value, value, 0xff]);
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_cursor_has_visible_and_transparent_pixels() {
        for shape in Builtin::ALL {
            let (pixels, width, height) = shape.pixels();
            assert_eq!(pixels.len(), (width * height * 4) as usize, "{shape:?}");
            assert!(
                pixels.chunks_exact(4).any(|pixel| pixel[3] == 0xff),
                "{shape:?} draws nothing"
            );
            assert!(
                pixels.chunks_exact(4).any(|pixel| pixel[3] == 0),
                "{shape:?} is a solid block"
            );
            // Both an outline and a core, or it is a silhouette on whatever is behind it.
            let values: Vec<u8> = pixels
                .chunks_exact(4)
                .filter(|pixel| pixel[3] == 0xff)
                .map(|pixel| pixel[0])
                .collect();
            assert!(
                values.contains(&0xff) && values.contains(&0x18),
                "{shape:?} has no outline"
            );
        }
    }

    /// The four resize shapes are distinct images, and each is symmetric about the point the
    /// pointer is actually at — a double arrow that pulls to one side reads as pointing there.
    #[test]
    fn the_resize_cursors_differ_and_are_centred() {
        let drawn: Vec<Vec<u8>> = [Builtin::Nwse, Builtin::Nesw, Builtin::Ew, Builtin::Ns]
            .iter()
            .map(|shape| shape.pixels().0)
            .collect();
        for (i, a) in drawn.iter().enumerate() {
            for b in drawn.iter().skip(i + 1) {
                assert_ne!(a, b, "two resize cursors are the same image");
            }
            // Rotating the image 180 degrees leaves it unchanged.
            let mirrored: Vec<u8> = a.chunks_exact(4).rev().flat_map(<[u8]>::to_vec).collect();
            assert_eq!(
                *a, mirrored,
                "resize cursor {i} is not symmetric about its hotspot"
            );
        }
        assert_eq!(
            Builtin::Nwse.hotspot(),
            Point::from((RESIZE_SIZE / 2, RESIZE_SIZE / 2))
        );
        assert_eq!(Builtin::Arrow.hotspot(), Point::from((0, 0)));
    }

    #[test]
    fn a_named_cursor_with_no_drawing_is_the_arrow() {
        assert_eq!(Builtin::for_icon(CursorIcon::Default), Builtin::Arrow);
        assert_eq!(Builtin::for_icon(CursorIcon::Text), Builtin::Arrow);
        assert_eq!(Builtin::for_icon(CursorIcon::NwseResize), Builtin::Nwse);
        assert_eq!(Builtin::for_icon(CursorIcon::NeswResize), Builtin::Nesw);
        assert_eq!(Builtin::for_icon(CursorIcon::EwResize), Builtin::Ew);
        assert_eq!(Builtin::for_icon(CursorIcon::NsResize), Builtin::Ns);
    }
}
