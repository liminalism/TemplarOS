//! Bounded selected-file previews for file-manager clients.
//!
//! Ordinary rasters and the user's BPG/HEIC decoder run directly on a blocking
//! service worker. JPXL and the Templar PDF renderer live in a packaged companion
//! because those workspaces require Rust 1.97 while TemplarOS retains its Rust
//! 1.93 archive baseline. Video is the one intentional external-tool route:
//! ffprobe supplies the duration and ffmpeg seeks to exactly three percent.
//! ImageMagick and Poppler are not used.

use anyhow::{Context, Result, anyhow, bail};
// `bpg-decode` is the still265 crate under its historical name; the HEIC helpers live
// under its compatibility module rather than at the root.
use bpg_decode::internal::bpg_decode::heic;
use bpg_decode::{DecoderConfig, Limits as BpgLimits, PixelLayout};
use image::{ImageReader, RgbaImage, imageops::FilterType};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Cursor,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const MAX_PREVIEW_EDGE: u32 = 768;
pub const MAX_PREVIEW_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_PREVIEW_PNG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DECODE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_DECODE_DIMENSION: u32 = 65_536;
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PROBE_OUTPUT_BYTES: u64 = 4 * 1024;
const HELPER_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const WIRE_MAGIC: &[u8; 4] = b"LPV1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewKind {
    Bpg,
    Heic,
    JpegXl,
    Pdf,
    Raster,
    Video,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreviewMetadata {
    pub renderer: String,
    pub kind: PreviewKind,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedPreview {
    pub metadata: PreviewMetadata,
    pub png: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DecodedRaster {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HelperSpec {
    renderer: &'static str,
    program: OsString,
    args: Vec<OsString>,
    output: PathBuf,
    stdout_to_output: bool,
    max_output_bytes: u64,
    timeout: Duration,
}

/// Render a supported file to a bounded PNG. `Ok(None)` means the type has no preview route;
/// malformed files or missing/failing helpers are errors so the UI can distinguish those cases.
pub fn render(path: &Path, requested_edge: u32) -> Result<Option<RenderedPreview>> {
    if !path.is_absolute() {
        bail!("preview path must be absolute: {}", path.display());
    }
    let Some(kind) = classify(path) else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspecting preview source {}", path.display()))?;
    if !metadata.is_file() {
        bail!("preview source is not a regular file: {}", path.display());
    }
    // Video is seeked rather than read into memory, so its container size is not
    // an allocation bound. Still-image and document decoders read the source.
    if kind != PreviewKind::Video && metadata.len() > MAX_PREVIEW_SOURCE_BYTES {
        bail!(
            "preview source is larger than {} bytes: {}",
            MAX_PREVIEW_SOURCE_BYTES,
            path.display()
        );
    }

    let edge = requested_edge.clamp(32, MAX_PREVIEW_EDGE);
    let preview = match kind {
        PreviewKind::Bpg | PreviewKind::Heic | PreviewKind::Raster => {
            let (renderer, raster) = decode_in_process(kind, path)?;
            finish_raster(kind, renderer, raster, edge)?
        }
        PreviewKind::JpegXl | PreviewKind::Pdf => render_with_companion(kind, path, edge)?,
        PreviewKind::Video => render_video(path, edge)?,
    };
    Ok(Some(preview))
}

fn classify(path: &Path) -> Option<PreviewKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "bpg" => Some(PreviewKind::Bpg),
        "heic" | "heif" | "hif" => Some(PreviewKind::Heic),
        "jxl" => Some(PreviewKind::JpegXl),
        "pdf" => Some(PreviewKind::Pdf),
        "bmp" | "dds" | "exr" | "gif" | "hdr" | "ico" | "jpeg" | "jpg" | "png" | "pnm" | "ppm"
        | "qoi" | "tga" | "tif" | "tiff" | "webp" => Some(PreviewKind::Raster),
        "3g2" | "3gp" | "asf" | "av1" | "avi" | "divx" | "f4v" | "flv" | "ivf" | "m2ts" | "m2v"
        | "m4v" | "mkv" | "mov" | "mp4" | "mpeg" | "mpg" | "mts" | "mxf" | "ogv" | "rm"
        | "rmvb" | "ts" | "vob" | "webm" | "wmv" => Some(PreviewKind::Video),
        _ => None,
    }
}

fn decode_in_process(kind: PreviewKind, path: &Path) -> Result<(&'static str, DecodedRaster)> {
    let source = std::fs::read(path)
        .with_context(|| format!("reading preview source {}", path.display()))?;
    if source.len() as u64 > MAX_PREVIEW_SOURCE_BYTES {
        bail!("preview source grew past its {MAX_PREVIEW_SOURCE_BYTES} byte limit");
    }
    match kind {
        PreviewKind::Bpg => decode_bpg(&source).map(|raster| ("bpg-rs", raster)),
        PreviewKind::Heic => decode_heic(&source).map(|raster| ("bpg-rs HEIC", raster)),
        PreviewKind::Raster => decode_image_rs(&source).map(|raster| ("image-rs", raster)),
        PreviewKind::JpegXl | PreviewKind::Pdf | PreviewKind::Video => {
            bail!("internal preview route mismatch for {kind:?}")
        }
    }
}

fn decode_bpg(source: &[u8]) -> Result<DecodedRaster> {
    let info = bpg_decode::ImageInfo::from_bytes(source).context("reading BPG dimensions")?;
    checked_dimensions(info.width, info.height)?;
    let limits = BpgLimits {
        max_width: Some(u64::from(MAX_DECODE_DIMENSION)),
        max_height: Some(u64::from(MAX_DECODE_DIMENSION)),
        max_pixels: Some(MAX_DECODE_PIXELS),
        max_memory_bytes: Some(MAX_DECODE_BYTES),
    };
    let decoded = DecoderConfig::new()
        .decode_request(source)
        .with_output_layout(PixelLayout::Rgba8)
        .with_limits(&limits)
        .decode()
        .context("decoding BPG with bpg-rs")?;
    raster_from_bpg(decoded)
}

fn decode_heic(source: &[u8]) -> Result<DecodedRaster> {
    let info =
        heic::get_heic_image_info(source).context("reading HEIC/HEIF dimensions with bpg-rs")?;
    checked_dimensions(info.width, info.height)?;
    // Prefer a container-provided thumbnail: the file manager never needs the
    // full camera raster when a purpose-built preview is available.
    let decoded = match heic::decode_heic_thumbnail(source, PixelLayout::Rgba8) {
        Ok(Some(thumbnail)) => thumbnail,
        Ok(None) | Err(_) => heic::decode_heic(source, PixelLayout::Rgba8)
            .context("decoding HEIC/HEIF with bpg-rs")?,
    };
    raster_from_bpg(decoded)
}

fn raster_from_bpg(decoded: bpg_decode::DecodeOutput) -> Result<DecodedRaster> {
    checked_dimensions(decoded.width, decoded.height)?;
    checked_rgba_len(decoded.width, decoded.height, decoded.data.len())?;
    Ok(DecodedRaster {
        width: decoded.width,
        height: decoded.height,
        rgba: decoded.data,
    })
}

fn decode_image_rs(source: &[u8]) -> Result<DecodedRaster> {
    let dimensions = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .context("identifying raster format")?
        .into_dimensions()
        .context("reading raster dimensions")?;
    checked_dimensions(dimensions.0, dimensions.1)?;

    let mut reader = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .context("identifying raster format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().context("decoding raster with image-rs")?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    checked_dimensions(width, height)?;
    Ok(DecodedRaster {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

fn finish_raster(
    kind: PreviewKind,
    renderer: &str,
    raster: DecodedRaster,
    edge: u32,
) -> Result<RenderedPreview> {
    let raster = downscale(raster, edge)?;
    let png = encode_png(&raster)?;
    if png.len() as u64 > MAX_PREVIEW_PNG_BYTES {
        bail!("encoded preview exceeds {MAX_PREVIEW_PNG_BYTES} bytes");
    }
    Ok(RenderedPreview {
        metadata: PreviewMetadata {
            renderer: renderer.to_owned(),
            kind,
            width: raster.width,
            height: raster.height,
        },
        png,
    })
}

fn downscale(raster: DecodedRaster, edge: u32) -> Result<DecodedRaster> {
    checked_rgba_len(raster.width, raster.height, raster.rgba.len())?;
    if raster.width.max(raster.height) <= edge {
        return Ok(raster);
    }
    let scale = f64::from(edge) / f64::from(raster.width.max(raster.height));
    let width = ((f64::from(raster.width) * scale).round() as u32).clamp(1, edge);
    let height = ((f64::from(raster.height) * scale).round() as u32).clamp(1, edge);
    let source = RgbaImage::from_raw(raster.width, raster.height, raster.rgba)
        .ok_or_else(|| anyhow!("decoder returned a malformed RGBA plane"))?;
    let scaled = image::imageops::resize(&source, width, height, FilterType::Triangle);
    Ok(DecodedRaster {
        width,
        height,
        rgba: scaled.into_raw(),
    })
}

fn encode_png(raster: &DecodedRaster) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, raster.width, raster.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .context("writing preview PNG header")?;
        writer
            .write_image_data(&raster.rgba)
            .context("writing preview PNG pixels")?;
    }
    Ok(output)
}

fn render_with_companion(kind: PreviewKind, path: &Path, edge: u32) -> Result<RenderedPreview> {
    let temp = tempfile::Builder::new()
        .prefix("templaros-preview-")
        .tempdir()
        .context("creating private preview directory")?;
    let spec = companion_spec(kind, path, edge, temp.path(), preview_companion());
    run_helper(&spec)?;
    let raster = read_wire(&spec.output, edge)?;
    finish_raster(kind, spec.renderer, raster, edge)
}

fn preview_companion() -> OsString {
    if let Some(program) = std::env::var_os("TEMPLAROS_PREVIEW_CODECS") {
        return program;
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let adjacent = directory.join("templar-preview-codecs");
        if adjacent.is_file() {
            return adjacent.into_os_string();
        }
    }
    let development = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/preview-codecs/templar-preview-codecs");
    if development.is_file() {
        return development.into_os_string();
    }
    OsString::from("templar-preview-codecs")
}

fn companion_spec(
    kind: PreviewKind,
    path: &Path,
    edge: u32,
    temp: &Path,
    program: OsString,
) -> HelperSpec {
    let (command, renderer) = match kind {
        PreviewKind::JpegXl => ("jxl", "JPXL"),
        PreviewKind::Pdf => ("pdf", "Templar PDF"),
        _ => unreachable!("only companion preview kinds have a companion spec"),
    };
    let output = temp.join("preview.rgba");
    HelperSpec {
        renderer,
        program,
        args: vec![
            OsString::from(command),
            path.as_os_str().to_owned(),
            output.as_os_str().to_owned(),
            OsString::from(edge.to_string()),
        ],
        output,
        stdout_to_output: false,
        max_output_bytes: 12 + u64::from(edge) * u64::from(edge) * 4,
        timeout: HELPER_TIMEOUT,
    }
}

fn read_wire(path: &Path, edge: u32) -> Result<DecodedRaster> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading native preview output {}", path.display()))?;
    let max_bytes = 12 + u64::from(edge) * u64::from(edge) * 4;
    if metadata.len() < 12 || metadata.len() > max_bytes {
        bail!("native preview output has invalid size {}", metadata.len());
    }
    let wire = std::fs::read(path)?;
    if wire.get(..4) != Some(WIRE_MAGIC) {
        bail!("native preview output has an invalid magic value");
    }
    let width = u32::from_le_bytes(
        wire.get(4..8)
            .ok_or_else(|| anyhow!("truncated native preview width"))?
            .try_into()
            .map_err(|_| anyhow!("invalid native preview width"))?,
    );
    let height = u32::from_le_bytes(
        wire.get(8..12)
            .ok_or_else(|| anyhow!("truncated native preview height"))?
            .try_into()
            .map_err(|_| anyhow!("invalid native preview height"))?,
    );
    if width == 0 || height == 0 || width > edge || height > edge {
        bail!("native preview dimensions {width}x{height} exceed {edge}x{edge}");
    }
    checked_rgba_len(width, height, wire.len().saturating_sub(12))?;
    Ok(DecodedRaster {
        width,
        height,
        rgba: wire[12..].to_vec(),
    })
}

fn render_video(path: &Path, edge: u32) -> Result<RenderedPreview> {
    let temp = tempfile::Builder::new()
        .prefix("templaros-video-preview-")
        .tempdir()
        .context("creating private video-preview directory")?;
    let duration = probe_video_duration(path, temp.path())?;
    let seek = duration * 0.03;
    let spec = video_spec(path, edge, seek, temp.path());
    run_helper(&spec)?;
    let (png, width, height) = read_bounded_png(&spec.output, edge)?;
    Ok(RenderedPreview {
        metadata: PreviewMetadata {
            renderer: "ffmpeg video at 3%".to_owned(),
            kind: PreviewKind::Video,
            width,
            height,
        },
        png,
    })
}

fn probe_video_duration(path: &Path, temp: &Path) -> Result<f64> {
    let output = temp.join("duration.txt");
    let spec = HelperSpec {
        renderer: "ffprobe-duration",
        program: std::env::var_os("TEMPLAROS_FFPROBE").unwrap_or_else(|| OsString::from("ffprobe")),
        args: vec![
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-select_streams"),
            OsString::from("v:0"),
            OsString::from("-show_entries"),
            OsString::from("format=duration:stream=duration"),
            OsString::from("-of"),
            OsString::from("default=noprint_wrappers=1:nokey=1"),
            path.as_os_str().to_owned(),
        ],
        output,
        stdout_to_output: true,
        max_output_bytes: MAX_PROBE_OUTPUT_BYTES,
        timeout: PROBE_TIMEOUT,
    };
    run_helper(&spec)?;
    let text = std::fs::read_to_string(&spec.output).context("reading ffprobe duration")?;
    parse_video_duration(&text)
}

fn parse_video_duration(output: &str) -> Result<f64> {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .max_by(f64::total_cmp)
        .ok_or_else(|| anyhow!("ffprobe did not report a positive video duration"))
}

fn video_spec(path: &Path, edge: u32, seek: f64, temp: &Path) -> HelperSpec {
    let output = temp.join("preview.png");
    HelperSpec {
        renderer: "ffmpeg-video-3-percent",
        program: std::env::var_os("TEMPLAROS_FFMPEG").unwrap_or_else(|| OsString::from("ffmpeg")),
        args: vec![
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-nostdin"),
            OsString::from("-y"),
            OsString::from("-filter_threads"),
            OsString::from("1"),
            OsString::from("-threads"),
            OsString::from("2"),
            OsString::from("-ss"),
            OsString::from(format!("{seek:.6}")),
            OsString::from("-i"),
            path.as_os_str().to_owned(),
            OsString::from("-map"),
            OsString::from("0:v:0"),
            OsString::from("-frames:v"),
            OsString::from("1"),
            OsString::from("-an"),
            OsString::from("-sn"),
            OsString::from("-threads"),
            OsString::from("1"),
            OsString::from("-vf"),
            OsString::from(format!(
                "scale={edge}:{edge}:force_original_aspect_ratio=decrease:flags=lanczos"
            )),
            output.as_os_str().to_owned(),
        ],
        output,
        stdout_to_output: false,
        max_output_bytes: MAX_PREVIEW_PNG_BYTES,
        timeout: HELPER_TIMEOUT,
    }
}

fn run_helper(spec: &HelperSpec) -> Result<()> {
    let _ = std::fs::remove_file(&spec.output);
    let stderr_path = spec.output.with_extension("stderr");
    let stderr = File::create(&stderr_path).context("creating preview helper error log")?;
    let stdout = if spec.stdout_to_output {
        Stdio::from(File::create(&spec.output).context("creating preview helper output")?)
    } else {
        Stdio::null()
    };
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::from(stderr));
    constrain_helper(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", OsStr::new(&spec.program).to_string_lossy()))?;
    let deadline = Instant::now() + spec.timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for preview helper")? {
            break status;
        }
        if std::fs::metadata(&spec.output)
            .is_ok_and(|metadata| metadata.len() > spec.max_output_bytes)
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} output exceeded {} bytes",
                spec.renderer,
                spec.max_output_bytes
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {} seconds", spec.timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !status.success() {
        let detail = std::fs::read_to_string(&stderr_path)
            .unwrap_or_default()
            .chars()
            .take(1_024)
            .collect::<String>();
        bail!(
            "{} exited with {status}{}",
            spec.renderer,
            if detail.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", detail.trim())
            }
        );
    }
    let length = std::fs::metadata(&spec.output)
        .with_context(|| format!("{} produced no output", spec.renderer))?
        .len();
    if length == 0 || length > spec.max_output_bytes {
        bail!(
            "{} output size {length} is outside the permitted range",
            spec.renderer
        );
    }
    Ok(())
}

