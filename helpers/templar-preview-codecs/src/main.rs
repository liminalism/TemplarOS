//! Native-codec preview companion for `templar-filed`.
//!
//! JPXL and the Templar PDF renderer require Rust 1.97, while the main TemplarOS
//! workspace intentionally stays on the Ubuntu archive's Rust 1.93. This small
//! process is the compatibility seam: it performs one bounded decode and emits
//! a tightly validated RGBA wire image for the service to turn into PNG.

use anyhow::{Context, Result, anyhow, bail};
use jpxl_bitstream::BitReader;
use jpxl_core::limits::{AllocGuard, Limits};
use jpxl_decode::headers::enums::ExtraChannelType;
use templar_pdf_read::{RasterPlane, RasterProduct, RenderSession};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DECODE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PREVIEW_EDGE: u32 = 768;
const WIRE_MAGIC: &[u8; 4] = b"LPV1";

#[derive(Debug)]
struct RgbaRaster {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct AlphaInfo {
    plane: usize,
    associated: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("templar-preview-codecs: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        return usage();
    };
    if command == "--self-check" {
        if args.next().is_some() {
            return usage();
        }
        println!("templar-preview-codecs: JPXL and Templar PDF renderer available");
        return Ok(());
    }

    let input = args.next().map(PathBuf::from).ok_or_else(usage_error)?;
    let output = args.next().map(PathBuf::from).ok_or_else(usage_error)?;
    let edge = args
        .next()
        .ok_or_else(usage_error)?
        .to_string_lossy()
        .parse::<u32>()
        .context("max edge must be an integer")?;
    if args.next().is_some() {
        return usage();
    }
    let edge = edge.clamp(32, MAX_PREVIEW_EDGE);
    let source = read_bounded(&input)?;
    let raster = match command.to_str() {
        Some("jxl") => decode_jxl(&source, edge)?,
        Some("pdf") => render_pdf(source, edge)?,
        _ => return usage(),
    };
    write_wire(&output, &raster)
}

fn usage_error() -> anyhow::Error {
    anyhow!("usage: templar-preview-codecs <jxl|pdf> INPUT OUTPUT MAX_EDGE")
}

fn usage<T>() -> Result<T> {
    Err(usage_error())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if !metadata.is_file() {
        bail!("input is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("input is larger than {MAX_SOURCE_BYTES} bytes");
    }
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

fn decode_jxl(source: &[u8], edge: u32) -> Result<RgbaRaster> {
    let limits = Limits {
        max_pixels: MAX_DECODE_PIXELS,
        max_frames: 1,
        max_alloc_bytes: MAX_DECODE_BYTES,
    };
    let decoded = jpxl_decode::decode(source, &limits).context("JPXL decode failed")?;
    checked_dimensions(decoded.width, decoded.height)?;
    if !matches!(decoded.num_colour_channels, 1 | 3) {
        bail!(
            "JPXL returned {} colour channels; only gray and RGB previews are supported",
            decoded.num_colour_channels
        );
    }

    let alpha = jxl_alpha_info(source, &decoded, &limits);
    let alpha_plane = alpha.and_then(|info| decoded.planes.get(info.plane));
    let pixels = pixel_count(decoded.width, decoded.height)?;
    let mut rgba = Vec::with_capacity(
        pixels
            .checked_mul(4)
            .ok_or_else(|| anyhow!("JPXL RGBA size overflow"))?,
    );
    for index in 0..pixels {
        let alpha_sample = alpha_plane.map_or(255, |plane| scaled_sample(plane, index));
        if decoded.num_colour_channels == 1 {
            let mut gray = scaled_sample(&decoded.planes[0], index);
            if alpha.is_some_and(|info| info.associated) {
                gray = unassociate(gray, alpha_sample);
            }
            rgba.extend_from_slice(&[gray, gray, gray, alpha_sample]);
        } else {
            for plane in decoded.planes.iter().take(3) {
                let mut sample = scaled_sample(plane, index);
                if alpha.is_some_and(|info| info.associated) {
                    sample = unassociate(sample, alpha_sample);
                }
                rgba.push(sample);
            }
            rgba.push(alpha_sample);
        }
    }
    downscale(
        RgbaRaster {
            width: decoded.width,
            height: decoded.height,
            pixels: rgba,
        },
        edge,
    )
}

fn jxl_alpha_info(
    encoded: &[u8],
    image: &jpxl_decode::DecodedImage,
    limits: &Limits,
) -> Option<AlphaInfo> {
    let mut guard = AllocGuard::new(limits);
    let codestream = if jpxl_decode::container::is_container(encoded) {
        Cow::Owned(jpxl_decode::container::extract_codestream(encoded, &mut guard).ok()?)
    } else {
        Cow::Borrowed(encoded)
    };
    let mut reader = BitReader::new(codestream.as_ref());
    let headers = jpxl_decode::decode_image_headers(&mut reader, limits).ok()?;
    let (extra_index, info) = headers
        .metadata
        .ec_info
        .iter()
        .enumerate()
        .find(|(_, info)| info.channel_type == ExtraChannelType::KAlpha)?;
    let plane = image.num_colour_channels.checked_add(extra_index)?;
    image.planes.get(plane)?;
    Some(AlphaInfo {
        plane,
        associated: info.alpha_associated,
    })
}

fn scaled_sample(plane: &jpxl_decode::Plane, index: usize) -> u8 {
    let source_max = plane.max_value().max(1);
    let sample = plane.samples.get(index).copied().unwrap_or(0);
    let sample = u64::try_from(i64::from(sample).clamp(0, i64::from(source_max))).unwrap_or(0);
    let scaled = sample * u64::from(u8::MAX) + u64::from(source_max) / 2;
    u8::try_from(scaled / u64::from(source_max)).unwrap_or(u8::MAX)
}

fn unassociate(value: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        return 0;
    }
    let expanded =
        (u32::from(value) * u32::from(u8::MAX) + u32::from(alpha) / 2) / u32::from(alpha);
    u8::try_from(expanded.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
}

fn render_pdf(source: Vec<u8>, edge: u32) -> Result<RgbaRaster> {
    let session = RenderSession::open(Arc::from(source), None).context("opening PDF")?;
    if session.page_count() == 0 {
        bail!("PDF has no pages");
    }
    let geometry = session
        .page_geometry(0)
        .context("reading first-page geometry")?;
    let (width, height) =
        fitted_dimensions(geometry.display_width(), geometry.display_height(), edge)?;
    let page = session.compile(0).context("compiling first PDF page")?;
    let rendered = session
        .render(&page, &RasterProduct::rgb8(width, height))
        .context("rendering first PDF page with the Templar CPU renderer")?;
    let RasterPlane::Rgb8(surface) = rendered else {
        bail!("Templar PDF renderer returned a non-RGB surface");
    };
    let source_row = usize::try_from(surface.width)
        .ok()
        .and_then(|width| width.checked_mul(3))
        .ok_or_else(|| anyhow!("PDF RGB row size overflow"))?;
    if surface.stride < source_row {
        bail!("Templar PDF renderer returned a short row stride");
    }
    let pixels = pixel_count(surface.width, surface.height)?;
    let mut rgba = Vec::with_capacity(
        pixels
            .checked_mul(4)
            .ok_or_else(|| anyhow!("PDF RGBA size overflow"))?,
    );
    for row in 0..surface.height as usize {
        let start = row
            .checked_mul(surface.stride)
            .ok_or_else(|| anyhow!("PDF row offset overflow"))?;
        let end = start
            .checked_add(source_row)
            .ok_or_else(|| anyhow!("PDF row end overflow"))?;
        let bytes = surface
            .pixels
            .get(start..end)
            .ok_or_else(|| anyhow!("Templar PDF renderer returned a truncated surface"))?;
        for rgb in bytes.chunks_exact(3) {
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], u8::MAX]);
        }
    }
    Ok(RgbaRaster {
        width: surface.width,
        height: surface.height,
        pixels: rgba,
    })
}

