use anyhow::{Result, ensure};
use templar_types::wallpaper::{SizingMode, TextureLimit};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub source: [f64; 4],
    pub destination: [f64; 4],
    pub tile: Option<[u32; 2]>,
}
/// Geometry always uses oriented original dimensions, never reduced storage dimensions.
pub fn layout(source: (u32, u32), output: (u32, u32), sizing: SizingMode) -> Result<Geometry> {
    ensure!(
        source.0 > 0 && source.1 > 0 && output.0 > 0 && output.1 > 0,
        "zero image/output dimensions"
    );
    let (sw, sh, ow, oh) = (
        f64::from(source.0),
        f64::from(source.1),
        f64::from(output.0),
        f64::from(output.1),
    );
    let mut result = Geometry {
        source: [0.0, 0.0, 1.0, 1.0],
        destination: [0.0, 0.0, ow, oh],
        tile: None,
    };
    match sizing {
        SizingMode::Stretch => (),
        SizingMode::Tile => result.tile = Some([source.0, source.1]),
        SizingMode::Fill => {
            let scale = (ow / sw).max(oh / sh);
            let w = ow / (sw * scale);
            let h = oh / (sh * scale);
            result.source = [(1.0 - w) / 2.0, (1.0 - h) / 2.0, w, h];
        }
        SizingMode::Fit => {
            let scale = (ow / sw).min(oh / sh);
            result.destination = [
                (ow - sw * scale) / 2.0,
                (oh - sh * scale) / 2.0,
                sw * scale,
                sh * scale,
            ];
        }
        SizingMode::Center | SizingMode::Actual => {
            let (w, h) = (sw.min(ow), sh.min(oh));
            let centered = sizing == SizingMode::Center;
            result.source = [
                if centered { (sw - w) / (2.0 * sw) } else { 0.0 },
                if centered { (sh - h) / (2.0 * sh) } else { 0.0 },
                w / sw,
                h / sh,
            ];
            result.destination = [
                if centered { (ow - w) / 2.0 } else { 0.0 },
                if centered { (oh - h) / 2.0 } else { 0.0 },
                w,
                h,
            ];
        }
    }
    Ok(result)
}
/// Explicit ceilings reduce storage, preserving aspect; no policy upscales a source.
pub fn texture_dimensions(source: (u32, u32), texture: TextureLimit) -> Result<(u32, u32)> {
    ensure!(source.0 > 0 && source.1 > 0, "zero source dimensions");
    crate::config::validate_texture(texture)?;
    let scale = match texture {
        TextureLimit::Native => 1.0,
        TextureLimit::MaxEdge { pixels } => {
            (f64::from(pixels) / f64::from(source.0.max(source.1))).min(1.0)
        }
        TextureLimit::MaxPixels { pixels } => {
            ((pixels as f64 / (f64::from(source.0) * f64::from(source.1))).sqrt()).min(1.0)
        }
    };
    let mut w = (f64::from(source.0) * scale).floor().max(1.0) as u32;
    let mut h = (f64::from(source.1) * scale).floor().max(1.0) as u32;
    if let TextureLimit::MaxPixels { pixels } = texture {
        // Extreme aspect ratios need an explicit correction after the minimum-one clamp.
        if u64::from(w) * u64::from(h) > pixels {
            if w >= h {
                w = (pixels / u64::from(h)).max(1) as u32;
            } else {
                h = (pixels / u64::from(w)).max(1) as u32;
            }
        }
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "these dyadic crops and integer destinations are exact"
    )]
    fn exact_size_is_not_fit() {
        let center = layout((800, 200), (400, 400), SizingMode::Center).unwrap();
        assert_eq!(center.source, [0.25, 0.0, 0.5, 1.0]);
        assert_eq!(center.destination, [0.0, 100.0, 400.0, 200.0]);
        assert_eq!(
            layout((640, 360), (3840, 2160), SizingMode::Tile)
                .unwrap()
                .tile,
            Some([640, 360])
        );
    }
    #[test]
    fn ceilings_never_upscale_or_overflow() {
        assert_eq!(
            texture_dimensions((640, 360), TextureLimit::MaxEdge { pixels: 1920 }).unwrap(),
            (640, 360)
        );
        assert_eq!(
            texture_dimensions((10000, 1), TextureLimit::MaxPixels { pixels: 1 }).unwrap(),
            (1, 1)
        );
    }
}