#[allow(unsafe_code)]
fn constrain_helper(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `pre_exec` runs after fork. The closure makes only async-signal-safe
    // `setrlimit(2)` calls, captures nothing, and returns the OS error directly.
    unsafe {
        command.pre_exec(|| {
            let address_space = libc::rlimit {
                rlim_cur: 2 * 1024 * 1024 * 1024,
                rlim_max: 2 * 1024 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &raw const address_space) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let file_size = libc::rlimit {
                rlim_cur: 32 * 1024 * 1024,
                rlim_max: 32 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &raw const file_size) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let cpu = libc::rlimit {
                rlim_cur: 25,
                rlim_max: 25,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &raw const cpu) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn read_bounded_png(path: &Path, edge: u32) -> Result<(Vec<u8>, u32, u32)> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading helper output {}", path.display()))?;
    if metadata.len() == 0 || metadata.len() > MAX_PREVIEW_PNG_BYTES {
        bail!(
            "PNG output size {} is outside the permitted range",
            metadata.len()
        );
    }
    let png = std::fs::read(path)?;
    let (width, height) = png_dimensions(&png)?;
    if width == 0 || height == 0 || width > edge || height > edge {
        bail!("PNG dimensions {width}x{height} exceed {edge}x{edge}");
    }
    Ok((png, width, height))
}

fn png_dimensions(png: &[u8]) -> Result<(u32, u32)> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png.get(..8) != Some(SIGNATURE) || png.get(12..16) != Some(b"IHDR") {
        bail!("helper output is not a PNG with an IHDR header");
    }
    let width = u32::from_be_bytes(
        png.get(16..20)
            .ok_or_else(|| anyhow!("truncated PNG width"))?
            .try_into()
            .map_err(|_| anyhow!("bad PNG width"))?,
    );
    let height = u32::from_be_bytes(
        png.get(20..24)
            .ok_or_else(|| anyhow!("truncated PNG height"))?
            .try_into()
            .map_err(|_| anyhow!("bad PNG height"))?,
    );
    Ok((width, height))
}

fn checked_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("decoded image has a zero dimension");
    }
    if width > MAX_DECODE_DIMENSION || height > MAX_DECODE_DIMENSION {
        bail!("decoded dimensions {width}x{height} exceed {MAX_DECODE_DIMENSION} on one axis");
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| anyhow!("decoded image dimensions overflow"))?;
    if pixels > MAX_DECODE_PIXELS {
        bail!("decoded image has {pixels} pixels; limit is {MAX_DECODE_PIXELS}");
    }
    Ok(())
}