fn fitted_dimensions(width: f64, height: f64, edge: u32) -> Result<(u32, u32)> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        bail!("document has invalid first-page dimensions {width}x{height}");
    }
    let scale = f64::from(edge) / width.max(height);
    let fit = |value: f64| {
        let scaled = (value * scale).round();
        if scaled < 1.0 {
            1
        } else if scaled > f64::from(edge) {
            edge
        } else {
            scaled as u32
        }
    };
    Ok((fit(width), fit(height)))
}

fn downscale(source: RgbaRaster, edge: u32) -> Result<RgbaRaster> {
    if source.width.max(source.height) <= edge {
        return Ok(source);
    }
    let scale = f64::from(edge) / f64::from(source.width.max(source.height));
    let width = ((f64::from(source.width) * scale).round() as u32).clamp(1, edge);
    let height = ((f64::from(source.height) * scale).round() as u32).clamp(1, edge);
    let target_pixels = pixel_count(width, height)?;
    let mut pixels = vec![0_u8; target_pixels * 4];
    for y in 0..height {
        let source_y = ((u64::from(y) * u64::from(source.height)) / u64::from(height))
            .min(u64::from(source.height - 1)) as usize;
        for x in 0..width {
            let source_x = ((u64::from(x) * u64::from(source.width)) / u64::from(width))
                .min(u64::from(source.width - 1)) as usize;
            let from = (source_y * source.width as usize + source_x) * 4;
            let to = (y as usize * width as usize + x as usize) * 4;
            let sample = source
                .pixels
                .get(from..from + 4)
                .ok_or_else(|| anyhow!("decoded image has a truncated RGBA plane"))?;
            pixels[to..to + 4].copy_from_slice(sample);
        }
    }
    Ok(RgbaRaster {
        width,
        height,
        pixels,
    })
}

