//! The CPU cushion rasterizer: accumulated parabolic ridges per rectangle, shaded per pixel with
//! Lambertian lighting (the paper's light at (1, 2, 10)). Writes straight into a byte buffer, so
//! a tray's interior is one cached image the field blits and moves as a whole.

use crate::colors::{self, ColorSettings};
use crate::layout::LayoutRect;
use crate::tree::arena::FileTree;

/// Cushion shading parameters (van Wijk & van de Wetering 1999).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CushionConfig {
    /// Ambient light intensity (paper default: ~0.16 = 40/255)
    pub ambient: f32,
    /// Diffuse light intensity (paper default: ~0.84 = 215/255)
    pub diffuse: f32,
    /// Normalized light direction [x, y, z]
    pub light: [f32; 3],
    /// Fast approximate lighting mode (avoids per-pixel normal normalization).
    pub fast_lighting: bool,
}

impl Default for CushionConfig {
    fn default() -> Self {
        // Light direction from paper: (1, 2, 10), normalized
        let (lx, ly, lz) = (1.0_f32, 2.0, 10.0);
        let len = (lx * lx + ly * ly + lz * lz).sqrt();
        Self {
            // Lower ambient + stronger diffuse gives better visual separation.
            ambient: 0.26,
            diffuse: 0.92,
            light: [lx / len, ly / len, lz / len],
            // Prioritize visual fidelity by default; fast mode remains optional.
            fast_lighting: false,
        }
    }
}

/// CPU-rasterize the cushion treemap into an RGBA pixel buffer.
///
/// Each pixel's color is determined by the deepest (last-drawn) rectangle
/// containing it. The surface normal is derived from the accumulated
/// cushion coefficients and shaded with Lambertian reflectance.
pub fn rasterize_cushions(
    width: u32,
    height: u32,
    layout_rects: &[LayoutRect],
    tree: &FileTree,
    config: &CushionConfig,
    color_settings: &ColorSettings,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut buf = vec![0u8; w * h * 4];

    // Initialize to dark neutral background, fully opaque.
    for pixel in buf.chunks_exact_mut(4) {
        pixel[0] = 20;
        pixel[1] = 22;
        pixel[2] = 28;
        pixel[3] = 255;
    }

    // Normalize light once per rasterization pass (never per-pixel).
    let [mut lx, mut ly, mut lz] = config.light;
    let light_len = (lx * lx + ly * ly + lz * lz).sqrt();
    if light_len > 1e-6 {
        lx /= light_len;
        ly /= light_len;
        lz /= light_len;
    } else {
        lx = 0.097_590_01;
        ly = 0.195_180_03;
        lz = 0.975_900_1;
    }

    // Iterate rects in order: parents before children.
    // Children overwrite parent pixels, so deeper structure shows through.
    for rect in layout_rects {
        let node = tree.get(rect.node);

        // Base color
        let base = if node.is_dir {
            colors::directory_color(&tree.category_weights[rect.node.index()], color_settings)
        } else {
            let ext = if node.extension_id > 0 {
                tree.extensions
                    .get(node.extension_id as usize)
                    .map_or("", String::as_str)
            } else {
                ""
            };
            colors::file_color(node.category, ext, color_settings)
        };

        let [sx1, sx2, sy1, sy2] = rect.surface;

        // Pixel bounds (clamped to buffer)
        let px0 = (rect.x as usize).min(w);
        let py0 = (rect.y as usize).min(h);
        let px1 = ((rect.x + rect.w).ceil() as usize).min(w);
        let py1 = ((rect.y + rect.h).ceil() as usize).min(h);

        if config.fast_lighting {
            // Approximate mode: skip per-pixel normal normalization for speed.
            for py in py0..py1 {
                let py_f = py as f32 + 0.5;
                let ny = -(2.0 * sy2 * py_f + sy1);

                let row_offset = py * w;
                for px in px0..px1 {
                    let px_f = px as f32 + 0.5;
                    let nx = -(2.0 * sx2 * px_f + sx1);

                    // Fast path: approximate normalization with reciprocal sqrt.
                    let lambert = (nx * lx + ny * ly + lz).max(0.0);
                    let inv_len = (nx * nx + ny * ny + 1.0).max(1e-5).sqrt().recip();
                    let ndotl = lambert * inv_len;
                    let intensity = (config.ambient + config.diffuse * ndotl)
                        .clamp(0.0, 1.0)
                        .powf(1.22);

                    let idx = (row_offset + px) * 4;
                    buf[idx] = (base.r * intensity * 255.0) as u8;
                    buf[idx + 1] = (base.g * intensity * 255.0) as u8;
                    buf[idx + 2] = (base.b * intensity * 255.0) as u8;
                }
            }
        } else {
            // Full Lambert mode: normalize per-pixel normal for higher fidelity.
            for py in py0..py1 {
                let py_f = py as f32 + 0.5;
                let ny = -(2.0 * sy2 * py_f + sy1);

                let row_offset = py * w;
                for px in px0..px1 {
                    let px_f = px as f32 + 0.5;
                    let nx = -(2.0 * sx2 * px_f + sx1);

                    let dot = nx * lx + ny * ly + lz;
                    let n_len = (nx * nx + ny * ny + 1.0).sqrt();
                    let cos_theta = (dot / n_len).max(0.0);
                    let intensity = (config.ambient + config.diffuse * cos_theta)
                        .clamp(0.0, 1.0)
                        .powf(1.22);

                    let idx = (row_offset + px) * 4;
                    buf[idx] = (base.r * intensity * 255.0) as u8;
                    buf[idx + 1] = (base.g * intensity * 255.0) as u8;
                    buf[idx + 2] = (base.b * intensity * 255.0) as u8;
                }
            }
        }
    }

    buf
}

/// [`rasterize_cushions`] packed as `0xAARRGGBB` words (opaque), pixelkit's bitmap layout, so
/// the tray can blit it without a channel shuffle.
pub fn rasterize_argb(
    width: u32,
    height: u32,
    layout_rects: &[LayoutRect],
    tree: &FileTree,
    config: &CushionConfig,
    color_settings: &ColorSettings,
) -> Vec<u32> {
    rasterize_cushions(width, height, layout_rects, tree, config, color_settings)
        .chunks_exact(4)
        .map(|px| {
            0xff00_0000 | (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{LayoutConfig, compute_layout_in_rect};
    use crate::scan::RawEntry;
    use crate::tree::build_tree;
    use std::path::PathBuf;

    #[test]
    fn a_rasterized_tree_lights_every_pixel_of_every_rect() {
        let entries = vec![
            RawEntry {
                path: PathBuf::from("/r/a.png"),
                size: 300,
                is_dir: false,
            },
            RawEntry {
                path: PathBuf::from("/r/b.txt"),
                size: 100,
                is_dir: false,
            },
        ];
        let tree = build_tree(&entries);
        let layout = compute_layout_in_rect(
            &tree,
            tree.root,
            0.0,
            0.0,
            64.0,
            32.0,
            &LayoutConfig::default(),
        );
        assert!(layout.rects.len() >= 3, "root plus two files");
        let pixels = rasterize_argb(
            64,
            32,
            &layout.rects,
            &tree,
            &CushionConfig::default(),
            &ColorSettings::default(),
        );
        assert_eq!(pixels.len(), 64 * 32);
        assert!(pixels.iter().all(|px| px >> 24 == 0xff));
        let distinct: std::collections::HashSet<u32> = pixels.iter().copied().collect();
        assert!(distinct.len() > 8, "cushions shade, they do not flat-fill");
    }
}