fn checked_rgba_len(width: u32, height: u32, actual: usize) -> Result<()> {
    checked_dimensions(width, height)?;
    let expected = usize::try_from(u64::from(width) * u64::from(height))
        .ok()
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("decoded RGBA size overflow"))?;
    if actual != expected {
        bail!("decoded RGBA plane has {actual} bytes, expected {expected}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_formats_route_to_the_native_decoders() {
        let cases = [
            ("photo.BPG", PreviewKind::Bpg),
            ("photo.HEIC", PreviewKind::Heic),
            ("photo.jxl", PreviewKind::JpegXl),
            ("report.PDF", PreviewKind::Pdf),
            ("ordinary.webp", PreviewKind::Raster),
            ("movie.MKV", PreviewKind::Video),
        ];
        for (name, kind) in cases {
            assert_eq!(classify(&Path::new("/").join(name)), Some(kind));
        }
        assert_eq!(classify(Path::new("/notes.txt")), None);

        let temp = tempfile::tempdir().unwrap();
        let jxl = companion_spec(
            PreviewKind::JpegXl,
            Path::new("/photo.jxl"),
            512,
            temp.path(),
            OsString::from("/usr/libexec/templar-preview-codecs"),
        );
        let pdf = companion_spec(
            PreviewKind::Pdf,
            Path::new("/report.pdf"),
            512,
            temp.path(),
            OsString::from("/usr/libexec/templar-preview-codecs"),
        );
        assert_eq!(jxl.renderer, "JPXL");
        assert_eq!(pdf.renderer, "Templar PDF");
        for spec in [jxl, pdf] {
            let program = spec.program.to_string_lossy();
            assert!(!program.contains("magick"));
            assert!(!program.contains("ffmpeg"));
            assert!(!program.contains("poppler"));
            assert!(!program.contains("pdftoppm"));
        }
    }

    #[test]
    fn preview_wire_is_dimension_and_length_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("preview.rgba");
        let mut wire = WIRE_MAGIC.to_vec();
        wire.extend_from_slice(&2_u32.to_le_bytes());
        wire.extend_from_slice(&1_u32.to_le_bytes());
        wire.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        std::fs::write(&path, &wire).unwrap();
        let raster = read_wire(&path, 64).unwrap();
        assert_eq!((raster.width, raster.height), (2, 1));
        wire.pop();
        std::fs::write(&path, wire).unwrap();
        assert!(read_wire(&path, 64).is_err());
    }

    #[test]
    fn preview_png_header_is_dimension_bounded() {
        let mut header = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        header.extend_from_slice(&320_u32.to_be_bytes());
        header.extend_from_slice(&200_u32.to_be_bytes());
        assert_eq!(png_dimensions(&header).unwrap(), (320, 200));
        header[0] = 0;
        assert!(png_dimensions(&header).is_err());
    }

    #[test]
    fn video_preview_uses_three_percent_of_duration() {
        assert!((parse_video_duration("N/A\n1000.0\n").unwrap() - 1000.0).abs() < f64::EPSILON);
        let temp = tempfile::tempdir().unwrap();
        let spec = video_spec(Path::new("/movie.mkv"), 512, 1000.0 * 0.03, temp.path());
        let seek = spec
            .args
            .windows(2)
            .find(|pair| pair[0] == "-ss")
            .map(|pair| pair[1].to_string_lossy().into_owned());
        assert_eq!(seek.as_deref(), Some("30.000000"));
        assert_eq!(spec.renderer, "ffmpeg-video-3-percent");
    }

    #[test]
    fn preview_rejects_relative_paths_and_unsupported_types_without_spawning() {
        assert!(render(Path::new("photo.heic"), 256).is_err());
        let temp = tempfile::tempdir().unwrap();
        let text = temp.path().join("notes.txt");
        std::fs::write(&text, b"hello").unwrap();
        assert_eq!(render(&text, 256).unwrap(), None);
    }

    #[test]
    #[ignore = "set TEMPLAROS_PREVIEW_FIXTURE to exercise an installed decoder"]
    fn preview_live_fixture() {
        let path = std::env::var_os("TEMPLAROS_PREVIEW_FIXTURE")
            .map(PathBuf::from)
            .expect("TEMPLAROS_PREVIEW_FIXTURE must name an absolute supported file");
        let preview = render(&path, 256)
            .expect("fixture preview should render")
            .expect("fixture extension should have a preview route");
        let dimensions = png_dimensions(&preview.png).expect("service should return a PNG");
        assert_eq!(
            dimensions,
            (preview.metadata.width, preview.metadata.height)
        );
        assert!(preview.metadata.width <= 256);
        assert!(preview.metadata.height <= 256);
        eprintln!(
            "{}: {:?} via {} at {}x{} ({} PNG bytes)",
            path.display(),
            preview.metadata.kind,
            preview.metadata.renderer,
            preview.metadata.width,
            preview.metadata.height,
            preview.png.len()
        );
    }
}
