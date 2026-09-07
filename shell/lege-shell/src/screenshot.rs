//! `lege-shell --screenshot PATH.png` — the whole output, written to a PNG and nothing else.
//!
//! There is no other way to see this desktop. `grim`, `xdotool` and friends need protocols
//! `lege-comp` deliberately does not offer, and the compositor's own capture globals are visible
//! only on the trusted-shell socket (`compositor/lege-comp/src/screencopy.rs`) — so the one
//! process that can take a screenshot is this one, and this is the mode that does it.
//!
//! A **second, short-lived connection**, not the running shell's: it opens the trusted socket,
//! captures one frame, writes the file and exits. That is why it does not reuse
//! [`crate::previews`] — every `Dispatch` there is on the running shell's `BridgeState` and its
//! queue, which this process does not have and must not disturb. The protocol shape is the same
//! (`ext-image-capture-source-v1` + `ext-image-copy-capture-v1`); only the source is an output
//! rather than a toplevel.

use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_output::{self, WlOutput},
    wl_registry,
    wl_shm::{self, WlShm},
    wl_shm_pool::{self, WlShmPool},
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
    ext_output_image_capture_source_manager_v1::{self, ExtOutputImageCaptureSourceManagerV1},
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

/// How long to wait for the compositor to answer one capture before giving up.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Capture the first output and write it to `path`.
///
/// The socket is the compositor's trusted-shell one. `WAYLAND_DISPLAY` normally already names it
/// (that is how `lege-shell --surface` is started), so this tries it as given and falls back to
/// deriving `legeos-shell-<display>` from an ordinary one — which is what makes the command work
/// unchanged from a terminal inside the session and from the shell's own environment.
pub fn capture(path: &Path) -> Result<()> {
    let display = std::env::var("WAYLAND_DISPLAY")
        .context("WAYLAND_DISPLAY is not set; there is no compositor to screenshot")?;
    let (connection, socket) = connect(&display)?;
    tracing::info!(%socket, "connected for a screenshot");

    // ponytail: a watchdog thread rather than a poll-with-timeout event loop. The compositor
    // answers a capture from its next render pass and asks its backend for one
    // (`Backend::request_redraw`), but a backend that cannot draw at all — a nested compositor
    // whose host has stopped sending it frame callbacks — has no next render pass, and a
    // screenshot command must fail out loud rather than hang a terminal. Upgrade path: drive the
    // queue through `prepare_read` + `poll` if this ever needs to be a library call.
    std::thread::spawn(|| {
        std::thread::sleep(TIMEOUT);
        eprintln!(
            "the compositor did not answer a screen capture within {}s; it may not be drawing \
             frames at all",
            TIMEOUT.as_secs()
        );
        std::process::exit(1);
    });

    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;

    let (Some(shm), Some(output), Some(sources), Some(captures)) = (
        state.shm.clone(),
        state.output.clone(),
        state.sources.clone(),
        state.captures.clone(),
    ) else {
        bail!(
            "the compositor offers no output capture on {socket}: point WAYLAND_DISPLAY at the \
             trusted-shell socket (legeos-shell-<display>)"
        );
    };

    let source = sources.create_source(&output, &qh, ());
    let session = captures.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        &qh,
        (),
    );

    // The compositor answers `buffer_size`/`shm_format`/`done` before a frame may be asked for.
    while state.size.is_none() || state.formats.is_empty() {
        queue.blocking_dispatch(&mut state)?;
        if state.stopped {
            bail!("the compositor stopped the capture session before it described a buffer");
        }
    }
    let (width, height) = state.size.expect("checked by the loop");
    let format = if state.formats.contains(&wl_shm::Format::Argb8888) {
        wl_shm::Format::Argb8888
    } else if state.formats.contains(&wl_shm::Format::Xrgb8888) {
        wl_shm::Format::Xrgb8888
    } else {
        bail!(
            "the compositor offers no shm format this can read: {:?}",
            state.formats
        );
    };

    let stride = width * 4;
    let file = tempfile::tempfile().context("creating the screenshot pool file")?;
    file.set_len(u64::from(stride) * u64::from(height))
        .context("sizing the screenshot pool")?;
    let pool = shm.create_pool(
        std::os::fd::AsFd::as_fd(&file),
        i32::try_from(u64::from(stride) * u64::from(height)).context("the output is too large")?,
        &qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        i32::try_from(width)?,
        i32::try_from(height)?,
        i32::try_from(stride)?,
        format,
        &qh,
        (),
    );

    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, i32::try_from(width)?, i32::try_from(height)?);
    frame.capture();
    while state.frame.is_none() {
        queue.blocking_dispatch(&mut state)?;
    }
    match state.frame.take().expect("checked by the loop") {
        Ok(()) => {}
        Err(reason) => bail!("the compositor refused the capture: {reason}"),
    }

    write_png(path, &file, width, height, stride)?;
    frame.destroy();
    buffer.destroy();
    pool.destroy();
    session.destroy();
    source.destroy();
    Ok(())
}

