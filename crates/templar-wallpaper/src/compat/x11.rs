//! One-shot root-depth setter. Only the completed pixmap and its authenticated
//! ownership marker survive disconnect. Never use KillClient or trust root IDs.
use super::pixels::Sampler;
use super::{ApplyStatus, Backend, Capabilities, Request, path_bytes};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant};
use x11rb::COPY_FROM_PARENT;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::{Event, randr, shm, xproto::*};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[derive(Debug, Serialize, Deserialize)]
struct Owned {
    marker: u32,
    pixmap: u32,
    token: [u8; 32],
    bytes: u64,
}

#[derive(Debug)]
struct Ownership {
    file: File,
    old: Option<Owned>,
}
impl Ownership {
    fn open(display: &str, screen: usize, root: u32) -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .context("XDG_RUNTIME_DIR is required for safe one-shot X11 ownership")?;
        let directory = std::path::PathBuf::from(runtime);
        let meta = std::fs::symlink_metadata(&directory)?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            bail!("XDG_RUNTIME_DIR must be an owned private directory");
        }
        // Stable filename hash is not an ownership credential: the random marker is.
        let hash = display
            .as_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
            });
        let path = directory.join(format!(
            "templar-wallpaper-x11-{hash:016x}-{screen}-{root:08x}.json"
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
            bail!("unsafe X11 ownership file");
        }
        // SAFETY: flock operates on our owned descriptor. The lock serializes repeats.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut body = Vec::new();
        (&mut file).take(4097).read_to_end(&mut body)?;
        if body.len() > 4096 {
            bail!("oversized X11 ownership record");
        }
        let old = if body.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&body).context("invalid private X11 ownership record")?)
        };
        Ok(Self { file, old })
    }
    fn save(&mut self, owned: &Owned) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        serde_json::to_writer(&mut self.file, owned)?;
        self.file.sync_all()?;
        Ok(())
    }
}

fn atom(conn: &RustConnection, name: &[u8]) -> Result<u32> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}
fn reject_xwayland(conn: &RustConnection, root: u32) -> Result<()> {
    if conn.query_extension(b"XWAYLAND")?.reply()?.present {
        bail!("XWayland root is not a Wayland wallpaper backend; use layer-shell or portal");
    }
    let scale = conn
        .intern_atom(true, b"_XWAYLAND_GLOBAL_OUTPUT_SCALE")?
        .reply()?
        .atom;
    if scale != 0
        && conn
            .get_property(false, root, scale, AtomEnum::ANY, 0, 1)?
            .reply()?
            .type_
            != 0
    {
        bail!("XWayland root is not a Wayland wallpaper backend");
    }
    if conn.query_extension(b"RANDR")?.reply()?.present {
        let resources = randr::get_screen_resources_current(conn, root)?.reply()?;
        for output in resources.outputs {
            let info = randr::get_output_info(conn, output, resources.config_timestamp)?.reply()?;
            if info.name.starts_with(b"XWAYLAND") {
                bail!("XWayland output is not a Wayland wallpaper backend");
            }
        }
    }
    Ok(())
}

pub fn inspect() -> Result<Capabilities> {
    let (conn, screen) = x11rb::connect(None)?;
    reject_xwayland(&conn, conn.setup().roots[screen].root)?;
    let root = &conn.setup().roots[screen];
    Packing::new(&conn, root.root_depth, root.root_visual, screen)?;
    Ok(capabilities())
}
fn capabilities() -> Capabilities {
    Capabilities { backend: Backend::X11, available: true, exact_sizing: true, reduced_buffers: false,
        resident_connection: false, indexed_storage: false, presentation_verified: false,
        detail: "one-shot root-depth pixmap; source reduction does not reduce the server root pixmap; use a session-scoped external scheduler, never a timer surviving logout".into() }
}

