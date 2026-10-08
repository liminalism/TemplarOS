use anyhow::{Result, bail};
use templar_types::wallpaper::{PixelFormat, RasterDescriptor, SizingMode, TextureLimit};

#[derive(Debug)]
pub struct Sampler<'a> {
    raster: &'a RasterDescriptor,
    bytes: &'a [u8],
    geometry: crate::geometry::Geometry,
    ground: [u8; 4],
}
impl<'a> Sampler<'a> {
    pub fn new(raster: &'a RasterDescriptor, bytes: &'a [u8], output: (u32, u32), sizing: SizingMode, ground: [u8; 4]) -> Result<Self> {
        Ok(Self { raster, bytes, geometry: crate::geometry::layout((raster.source_width, raster.source_height), output, sizing)?, ground })
    }
    fn texel(&self, x: u32, y: u32) -> [u8; 4] {
        let bpp = if self.raster.format == PixelFormat::Rgba8 { 4 } else { 1 };
        let offset = y as usize * self.raster.stride as usize + x as usize * bpp;
        match self.raster.format {
            PixelFormat::Rgba8 => self.bytes[offset..offset + 4].try_into().expect("validated RGBA pixel"),
            PixelFormat::Indexed8 => self.raster.palette[self.bytes[offset] as usize],
        }
    }
    /// Resolve indexed colors before interpolation, just as the native shader does.
    pub fn at(&self, x: f64, y: f64) -> [u8; 4] {
        let [dx, dy, dw, dh] = self.geometry.destination;
        let [sx, sy, sw, sh] = self.geometry.source;
        let (u, v) = if let Some([tw, th]) = self.geometry.tile {
            (x.rem_euclid(f64::from(tw)) / f64::from(tw), y.rem_euclid(f64::from(th)) / f64::from(th))
        } else {
            if x < dx || y < dy || x >= dx + dw || y >= dy + dh { return self.ground; }
            (sx + (x - dx) / dw * sw, sy + (y - dy) / dh * sh)
        };
        let fx = (u * f64::from(self.raster.width) - 0.5).clamp(0.0, f64::from(self.raster.width - 1));
        let fy = (v * f64::from(self.raster.height) - 0.5).clamp(0.0, f64::from(self.raster.height - 1));
        let x0 = fx.floor() as u32; let y0 = fy.floor() as u32;
        let x1 = (x0 + 1).min(self.raster.width - 1); let y1 = (y0 + 1).min(self.raster.height - 1);
        let a = self.texel(x0, y0); let b = self.texel(x1, y0);
        let c = self.texel(x0, y1); let d = self.texel(x1, y1);
        let wx = fx - f64::from(x0); let wy = fy - f64::from(y0);
        let mut color = [0, 0, 0, 255];
        for channel in 0..3 {
            let top = f64::from(a[channel]) * (1.0 - wx) + f64::from(b[channel]) * wx;
            let bottom = f64::from(c[channel]) * (1.0 - wx) + f64::from(d[channel]) * wx;
            color[channel] = (top * (1.0 - wy) + bottom * wy).round() as u8;
        }
        color
    }
}

/// A reduced *output-aspect* canvas, not a resized source masquerading as exact-size
/// geometry. All samples still use original oriented physical source dimensions.
pub fn canvas_size(raster: &RasterDescriptor, output: (u32, u32), sizing: SizingMode, texture: TextureLimit, viewporter: bool) -> Result<(u32, u32)> {
    if !viewporter {
        if texture != TextureLimit::Native { bail!("reduced buffers require wp_viewporter"); }
        return Ok(output);
    }
    let geometry = crate::geometry::layout((raster.source_width, raster.source_height), output, sizing)?;
    let mut factor = match sizing {
        SizingMode::Fill | SizingMode::Fit | SizingMode::Stretch => {
            let visible_w = f64::from(raster.width) * geometry.source[2];
            let visible_h = f64::from(raster.height) * geometry.source[3];
            (visible_w / geometry.destination[2]).max(visible_h / geometry.destination[3]).min(1.0)
        }
        SizingMode::Center | SizingMode::Actual | SizingMode::Tile => {
            (f64::from(raster.width) / f64::from(raster.source_width))
                .max(f64::from(raster.height) / f64::from(raster.source_height)).min(1.0)
        }
    };
    match texture {
        TextureLimit::Native => {}
        TextureLimit::MaxEdge { pixels } => factor = factor.min(f64::from(pixels) / f64::from(output.0.max(output.1))),
        TextureLimit::MaxPixels { pixels } => factor = factor.min((pixels as f64 / (f64::from(output.0) * f64::from(output.1))).sqrt()),
    }
    let width = (f64::from(output.0) * factor).floor().max(1.0) as u32;
    let height = (f64::from(output.1) * factor).floor().max(1.0) as u32;
    // Reuse the core minimum-one/aspect correction for extreme MaxPixels limits.
    crate::geometry::texture_dimensions((width, height), texture)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raster() -> RasterDescriptor {
        RasterDescriptor { version: 1, width: 10, height: 5, source_width: 100, source_height: 50, stride: 40, format: PixelFormat::Rgba8, palette: vec![], bytes: 200 }
    }
    #[test]
    fn reduced_texture_preserves_physical_period_and_exact_placement() {
        let raster = RasterDescriptor {
            version: 1, width: 2, height: 1, source_width: 100, source_height: 50,
            stride: 8, format: PixelFormat::Rgba8, palette: vec![], bytes: 8,
        };
        let data = [255, 0, 0, 255, 0, 255, 0, 255];
        let ground = [24, 26, 30, 255];
        let tile = Sampler::new(&raster, &data, (300, 150), SizingMode::Tile, ground).unwrap();
        assert_eq!(tile.at(25.0, 25.0), [255, 0, 0, 255]);
        assert_eq!(tile.at(75.0, 25.0), [0, 255, 0, 255]);
        assert_eq!(tile.at(125.0, 75.0), [255, 0, 0, 255]);
        assert_eq!(tile.at(175.0, 75.0), [0, 255, 0, 255]);
        let centered = Sampler::new(&raster, &data, (300, 150), SizingMode::Center, ground).unwrap();
        assert_eq!(centered.at(125.0, 75.0), [255, 0, 0, 255]);
        assert_eq!(centered.at(175.0, 75.0), [0, 255, 0, 255]);
        assert_eq!(centered.at(99.0, 75.0), ground);
        let actual = Sampler::new(&raster, &data, (300, 150), SizingMode::Actual, ground).unwrap();
        assert_eq!(actual.at(25.0, 25.0), [255, 0, 0, 255]);
        assert_eq!(actual.at(75.0, 25.0), [0, 255, 0, 255]);
        assert_eq!(actual.at(100.0, 25.0), ground);
    }
    #[test]
    fn viewporter_absence_refuses_reduction() {
        assert!(canvas_size(&raster(), (1000, 500), SizingMode::Fill, TextureLimit::MaxEdge { pixels: 100 }, false).is_err());
    }
    #[test]
    fn every_mode_obeys_an_extreme_pixel_ceiling() {
        for mode in [SizingMode::Fill, SizingMode::Fit, SizingMode::Stretch, SizingMode::Center, SizingMode::Actual, SizingMode::Tile] {
            assert_eq!(canvas_size(&raster(), (10000, 1), mode, TextureLimit::MaxPixels { pixels: 1 }, true).unwrap(), (1, 1));
        }
    }
}
