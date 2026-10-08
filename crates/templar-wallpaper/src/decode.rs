//! Temporary pure-Rust codecs. Untagged 8-bit inputs are interpreted as sRGB;
//! ICC/HDR/non-sRGB declarations are refused, not silently relabeled.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::{BufReader, Read, Seek, SeekFrom}, os::fd::AsRawFd, ptr::NonNull};
use templar_types::wallpaper::{PixelFormat, RasterDescriptor, TextureLimit};

const OVERHEAD: u64 = 16 * 1024 * 1024;
const PNG_CODEC_LIMIT: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DecodeOptions {
    pub texture: TextureLimit,
    pub ground: [u8; 4],
    pub worker_budget_bytes: u64,
    pub quantize_colors: Option<u16>,
}
impl Default for DecodeOptions {
    fn default() -> Self { Self { texture: TextureLimit::Native, ground: [24, 26, 30, 255], worker_budget_bytes: 192 * 1024 * 1024, quantize_colors: Some(256) } }
}
#[derive(Debug)]
struct WritableMap { pointer: NonNull<u8>, length: usize }
#[allow(unsafe_code)]
impl WritableMap {
    fn new(file: &File, length: u64) -> Result<Self> {
        let length = usize::try_from(length)?;
        ensure!(length > 0 && length <= isize::MAX as usize, "invalid writable raster size");
        file.set_len(length as u64)?;
        // SAFETY: file has exact nonzero mapping length and the map is uniquely owned.
        let pointer = unsafe { libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        ensure!(pointer != libc::MAP_FAILED, "map output: {}", std::io::Error::last_os_error());
        Ok(Self { pointer: NonNull::new(pointer.cast()).context("null output mapping")?, length })
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: unique mutable borrow covers a live writable mapping.
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) }
    }
}
#[allow(unsafe_code)]
impl Drop for WritableMap {
    fn drop(&mut self) {
        // SAFETY: address/length belong to this unique mapping.
        unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.length); }
    }
}
fn pixels(dimensions: (u32, u32)) -> u64 { u64::from(dimensions.0) * u64::from(dimensions.1) }
fn oriented(dimensions: (u32, u32), orientation: u8) -> (u32, u32) { if orientation >= 5 { (dimensions.1, dimensions.0) } else { dimensions } }
fn oriented_position(x: u32, y: u32, width: u32, height: u32, orientation: u8) -> (u32, u32) {
    match orientation {
        2 => (width - 1 - x, y), 3 => (width - 1 - x, height - 1 - y), 4 => (x, height - 1 - y),
        5 => (y, x), 6 => (height - 1 - y, x), 7 => (height - 1 - y, width - 1 - x), 8 => (y, width - 1 - x), _ => (x, y),
    }
}
fn plan(options: &DecodeOptions, source: (u32, u32), orientation: u8, codec_bytes: u64, encoded_bytes: u64) -> Result<((u32, u32), u64)> {
    ensure!(source.0 > 0 && source.1 > 0 && source.0 <= 65535 && source.1 <= 65535, "source dimensions outside 1..=65535 resource envelope");
    let dimensions = crate::geometry::texture_dimensions(oriented(source, orientation), options.texture)?;
    let count = pixels(dimensions);
    let output_bytes = count.checked_mul(4).context("output overflow")?;
    let quantize = options.quantize_colors.map_or(Ok(0), |colors| {
        crate::quantize::scratch_bytes(crate::quantize::QuantizeOptions { colors, ..Default::default() })
            .map(|bytes| bytes as u64).map_err(anyhow::Error::from)
    })?;
    let rows = u64::from(source.0.max(dimensions.0).max(dimensions.1)) * 64;
    let total = OVERHEAD.checked_add(codec_bytes).and_then(|v| v.checked_add(encoded_bytes * 2)).and_then(|v| v.checked_add(output_bytes)).and_then(|v| v.checked_add(rows)).and_then(|v| v.checked_add(quantize)).context("worker allocation plan overflow")?;
    ensure!(total <= options.worker_budget_bytes, "image requires conservative worker envelope {total} bytes, budget is {}; use an explicit texture ceiling or a pre-sized source", options.worker_budget_bytes);
    Ok((dimensions, count))
}
/// Reads only already-open descriptors. The host must reap the worker and seal output.
pub fn decode_to_fd(mut source: File, output: File, options: DecodeOptions) -> Result<RasterDescriptor> {
    crate::config::validate_texture(options.texture)?;
    ensure!(options.ground[3] == 255, "ground must be opaque");
    ensure!((16 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&options.worker_budget_bytes), "worker budget outside 16 MiB..=2 GiB");
    if let Some(colors) = options.quantize_colors { ensure!((1..=256).contains(&colors), "palette size must be 1..=256"); }
    let encoded = source.metadata()?.len();
    ensure!(source.metadata()?.is_file() && encoded > 0 && encoded <= 64 * 1024 * 1024 && encoded <= options.worker_budget_bytes / 2, "compressed source exceeds 64 MiB/input-budget envelope");
    source.seek(SeekFrom::Start(0))?;
    let mut signature = [0; 12];
    source.read_exact(&mut signature)?;
    source.seek(SeekFrom::Start(0))?;
    if signature.starts_with(b"\x89PNG\r\n\x1a\n") { decode_png(source, output, &options, encoded) }
    else if signature.starts_with(&[0xff, 0xd8]) { decode_jpeg(source, output, &options, encoded) }
    else if &signature[..4] == b"RIFF" && &signature[8..12] == b"WEBP" { decode_webp(source, output, &options, encoded) }
    else { bail!("supported wallpaper codecs are 8-bit PNG, JPEG and WebP") }
}
struct Target {
    map: WritableMap,
    staging: Option<File>,
    descriptor: RasterDescriptor,
    orientation: u8,
    channels: usize,
}
impl Target {
    fn new(output: &File, source: (u32, u32), dimensions: (u32, u32), orientation: u8, options: &DecodeOptions) -> Result<Self> {
        let source = oriented(source, orientation);
        let quantized = options.quantize_colors.is_some();
        let channels = if quantized { 3 } else { 4 };
        let staging = if quantized { Some(crate::raster::create_memfd()?) } else { None };
        let map = WritableMap::new(staging.as_ref().unwrap_or(output), pixels(dimensions) * channels as u64)?;
        let descriptor = RasterDescriptor { version: 1, width: dimensions.0, height: dimensions.1, source_width: source.0, source_height: source.1, stride: dimensions.0.checked_mul(if quantized { 1 } else { 4 }).context("stride overflow")?, format: if quantized { PixelFormat::Indexed8 } else { PixelFormat::Rgba8 }, palette: vec![], bytes: pixels(dimensions) * if quantized { 1 } else { 4 } };
        Ok(Self { map, staging, descriptor, orientation, channels })
    }
    fn row(&mut self, y: u32, width: u32, height: u32, colors: &[[f64; 3]]) {
        for (x, color) in colors.iter().enumerate() {
            let x = x as u32;
            let (ox, oy) = oriented_position(x, y, width, height, self.orientation);
            let offset = (oy as usize * self.descriptor.width as usize + ox as usize) * self.channels;
            let bytes = self.map.bytes_mut();
            for c in 0..3 { bytes[offset + c] = encode_srgb(color[c]); }
            if self.channels == 4 { bytes[offset + 3] = 255; }
        }
    }
    /// Native opaque sRGB needs only a byte permutation, not the linear-light reducer.
    fn orient_rgb(&mut self, rgb: &[u8], source: (u32, u32)) -> Result<()> {
        ensure!(rgb.len() as u64 == pixels(source) * 3, "native JPEG RGB staging length mismatch");
        ensure!((self.descriptor.width, self.descriptor.height) == oriented(source, self.orientation), "native JPEG oriented dimensions mismatch");
        let channels = self.channels;
        let width = self.descriptor.width as usize;
        let orientation = self.orientation;
        let bytes = self.map.bytes_mut();
        for (y, row) in rgb.chunks_exact(source.0 as usize * 3).enumerate() {
            for (x, pixel) in row.chunks_exact(3).enumerate() {
                let (ox, oy) = oriented_position(x as u32, y as u32, source.0, source.1, orientation);
                let offset = (oy as usize * width + ox as usize) * channels;
                bytes[offset..offset + 3].copy_from_slice(pixel);
                if channels == 4 { bytes[offset + 3] = 255; }
            }
        }
        Ok(())
    }
    fn finish(mut self, output: &File, options: &DecodeOptions) -> Result<RasterDescriptor> {
        if let Some(colors) = options.quantize_colors {
            let mut indices = WritableMap::new(output, self.descriptor.bytes)?;
            self.descriptor.palette = crate::quantize::quantize_rgb_into(self.map.bytes_mut(), self.descriptor.width, self.descriptor.height, crate::quantize::QuantizeOptions { colors, ..Default::default() }, indices.bytes_mut()).map_err(|error| anyhow::anyhow!("WuDP: {error}"))?;
            drop(indices);
        }
        crate::raster::validate_descriptor(&self.descriptor)?;
        // Drop writable mappings before returning; the native host can now apply WRITE seals.
        drop(self.map);
        drop(self.staging);
        Ok(self.descriptor)
    }
}
fn linear_srgb(byte: u8) -> f64 { let v = f64::from(byte) / 255.0; if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } }
fn encode_srgb(value: f64) -> u8 { let v = value.clamp(0.0, 1.0); let encoded = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }; (encoded * 255.0).round() as u8 }
/// Exact box/area reduction with a single destination-row accumulator. Never enlarges.
struct Rows {
    source: (u32, u32), destination: (u32, u32), y: u32, output_y: u32,
    horizontal: Vec<[f64; 3]>, accumulated: Vec<[f64; 3]>, linear: [f64; 256], ground: [f64; 3],
}
impl Rows {
    fn new(source: (u32, u32), target: &Target, ground: [u8; 4]) -> Self {
        let destination = oriented((target.descriptor.width, target.descriptor.height), target.orientation);
        let mut linear = [0.0; 256]; for (i, value) in linear.iter_mut().enumerate() { *value = linear_srgb(i as u8); }
        Self { source, destination, y: 0, output_y: 0, horizontal: vec![[0.0; 3]; destination.0 as usize], accumulated: vec![[0.0; 3]; destination.0 as usize], linear, ground: [linear_srgb(ground[0]), linear_srgb(ground[1]), linear_srgb(ground[2])] }
    }
    fn push(&mut self, row: &[u8], channels: usize, target: &mut Target) -> Result<()> {
        ensure!(row.len() == self.source.0 as usize * channels && self.y < self.source.1, "codec row length/count mismatch");
        let ratio_x = f64::from(self.source.0) / f64::from(self.destination.0);
        for (x, result) in self.horizontal.iter_mut().enumerate() {
            *result = [0.0; 3];
            let start = x as f64 * ratio_x; let end = (x + 1) as f64 * ratio_x;
            for sx in start.floor() as u32..(end.ceil() as u32).min(self.source.0) {
                let weight = (end.min(f64::from(sx + 1)) - start.max(f64::from(sx))) / ratio_x;
                let pixel = &row[sx as usize * channels..][..channels];
                let alpha = if channels == 2 { f64::from(pixel[1]) / 255.0 } else if channels == 4 { f64::from(pixel[3]) / 255.0 } else { 1.0 };
                for c in 0..3 { let byte = pixel[if channels <= 2 { 0 } else { c }]; result[c] += (self.linear[usize::from(byte)] * alpha + self.ground[c] * (1.0 - alpha)) * weight; }
            }
        }
        let ratio_y = f64::from(self.source.1) / f64::from(self.destination.1);
        while self.output_y < self.destination.1 {
            let start = f64::from(self.output_y) * ratio_y;
            let end = f64::from(self.output_y + 1) * ratio_y;
            let weight = (end.min(f64::from(self.y + 1)) - start.max(f64::from(self.y))).max(0.0) / ratio_y;
            for (accumulator, color) in self.accumulated.iter_mut().zip(&self.horizontal) { for c in 0..3 { accumulator[c] += color[c] * weight; } }
            if end <= f64::from(self.y + 1) + 1e-9 {
                target.row(self.output_y, self.destination.0, self.destination.1, &self.accumulated);
                self.accumulated.fill([0.0; 3]); self.output_y += 1;
            } else { break; }
        }
        self.y += 1;
        Ok(())
    }
    fn finish(&self) -> Result<()> { ensure!(self.y == self.source.1 && self.output_y == self.destination.1, "incomplete codec raster"); Ok(()) }
}
fn decode_png(mut source: File, output: File, options: &DecodeOptions, encoded: u64) -> Result<RasterDescriptor> {
    let orientation = png_metadata(&mut source, encoded)?;
    source.seek(SeekFrom::Start(0))?;
    let mut decoder = png::Decoder::new_with_limits(BufReader::new(source), png::Limits { bytes: PNG_CODEC_LIMIT });
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder.set_ignore_text_chunk(true);
    let mut reader = decoder.read_info().context("PNG header")?;
    let info = reader.info();
    ensure!(info.bit_depth != png::BitDepth::Sixteen, "16-bit PNG/HDR is outside the 8-bit worker envelope");
    ensure!(info.icc_profile.is_none() && info.coding_independent_code_points.is_none() && info.mastering_display_color_volume.is_none() && info.content_light_level.is_none(), "PNG ICC/HDR color conversion is unsupported; export sRGB first");
    ensure!(info.srgb.is_some() || (info.source_chromaticities.is_none() && info.source_gamma.is_none_or(|gamma| (gamma.into_value() - 0.45455).abs() < 0.00002)), "non-sRGB PNG transfer/primaries are unsupported; export sRGB first");
    let original = (info.width, info.height);
    let interlaced = info.interlaced;
    let raw_bytes = if interlaced { pixels(original) * 4 } else { 0 };
    let (dimensions, _) = plan(options, original, orientation, raw_bytes + PNG_CODEC_LIMIT as u64, encoded)?;
    let mut target = Target::new(&output, original, dimensions, orientation, options)?;
    let (color, depth) = reader.output_color_type();
    ensure!(depth == png::BitDepth::Eight, "PNG transformation did not produce 8-bit samples");
    let channels = match color { png::ColorType::Grayscale => 1, png::ColorType::GrayscaleAlpha => 2, png::ColorType::Rgb => 3, png::ColorType::Rgba => 4, png::ColorType::Indexed => bail!("PNG palette expansion failed") };
    let mut rows = Rows::new(original, &target, options.ground);
    if interlaced {
        let mut frame = vec![0; reader.output_buffer_size()];
        let decoded = reader.next_frame(&mut frame)?;
        ensure!(decoded.width == original.0 && decoded.height == original.1, "PNG frame dimensions mismatch");
        for row in frame[..decoded.buffer_size()].chunks_exact(original.0 as usize * channels) { rows.push(row, channels, &mut target)?; }
    } else {
        while let Some(row) = reader.next_row()? { rows.push(row.data(), channels, &mut target)?; }
    }
    rows.finish()?;
    reader.finish().context("PNG completion")?;
    drop(reader); drop(rows);
    target.finish(&output, options)
}
/// Verify marker framing through EOI and reject late color/orientation declarations.
/// Some codec baseline paths tolerate a missing terminal marker even in strict mode.
fn validate_jpeg_stream(file: &mut File, orientation: u8) -> Result<()> {
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(2))?;
    let mut reader = BufReader::new(file);
    let mut entropy = false;
    let mut scans = 0;
    let mut markers = 0;
    let mut metadata = 0_u64;
    loop {
        let mut byte = [0];
        reader.read_exact(&mut byte).context("JPEG missing complete EOI-terminated stream")?;
        if entropy && byte[0] != 0xff { continue; }
        ensure!(byte[0] == 0xff, "invalid JPEG marker framing");
        while byte[0] == 0xff {
            reader.read_exact(&mut byte).context("truncated JPEG marker")?;
        }
        let marker = byte[0];
        if entropy && (marker == 0 || (0xd0..=0xd7).contains(&marker)) { continue; }
        if marker == 0xd9 {
            ensure!(scans > 0, "JPEG has no image scan");
            return Ok(());
        }
        markers += 1;
        ensure!(markers <= 65536, "JPEG exceeds bounded marker envelope");
        ensure!(marker != 0 && marker != 0x01 && !(0xd0..=0xd8).contains(&marker), "unexpected standalone JPEG marker");
        let mut segment_length = [0; 2];
        reader.read_exact(&mut segment_length)?;
        let segment_length = u16::from_be_bytes(segment_length);
        ensure!(segment_length >= 2, "invalid JPEG segment length");
        let bytes = usize::from(segment_length - 2);
        ensure!(reader.stream_position()?.checked_add(bytes as u64).is_some_and(|end| end <= length), "JPEG segment extends past input");
        metadata += u64::from(segment_length);
        ensure!(metadata <= 4 * 1024 * 1024, "JPEG segments exceed 4 MiB metadata envelope");
        let mut data = vec![0; bytes];
        reader.read_exact(&mut data)?;
        if marker == 0xe2 && data.starts_with(b"ICC_PROFILE\0") {
            bail!("JPEG ICC profile requires unsupported color conversion");
        }
        if marker == 0xe1 && data.starts_with(b"Exif\0\0") {
            ensure!(exif_orientation(&data)? == orientation, "JPEG changes EXIF orientation after initial header");
        }
        entropy = marker == 0xda;
        if entropy {
            scans += 1;
            ensure!(scans <= 100, "JPEG exceeds 100-scan worker envelope");
        }
    }
}
/// Native opaque JPEG goes directly into its target when unchanged. Other EXIF
/// orientations decode into one mapped RGB frame, then permute bytes into the
/// final RGBA map (or packed RGB quantizer staging), without any codec frame Vec.
fn decode_jpeg_native(mut source: File, output: File, options: &DecodeOptions, encoded: u64, original: (u32, u32), orientation: u8) -> Result<RasterDescriptor> {
    use zune_core::{colorspace::ColorSpace, options::DecoderOptions};
    let padded = u64::from(original.0).div_ceil(64) * 64 * u64::from(original.1).div_ceil(64) * 64;
    // Progressive and non-interleaved sequential scans may hold all components'
    // i16 DCT coefficients. Eight bytes/padded pixel includes three full planes
    // plus a conservative margin; scanline/upsampling scratch is additional.
    // plan's 4 bytes/output pixel covers either RGBA or RGB + indexed output.
    // Transformed pixels require just 3 extra bytes/source pixel, never RGBA.
    // This preflights both mappings, all codec scratch and quantizer allocations
    // before decode_headers/decode_into can allocate their coefficient storage.
    let staging_bytes = if orientation == 1 { 0 } else { pixels(original) * 3 };
    let (dimensions, _) = plan(options, original, orientation, padded * 8 + u64::from(original.0) * 512 + staging_bytes, encoded)?;
    validate_jpeg_stream(&mut source, orientation)?;
    source.seek(SeekFrom::Start(0))?;
    let colorspace = if orientation != 1 || options.quantize_colors.is_some() { ColorSpace::RGB } else { ColorSpace::RGBA };
    let codec_options = DecoderOptions::default()
        .set_max_width(65535)
        .set_max_height(65535)
        .set_strict_mode(true)
        .jpeg_set_out_colorspace(colorspace);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(BufReader::new(source), codec_options);
    decoder.decode_headers().context("native JPEG header")?;
    ensure!(decoder.dimensions() == Some((original.0 as usize, original.1 as usize)), "JPEG dimensions changed after preflight");
    ensure!(matches!(decoder.input_colorspace(), Some(ColorSpace::Luma | ColorSpace::RGB | ColorSpace::YCbCr)), "native JPEG requires an 8-bit grayscale/RGB/YCbCr source");
    ensure!(decoder.output_colorspace() == Some(colorspace), "native JPEG output colorspace mismatch");
    ensure!(decoder.icc_profile().is_none(), "JPEG ICC profile requires unsupported color conversion");
    if let Some(exif) = decoder.exif() { ensure!(exif_orientation(exif)? == orientation, "JPEG changes EXIF orientation after initial header"); }
    let expected_bytes = pixels(original) * colorspace.num_components() as u64;
    ensure!(decoder.output_buffer_size().map(|bytes| bytes as u64) == Some(expected_bytes), "native JPEG raster length mismatch");
    // The decoder and its coefficient arrays are gone before allocating the
    // transformed destination; the conservative preflight still includes both.
    let target = if orientation == 1 {
        let mut target = Target::new(&output, original, dimensions, orientation, options)?;
        decoder.decode_into(target.map.bytes_mut()).context("native JPEG decode")?;
        ensure!(decoder.icc_profile().is_none(), "JPEG ICC profile requires unsupported color conversion");
        if let Some(exif) = decoder.exif() { ensure!(exif_orientation(exif)? == orientation, "JPEG changes EXIF orientation during decode"); }
        drop(decoder);
        if target.channels == 4 {
            ensure!(target.map.bytes_mut().chunks_exact(4).all(|pixel| pixel[3] == 255), "native JPEG decoder did not produce opaque RGBA");
        }
        target
    } else {
        let staging_file = crate::raster::create_memfd()?;
        let mut staging = WritableMap::new(&staging_file, staging_bytes)?;
        decoder.decode_into(staging.bytes_mut()).context("native JPEG decode")?;
        ensure!(decoder.icc_profile().is_none(), "JPEG ICC profile requires unsupported color conversion");
        if let Some(exif) = decoder.exif() { ensure!(exif_orientation(exif)? == orientation, "JPEG changes EXIF orientation during decode"); }
        drop(decoder);
        let mut target = Target::new(&output, original, dimensions, orientation, options)?;
        target.orient_rgb(staging.bytes_mut(), original)?;
        drop(staging);
        drop(staging_file);
        target
    };
    target.finish(&output, options)
}
fn decode_jpeg(mut source: File, output: File, options: &DecodeOptions, encoded: u64) -> Result<RasterDescriptor> {
    let (original, orientation) = jpeg_header(&mut source)?;
    source.seek(SeekFrom::Start(0))?;
    let dimensions = crate::geometry::texture_dimensions(oriented(original, orientation), options.texture)?;
    if dimensions == oriented(original, orientation) {
        return decode_jpeg_native(source, output, options, encoded, original, orientation);
    }
    let unrotated = oriented(dimensions, orientation);
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(source));
    decoder.set_max_decoding_buffer_size(usize::try_from(options.worker_budget_bytes)?);
    decoder.read_info()?;
    let info = decoder.info().context("JPEG missing image metadata")?;
    ensure!(matches!(info.pixel_format, jpeg_decoder::PixelFormat::L8 | jpeg_decoder::PixelFormat::RGB24), "CMYK/16-bit JPEG requires unsupported color conversion");
    let scaled = decoder.scale(unrotated.0 as u16, unrotated.1 as u16)?;
    let decoded_size = (u32::from(scaled.0), u32::from(scaled.1));
    let padded = ((u64::from(original.0) + 63) / 64) * 64 * ((u64::from(original.1) + 63) / 64) * 64;
    // Progressive coefficients remain source-sized even with reduced IDCT;
    // sequential decoding instead uses bounded MCU-row coefficient scratch.
    let coefficients = if info.coding_process == jpeg_decoder::CodingProcess::DctProgressive {
        padded * 8
    } else {
        u64::from(original.0) * 256
    };
    plan(options, original, orientation, coefficients + pixels(decoded_size) * 8, encoded)?;
    let frame = decoder.decode().context("JPEG decode")?;
    ensure!(decoder.icc_profile().is_none(), "JPEG ICC profile requires unsupported color conversion");
    if let Some(exif) = decoder.exif_data() { ensure!(exif_orientation(exif)? == orientation, "JPEG changes EXIF orientation after initial header"); }
    let channels = info.pixel_format.pixel_bytes();
    drop(decoder);
    let mut target = Target::new(&output, original, dimensions, orientation, options)?;
    let mut rows = Rows::new(decoded_size, &target, options.ground);
    ensure!(frame.len() == pixels(decoded_size) as usize * channels, "JPEG raster length mismatch");
    for row in frame.chunks_exact(decoded_size.0 as usize * channels) { rows.push(row, channels, &mut target)?; }
    rows.finish()?;
    drop(frame); drop(rows);
    target.finish(&output, options)
}
fn decode_webp(source: File, output: File, options: &DecodeOptions, encoded: u64) -> Result<RasterDescriptor> {
    let mut decoder = image_webp::WebPDecoder::new(BufReader::new(source)).context("WebP header")?;
    decoder.set_memory_limit(usize::try_from(options.worker_budget_bytes / 2)?);
    ensure!(decoder.icc_profile()?.is_none(), "WebP ICC profile requires unsupported color conversion");
    let orientation = decoder.exif_metadata()?.as_deref().map_or(Ok(1), exif_orientation)?;
    let original = decoder.dimensions();
    // Lossless Huffman/transforms and lossy codec allocations are not universally covered
    // by WebP's internal limit: conservative full-source workspace is mandatory.
    let (dimensions, _) = plan(options, original, orientation, pixels(original) * 128 + encoded * 2, encoded)?;
    let mut frame = vec![0; decoder.output_buffer_size().context("WebP output overflow")?];
    decoder.read_image(&mut frame)?; // Static first frame only, never animated playback.
    let channels = if decoder.has_alpha() { 4 } else { 3 };
    drop(decoder);
    let mut target = Target::new(&output, original, dimensions, orientation, options)?;
    let mut rows = Rows::new(original, &target, options.ground);
    for row in frame.chunks_exact(original.0 as usize * channels) { rows.push(row, channels, &mut target)?; }
    rows.finish()?;
    drop(frame); drop(rows);
    target.finish(&output, options)
}
/// Scan chunk framing without retaining compressed pixels. EXIF can legally follow IDAT.
fn png_metadata(file: &mut File, length: u64) -> Result<u8> {
    file.seek(SeekFrom::Start(8))?;
    let mut orientation = None;
    let mut srgb = false;
    let mut chromaticities = false;
    let mut gamma = None;
    let mut chunks = 0;
    loop {
        chunks += 1;
        ensure!(chunks <= 65536, "PNG exceeds 65536-chunk resource envelope");
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let size = u32::from_be_bytes(header[..4].try_into()?) as u64;
        let position = file.stream_position()?;
        ensure!(position.checked_add(size + 4).is_some_and(|end| end <= length), "PNG chunk extends beyond input");
        match &header[4..] {
            b"iCCP" | b"cICP" | b"mDCV" | b"cLLI" => bail!("PNG ICC/HDR declarations require unsupported color conversion; export sRGB first"),
            b"sRGB" => srgb = true,
            b"cHRM" => chromaticities = true,
            b"gAMA" => {
                ensure!(size == 4, "invalid PNG gamma chunk");
                let mut value = [0; 4];
                file.read_exact(&mut value)?;
                gamma = Some(u32::from_be_bytes(value));
            }
            b"eXIf" => {
                ensure!(size <= 1024 * 1024 && orientation.is_none(), "PNG EXIF exceeds metadata envelope or is duplicated");
                let mut bytes = vec![0; size as usize];
                file.read_exact(&mut bytes)?;
                orientation = Some(exif_orientation(&bytes)?);
            }
            _ => (),
        }
        file.seek(SeekFrom::Start(position + size + 4))?;
        if &header[4..] == b"IEND" { break; }
    }
    ensure!(srgb || (!chromaticities && gamma.is_none_or(|value| (45453..=45457).contains(&value))), "non-sRGB PNG transfer/primaries are unsupported; export sRGB first");
    Ok(orientation.unwrap_or(1))
}
fn jpeg_header(file: &mut File) -> Result<((u32, u32), u8)> {
    let mut magic = [0; 2]; file.read_exact(&mut magic)?; ensure!(magic == [0xff, 0xd8], "invalid JPEG SOI");
    let mut dimensions = None; let mut orientation = 1;
    let mut metadata_bytes = 0_u64;
    loop {
        let mut byte = [0]; file.read_exact(&mut byte)?; ensure!(byte[0] == 0xff, "invalid JPEG marker");
        while byte[0] == 0xff { file.read_exact(&mut byte)?; }
        let marker = byte[0];
        if marker == 0xda || marker == 0xd9 { break; }
        ensure!(!(0xd0..=0xd8).contains(&marker) && marker != 0x01, "unexpected standalone JPEG marker");
        let mut length = [0; 2]; file.read_exact(&mut length)?;
        let length = u16::from_be_bytes(length); ensure!(length >= 2, "invalid JPEG segment length");
        let mut data = vec![0; usize::from(length - 2)]; file.read_exact(&mut data)?;
        metadata_bytes += u64::from(length);
        ensure!(metadata_bytes <= 4 * 1024 * 1024, "JPEG headers exceed 4 MiB metadata envelope");
        if marker == 0xe1 && data.starts_with(b"Exif\0\0") { orientation = exif_orientation(&data)?; }
        if marker == 0xe2 && data.starts_with(b"ICC_PROFILE\0") { bail!("JPEG ICC profile requires unsupported color conversion; export sRGB first"); }
        if (0xc0..=0xcf).contains(&marker) && ![0xc4,0xc8,0xcc].contains(&marker) {
            ensure!([0xc0,0xc1,0xc2].contains(&marker) && data.len() >= 6 && data[0] == 8, "only 8-bit baseline/sequential/progressive JPEG is supported");
            ensure!(data[5] == 1 || data[5] == 3, "CMYK JPEG is unsupported");
            ensure!(data.len() >= 6 + usize::from(data[5]) * 3, "truncated JPEG components");
            if data[5] == 3 {
                let ids = [data[6], data[9], data[12]];
                ensure!(ids != [1, 34, 35] && ids != [114, 103, 98], "wide-gamut JPEG transfer is unsupported; export sRGB first");
            }
            dimensions = Some((u32::from(u16::from_be_bytes([data[3], data[4]])), u32::from(u16::from_be_bytes([data[1], data[2]]))));
        }
    }
    Ok((dimensions.context("JPEG missing frame header")?, orientation))
}
/// Bounded TIFF IFD0 orientation parser: no recursive metadata graph or allocation.
fn exif_orientation(mut bytes: &[u8]) -> Result<u8> {
    if bytes.starts_with(b"Exif\0\0") { bytes = &bytes[6..]; }
    ensure!(bytes.len() >= 8, "truncated EXIF header");
    let little = match &bytes[..2] { b"II" => true, b"MM" => false, _ => bail!("invalid EXIF byte order") };
    let u16_at = |offset: usize| -> Result<u16> { let value: [u8; 2] = bytes.get(offset..offset + 2).context("truncated EXIF short")?.try_into()?; Ok(if little { u16::from_le_bytes(value) } else { u16::from_be_bytes(value) }) };
    let u32_at = |offset: usize| -> Result<u32> { let value: [u8; 4] = bytes.get(offset..offset + 4).context("truncated EXIF long")?.try_into()?; Ok(if little { u32::from_le_bytes(value) } else { u32::from_be_bytes(value) }) };
    ensure!(u16_at(2)? == 42, "invalid EXIF TIFF marker");
    let ifd = usize::try_from(u32_at(4)?)?;
    let count = usize::from(u16_at(ifd)?);
    ensure!(count <= 4096, "EXIF IFD exceeds bounded metadata envelope");
    for entry in 0..count {
        let offset = ifd.checked_add(2 + entry * 12).context("EXIF offset overflow")?;
        if u16_at(offset)? == 0x0112 {
            ensure!(u16_at(offset + 2)? == 3 && u32_at(offset + 4)? == 1, "invalid EXIF orientation entry");
            let value = u16_at(offset + 8)?;
            ensure!((1..=8).contains(&value), "invalid EXIF orientation value");
            return Ok(value as u8);
        }
    }
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn exif_all_orientations_and_bad_offsets() {
        for orientation in 1..=8_u8 {
            let mut bytes = vec![b'I',b'I',42,0,8,0,0,0,1,0,0x12,1,3,0,1,0,0,0,orientation,0,0,0,0,0,0,0];
            assert_eq!(exif_orientation(&bytes).unwrap(), orientation);
            bytes[4] = 255;
            assert!(exif_orientation(&bytes).is_err());
        }
    }
    #[test] fn native_rgb_permutation_preserves_all_bytes_and_oriented_metadata() {
        let source = (3, 2);
        let rgb = [1,19,237, 2,38,218, 3,57,199, 4,76,180, 5,95,161, 6,114,142];
        let orders = [
            [1,2,3,4,5,6], [3,2,1,6,5,4], [6,5,4,3,2,1], [4,5,6,1,2,3],
            [1,4,2,5,3,6], [4,1,5,2,6,3], [6,3,5,2,4,1], [3,6,2,5,1,4],
        ];
        for (index, order) in orders.iter().enumerate() {
            let orientation = index as u8 + 1;
            let dimensions = if orientation >= 5 { (2, 3) } else { (3, 2) };
            for colors in [None, Some(6)] {
                let output = crate::raster::create_memfd().unwrap();
                let options = DecodeOptions { quantize_colors: colors, ..Default::default() };
                let mut target = Target::new(&output, source, dimensions, orientation, &options).unwrap();
                target.orient_rgb(&rgb, source).unwrap();
                let descriptor = target.finish(&output, &options).unwrap();
                assert_eq!((descriptor.width, descriptor.height), dimensions);
                assert_eq!((descriptor.source_width, descriptor.source_height), dimensions);
                assert_eq!(descriptor.bytes, 6 * if colors.is_some() { 1 } else { 4 });
                crate::raster::seal(&output).unwrap();
                let mapped = crate::raster::RasterMapping::new(&output, &descriptor).unwrap();
                if colors.is_some() {
                    assert_eq!(descriptor.format, PixelFormat::Indexed8);
                    for (&index, id) in mapped.as_bytes().iter().zip(order) {
                        let offset = (*id as usize - 1) * 3;
                        assert_eq!(descriptor.palette[index as usize], [rgb[offset], rgb[offset + 1], rgb[offset + 2], 255]);
                    }
                } else {
                    assert_eq!(descriptor.format, PixelFormat::Rgba8);
                    for (pixel, id) in mapped.as_bytes().chunks_exact(4).zip(order) {
                        let offset = (*id as usize - 1) * 3;
                        assert_eq!(pixel, &[rgb[offset], rgb[offset + 1], rgb[offset + 2], 255]);
                    }
                }
            }
        }
    }
    #[test] fn area_filter_flattens_alpha_in_linear_light() {
        for quantize_colors in [None, Some(256)] {
            let output = crate::raster::create_memfd().unwrap();
            let options = DecodeOptions { ground: [0,0,0,255], quantize_colors, ..Default::default() };
            let mut target = Target::new(&output, (2,1), (1,1), 1, &options).unwrap();
            let mut rows = Rows::new((2,1), &target, options.ground);
            rows.push(&[255,255,255,255,255,255,255,0], 4, &mut target).unwrap();
            rows.finish().unwrap();
            let descriptor = target.finish(&output, &options).unwrap();
            crate::raster::seal(&output).unwrap();
            let mapped = crate::raster::RasterMapping::new(&output, &descriptor).unwrap();
            let color = match descriptor.format {
                PixelFormat::Rgba8 => mapped.as_bytes().try_into().unwrap(),
                PixelFormat::Indexed8 => descriptor.palette[usize::from(mapped.as_bytes()[0])],
            };
            assert_eq!(color, [188,188,188,255]);
        }
    }
    #[test] fn real_streaming_png_reduction_and_indices() {
        use std::io::{Seek, SeekFrom};
        for colors in [None, Some(2)] {
            let mut source = tempfile::tempfile().unwrap();
            {
                let mut encoder = png::Encoder::new(&mut source, 4, 2);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                writer.write_image_data(&[255,0,0,255, 255,0,0,255, 0,0,255,255, 0,0,255,255,
                                          255,0,0,255, 255,0,0,255, 0,0,255,255, 0,0,255,255]).unwrap();
            }
            source.seek(SeekFrom::Start(0)).unwrap();
            let output = crate::raster::create_memfd().unwrap();
            let descriptor = decode_to_fd(source, output.try_clone().unwrap(), DecodeOptions {
                texture: TextureLimit::MaxEdge { pixels: 2 }, quantize_colors: colors, ..Default::default()
            }).unwrap();
            assert_eq!((descriptor.width, descriptor.height), (2,1));
            assert_eq!((descriptor.source_width, descriptor.source_height), (4,2));
            crate::raster::seal(&output).unwrap();
            let map = crate::raster::RasterMapping::new(&output, &descriptor).unwrap();
            if colors.is_none() { assert_eq!(map.as_bytes(), &[255,0,0,255, 0,0,255,255]); }
            else { assert_eq!(map.len(), 2); assert_ne!(map[0], map[1]); }
        }
    }
}