/// The trusted-shell socket, as given or as derived. Returns the connection and the name it used,
/// so a failure says which socket was actually tried.
///
/// Connecting by path rather than through `WAYLAND_DISPLAY` because both candidates have to be
/// tried and `std::env::set_var` is `unsafe` in Rust 2024 — the same reason
/// `compositor/lege-comp/src/backend/check.rs`'s client test does it this way.
fn connect(display: &str) -> Result<(Connection, String)> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .context("XDG_RUNTIME_DIR is not set, so no Wayland socket can be found")?;
    let derived = format!("legeos-shell-{display}");
    let mut last = None;
    for name in [derived, display.to_owned()] {
        match std::os::unix::net::UnixStream::connect(runtime.join(&name))
            .map_err(anyhow::Error::from)
            .and_then(|stream| Connection::from_socket(stream).map_err(anyhow::Error::from))
        {
            Ok(connection) => return Ok((connection, name)),
            Err(error) => last = Some(error),
        }
    }
    Err(anyhow!(
        "no Wayland socket named legeos-shell-{display} or {display} in {}: {}",
        runtime.display(),
        last.map_or_else(
            || "no attempt was made".to_owned(),
            |error| error.to_string()
        )
    ))
}

/// The pool's `Argb8888`/`Xrgb8888` rows (little-endian: B, G, R, A per pixel) as an opaque RGBA
/// PNG. Alpha is forced opaque: a screenshot of a screen has nothing behind it to blend with, and
/// a viewer that honoured a stray alpha would show a checkerboard where the desktop was.
fn write_png(
    path: &Path,
    file: &std::fs::File,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<()> {
    let mut row = vec![0u8; stride as usize];
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        file.read_exact_at(&mut row, u64::from(y) * u64::from(stride))
            .context("reading a screenshot row")?;
        for x in 0..width as usize {
            let pixel = &row[x * 4..x * 4 + 4];
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xff]);
        }
    }
    let out =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(out);
    let mut encoder = png::Encoder::new(&mut writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&rgba)
        .context("encoding the screenshot as PNG")?;
    writer.flush().context("flushing the screenshot")?;
    Ok(())
}

/// Everything one capture needs, and nothing that outlives it.
#[derive(Debug, Default)]
struct State {
    shm: Option<WlShm>,
    output: Option<WlOutput>,
    sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    captures: Option<ExtImageCopyCaptureManagerV1>,
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    /// `Some` once the compositor answered the one frame: `Ok` for `ready`, `Err(reason)` for
    /// `failed`.
    frame: Option<Result<(), String>>,
    stopped: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
            // The first output the compositor advertises, which on a laptop is the laptop.
            // ponytail: no `--output` selection; add one when a second screen is worth naming.
            "wl_output" if state.output.is_none() => {
                state.output = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "ext_output_image_capture_source_manager_v1" => {
                state.sources = Some(registry.bind(name, version.min(1), qh, ()));
            }
            "ext_image_copy_capture_manager_v1" => {
                state.captures = Some(registry.bind(name, version.min(1), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _session: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        (): &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                if let Ok(format) = format.into_result() {
                    state.formats.push(format);
                }
            }
            ext_image_copy_capture_session_v1::Event::Stopped => state.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        (): &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.frame = Some(Ok(())),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.frame = Some(Err(format!("{reason:?}")));
            }
            _ => {}
        }
    }
}

// The rest send nothing this cares about, but every proxy needs a `Dispatch` to be created at all.
macro_rules! ignores_events {
    ($($proxy:ty => $event:ty),* $(,)?) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _state: &mut Self,
                _proxy: &$proxy,
                _event: $event,
                (): &(),
                _connection: &Connection,
                _qh: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignores_events! {
    WlShm => wl_shm::Event,
    WlShmPool => wl_shm_pool::Event,
    WlBuffer => wl_buffer::Event,
    WlOutput => wl_output::Event,
    ExtImageCaptureSourceV1 => ext_image_capture_source_v1::Event,
    ExtOutputImageCaptureSourceManagerV1 => ext_output_image_capture_source_manager_v1::Event,
    ExtImageCopyCaptureManagerV1 => ext_image_copy_capture_manager_v1::Event,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool bytes are `Argb8888` little-endian — B, G, R, A — and the PNG must come out in
    /// R, G, B order with opaque alpha. A channel swap here is invisible until someone looks at a
    /// screenshot and finds the desktop is orange.
    #[test]
    fn the_pool_is_written_out_with_its_channels_in_png_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("shot.png");
        let pool = tempfile::tempfile().expect("pool");
        // One pixel: B=0x10, G=0x20, R=0x30, A=0x00 (deliberately transparent in the source).
        pool.write_all_at(&[0x10, 0x20, 0x30, 0x00], 0)
            .expect("write");

        write_png(&path, &pool, 1, 1, 4).expect("written");

        let decoder = png::Decoder::new(std::fs::File::open(&path).expect("open"));
        let mut reader = decoder.read_info().expect("a valid PNG");
        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buffer).expect("a frame");
        assert_eq!((info.width, info.height), (1, 1));
        assert_eq!(&buffer[..4], &[0x30, 0x20, 0x10, 0xff]);
    }
}