fn checked_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("decoded image has a zero dimension");
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| anyhow!("decoded image dimensions overflow"))?;
    if pixels > MAX_DECODE_PIXELS {
        bail!("decoded image has {pixels} pixels; limit is {MAX_DECODE_PIXELS}");
    }
    Ok(())
}

fn pixel_count(width: u32, height: u32) -> Result<usize> {
    checked_dimensions(width, height)?;
    usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| anyhow!("decoded image is too large for this host"))
}

fn write_wire(path: &Path, raster: &RgbaRaster) -> Result<()> {
    checked_dimensions(raster.width, raster.height)?;
    let expected = pixel_count(raster.width, raster.height)?
        .checked_mul(4)
        .ok_or_else(|| anyhow!("wire image size overflow"))?;
    if raster.pixels.len() != expected {
        bail!(
            "RGBA plane has {} bytes, expected {expected}",
            raster.pixels.len()
        );
    }
    let mut wire = Vec::with_capacity(12 + raster.pixels.len());
    wire.extend_from_slice(WIRE_MAGIC);
    wire.extend_from_slice(&raster.width.to_le_bytes());
    wire.extend_from_slice(&raster.height.to_le_bytes());
    wire.extend_from_slice(&raster.pixels);
    std::fs::write(path, wire).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_dimensions_fit_the_requested_edge() {
        assert_eq!(fitted_dimensions(612.0, 792.0, 768).unwrap(), (593, 768));
        assert_eq!(fitted_dimensions(792.0, 612.0, 512).unwrap(), (512, 396));
        assert!(fitted_dimensions(0.0, 100.0, 512).is_err());
    }

    #[test]
    fn decoded_rgba_is_downscaled_and_bounded() {
        let source = RgbaRaster {
            width: 8,
            height: 4,
            pixels: vec![255; 8 * 4 * 4],
        };
        let preview = downscale(source, 3).unwrap();
        assert_eq!((preview.width, preview.height), (3, 2));
        assert_eq!(preview.pixels.len(), 3 * 2 * 4);
    }
}