#[derive(Debug)]
struct Packing {
    bytes: usize,
    pad: usize,
    masks: [u32; 3],
    opaque: u32,
    little: bool,
}
impl Packing {
    fn new(conn: &RustConnection, depth: u8, visual_id: u32, screen: usize) -> Result<Self> {
        let setup = conn.setup();
        let format = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .context("root pixmap format missing")?;
        if ![16, 24, 32].contains(&format.bits_per_pixel)
            || ![8, 16, 32].contains(&format.scanline_pad)
        {
            bail!("unsupported root ZPixmap format");
        }
        let visual = setup.roots[screen]
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == visual_id)
            .context("root visual missing")?;
        if visual.class != VisualClass::TRUE_COLOR {
            bail!("root visual must be TrueColor (colormap ownership is unsupported)");
        }
        let masks = [visual.red_mask, visual.green_mask, visual.blue_mask];
        for mask in masks {
            if mask == 0 {
                bail!("empty TrueColor mask");
            }
            let run = mask >> mask.trailing_zeros();
            if run & run.wrapping_add(1) != 0 {
                bail!("noncontiguous TrueColor mask");
            }
        }
        if masks[0] & masks[1] != 0 || masks[0] & masks[2] != 0 || masks[1] & masks[2] != 0 {
            bail!("overlapping TrueColor masks");
        }
        let all = masks[0] | masks[1] | masks[2];
        if format.bits_per_pixel < 32 && all >> format.bits_per_pixel != 0 {
            bail!("TrueColor mask exceeds pixel storage");
        }
        let significant = if depth == 32 {
            u32::MAX
        } else {
            (1_u32 << depth) - 1
        };
        Ok(Self {
            bytes: usize::from(format.bits_per_pixel / 8),
            pad: usize::from(format.scanline_pad / 8),
            masks,
            opaque: significant & !all,
            little: setup.image_byte_order == ImageOrder::LSB_FIRST,
        })
    }
    fn stride(&self, width: u32) -> usize {
        (width as usize * self.bytes).div_ceil(self.pad) * self.pad
    }
    fn pixel(&self, color: [u8; 4], dst: &mut [u8]) {
        let mut packed = self.opaque;
        for (value, mask) in color[..3].iter().zip(self.masks) {
            let shift = mask.trailing_zeros();
            let max = u64::from(mask >> shift);
            packed |= ((((u64::from(*value) * max + 127) / 255) as u32) << shift) & mask;
        }
        let bytes = if self.little {
            packed.to_le_bytes()
        } else {
            packed.to_be_bytes()
        };
        let offset = if self.little { 0 } else { 4 - self.bytes };
        dst.copy_from_slice(&bytes[offset..offset + self.bytes]);
    }
    fn fill(&self, sampler: &Sampler<'_>, data: &mut [u8], x: u32, y: u32, width: u32, rows: u32) {
        data.fill(0);
        let stride = self.stride(width);
        for row in 0..rows {
            for column in 0..width {
                let color = sampler.at(f64::from(x + column) + 0.5, f64::from(y + row) + 0.5);
                let offset = row as usize * stride + column as usize * self.bytes;
                self.pixel(color, &mut data[offset..offset + self.bytes]);
            }
        }
    }
}

#[derive(Debug)]
struct ShmStripe {
    id: i32,
    address: *mut libc::c_void,
    len: usize,
    segment: u32,
}
impl ShmStripe {
    fn create(conn: &RustConnection, len: usize) -> Result<Self> {
        shm::query_version(conn)?.reply()?;
        // SAFETY: creates a private, user-only SysV shared memory segment.
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: maps the segment just created; errors return (void*)-1.
        let address = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if address == (-1_isize) as *mut libc::c_void {
            // SAFETY: id identifies our segment; no buffer is live.
            unsafe {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            }
            return Err(std::io::Error::last_os_error().into());
        }
        let mut stripe = Self {
            id,
            address,
            len,
            segment: 0,
        };
        stripe.segment = conn.generate_id()?;
        shm::attach(conn, stripe.segment, id as u32, true)?.check()?;
        // Mark after checked server attach. Kernel deletes only after both mappings detach.
        // SAFETY: id is owned by this object, and IPC_RMID leaves mappings valid.
        unsafe {
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
        }
        Ok(stripe)
    }
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: this object owns a writable mapping of len bytes. Server reads only
        // after put_image; the caller checks completion before entering this method again.
        unsafe { std::slice::from_raw_parts_mut(self.address.cast(), self.len) }
    }
}
impl Drop for ShmStripe {
    fn drop(&mut self) {
        // SAFETY: detaches our mapping and marks our own segment for deletion even on errors.
        unsafe {
            libc::shmdt(self.address);
            libc::shmctl(self.id, libc::IPC_RMID, std::ptr::null_mut());
        }
    }
}
fn completion(conn: &RustConnection, drawable: u32, segment: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::ShmCompletion(event)
                    if event.drawable == drawable
                        && event.shmseg == segment
                        && event.offset == 0 =>
                {
                    return Ok(());
                }
                Event::Error(error) => bail!("X11 upload error: {error:?}"),
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            bail!("MIT-SHM completion did not arrive within 10 seconds; stripe was not reused");
        }
        let mut fd = libc::pollfd {
            fd: conn.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll references one valid initialized pollfd.
        if unsafe { libc::poll(&mut fd, 1, 100) } < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
}
fn owned_matches(
    conn: &RustConnection,
    old: &Owned,
    owner_atom: u32,
    pixmap_atom: u32,
    root: u32,
) -> bool {
    let check = (|| -> Result<bool> {
        let geometry = conn.get_geometry(old.marker)?.reply()?;
        if geometry.root != root {
            return Ok(false);
        }
        let token = conn
            .get_property(false, old.marker, owner_atom, AtomEnum::STRING, 0, 8)?
            .reply()?;
        let pixmap = conn
            .get_property(false, old.marker, pixmap_atom, AtomEnum::PIXMAP, 0, 1)?
            .reply()?;
        Ok(token.format == 8
            && token.value == old.token
            && token.bytes_after == 0
            && pixmap.format == 32
            && pixmap.value32().and_then(|mut v| v.next()) == Some(old.pixmap))
    })();
    check.unwrap_or(false)
}

pub fn apply(request: &Request) -> Result<ApplyStatus> {
    crate::config::validate(&request.config)?;
    let (conn, screen_index) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_index];
    reject_xwayland(&conn, screen.root)?;
    super::print_json(&capabilities())?;
    let geometry = conn.get_geometry(screen.root)?.reply()?;
    let width = u32::from(geometry.width);
    let height = u32::from(geometry.height);
    if width == 0 || height == 0 || width > i16::MAX as u32 || height > i16::MAX as u32 {
        bail!("root dimensions exceed core X11 signed placement range");
    }
    let key = format!("screen-{screen_index}");
    if request.output.as_ref().is_some_and(|name| name != &key) {
        bail!(
            "X11 --output selects {key}; monitor-specific placement belongs to a Wayland/native owner"
        );
    }
    let config = crate::config::effective(&request.config, &key);
    let display = std::env::var("DISPLAY").context("DISPLAY is unset")?;
    let mut ownership = Ownership::open(&display, screen_index, screen.root)?;
    let owner_atom = atom(&conn, b"_TEMPLAR_WALLPAPER_OWNER")?;
    let pixmap_atom = atom(&conn, b"_TEMPLAR_WALLPAPER_PIXMAP")?;
    let old = ownership
        .old
        .take()
        .filter(|old| owned_matches(&conn, old, owner_atom, pixmap_atom, screen.root));
    let mut selector = crate::selection::SourceSelector::new();
    let source = if let Some(bytes) = request
        .config
        .outputs
        .get(&key)
        .and_then(|override_| override_.source.as_ref())
    {
        use std::os::unix::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()))
    } else {
        request.select(&mut selector)?
    };
    let decoded = super::worker::decode(&source, &request.decoder, &config)?;
    let packing = Packing::new(&conn, screen.root_depth, screen.root_visual, screen_index)?;
    let bytes = (packing.stride(width) as u64)
        .checked_mul(u64::from(height))
        .context("root byte count overflow")?;
    let stripe_bytes = packing.stride(width) * 64;
    let overlap = bytes
        .checked_add(old.as_ref().map_or(0, |old| old.bytes))
        .and_then(|b| b.checked_add(decoded.raster.bytes))
        .and_then(|b| b.checked_add(stripe_bytes as u64))
        .context("transition size overflow")?;
    if overlap > config.transition_budget_bytes {
        bail!(
            "X11 current + candidate + decoded source + stripe require {overlap} bytes, exceeding transition budget {}",
            config.transition_budget_bytes
        );
    }
    let mapping = crate::raster::RasterMapping::new(&decoded.file, &decoded.raster)?;
    let sampler = Sampler::new(
        &decoded.raster,
        mapping.as_bytes(),
        (width, height),
        config.sizing,
        config.ground,
    )?;
    let pixmap = conn.generate_id()?;
    let gc = conn.generate_id()?;
    conn.create_pixmap(
        screen.root_depth,
        pixmap,
        screen.root,
        geometry.width,
        geometry.height,
    )?
    .check()?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new())?.check()?;
    let shm_enabled = conn.query_extension(b"MIT-SHM")?.reply()?.present;
    if let Some(mut stripe) = shm_enabled
        .then(|| ShmStripe::create(&conn, stripe_bytes))
        .transpose()
        .unwrap_or(None)
    {
        for y in (0..height).step_by(64) {
            let rows = (height - y).min(64);
            packing.fill(&sampler, stripe.bytes(), 0, y, width, rows);
            shm::put_image(
                &conn,
                pixmap,
                gc,
                geometry.width,
                64,
                0,
                0,
                geometry.width,
                rows as u16,
                0,
                y as i16,
                screen.root_depth,
                u8::from(ImageFormat::Z_PIXMAP),
                true,
                stripe.segment,
                0,
            )?
            .check()?;
            completion(&conn, pixmap, stripe.segment)?;
        }
        shm::detach(&conn, stripe.segment)?.check()?;
    } else {
        // Include the BigRequests length word and all alignment in the reserve.
        let limit = conn
            .maximum_request_bytes()
            .checked_sub(128)
            .context("X11 request limit too small")?;
        let mut chunk_width = width.min((limit / packing.bytes) as u32);
        while chunk_width > 0 && packing.stride(chunk_width) > limit {
            chunk_width -= 1;
        }
        if chunk_width == 0 {
            bail!("X11 request limit cannot contain one pixel row");
        }
        let mut strip = vec![0; (packing.stride(chunk_width) * 64).min(limit)];
        for x in (0..width).step_by(chunk_width as usize) {
            let columns = (width - x).min(chunk_width);
            let stride = packing.stride(columns);
            let max_rows = (limit / stride).min(64) as u32;
            for y in (0..height).step_by(max_rows as usize) {
                let rows = (height - y).min(max_rows);
                let length = stride * rows as usize;
                packing.fill(&sampler, &mut strip[..length], x, y, columns, rows);
                conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    pixmap,
                    gc,
                    columns as u16,
                    rows as u16,
                    x as i16,
                    y as i16,
                    0,
                    screen.root_depth,
                    &strip[..length],
                )?
                .check()?;
            }
        }
    }
    conn.free_gc(gc)?.check()?;
    drop(mapping);
    drop(decoded);
    // Every upload has completed before root publication. Create the authenticated
    // marker before retaining resources; stale root properties never authorize cleanup.
    let marker = conn.generate_id()?;
    let mut token = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut token)?;
    conn.create_window(
        COPY_FROM_PARENT as u8,
        marker,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        COPY_FROM_PARENT,
        &CreateWindowAux::new(),
    )?
    .check()?;
    conn.change_property8(
        PropMode::REPLACE,
        marker,
        owner_atom,
        AtomEnum::STRING,
        &token,
    )?
    .check()?;
    conn.change_property32(
        PropMode::REPLACE,
        marker,
        pixmap_atom,
        AtomEnum::PIXMAP,
        &[pixmap],
    )?
    .check()?;
    conn.grab_server()?.check()?;
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
    )?
    .check()?;
    for name in [b"_XROOTPMAP_ID".as_slice(), b"ESETROOT_PMAP_ID".as_slice()] {
        conn.change_property32(
            PropMode::REPLACE,
            screen.root,
            atom(&conn, name)?,
            AtomEnum::PIXMAP,
            &[pixmap],
        )?
        .check()?;
    }
    conn.clear_area(false, screen.root, 0, 0, 0, 0)?.check()?;
    if let Some(old) = old {
        // Re-check under the server grab. Never FreePixmap based on public root IDs.
        if owned_matches(&conn, &old, owner_atom, pixmap_atom, screen.root) {
            conn.free_pixmap(old.pixmap)?.check()?;
            conn.destroy_window(old.marker)?.check()?;
        }
    }
    ownership.save(&Owned {
        marker,
        pixmap,
        token,
        bytes,
    })?;
    conn.set_close_down_mode(CloseDown::RETAIN_PERMANENT)?
        .check()?;
    conn.ungrab_server()?.check()?;
    conn.flush()?;
    Ok(ApplyStatus {
        backend: Backend::X11,
        accepted: true,
        presented: false,
        source: path_bytes(&source),
        buffer_bytes: bytes,
        retained_bytes: bytes,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packing_honors_byte_order_and_padding() {
        let mut pixel = [0; 3];
        let packing = Packing {
            bytes: 3,
            pad: 4,
            masks: [0xff0000, 0xff00, 0xff],
            opaque: 0,
            little: false,
        };
        packing.pixel([1, 2, 3, 255], &mut pixel);
        assert_eq!(pixel, [1, 2, 3]);
        assert_eq!(packing.stride(1), 4);
        let packing = Packing {
            little: true,
            ..packing
        };
        packing.pixel([1, 2, 3, 255], &mut pixel);
        assert_eq!(pixel, [3, 2, 1]);
    }
}
