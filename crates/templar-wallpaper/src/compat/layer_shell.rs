//! Minimal layer-shell client: two immutable buffers per output at most, never a
//! release wait. A third change is rejected while the retired buffer is held.
use super::pixels::{Sampler, canvas_size};
use super::{ApplyStatus, Backend, Capabilities, Request, path_bytes, print_json};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use templar_types::wallpaper::WallpaperConfig;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

#[derive(Debug)]
struct Output {
    proxy: wl_output::WlOutput,
    name: String,
    scale: u32,
    mode: (u32, u32),
    rotated: bool,
}
#[derive(Debug)]
struct Buffer {
    proxy: wl_buffer::WlBuffer,
    id: u64,
    bytes: u64,
}
#[derive(Debug)]
struct Surface {
    proxy: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    viewport: Option<wp_viewport::WpViewport>,
    size: Option<(u32, u32)>,
    current: Option<Buffer>,
    retired: Option<Buffer>,
    dirty: bool,
    closed: bool,
}
#[derive(Debug, Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    formats: Vec<wl_shm::Format>,
    outputs: BTreeMap<u32, Output>,
    closed_outputs: BTreeSet<u32>,
    surfaces: BTreeMap<u32, Surface>,
    released: HashMap<u64, bool>,
    next_id: u64,
    sync_done: bool,
    stopped: bool,
}
impl State {
    fn capabilities(&self) -> Capabilities {
        let available = self.compositor.is_some()
            && self.shm.is_some()
            && self.shell.is_some()
            && !self.outputs.is_empty()
            && self
                .formats
                .iter()
                .any(|f| *f == wl_shm::Format::Xrgb8888 || *f == wl_shm::Format::Argb8888);
        Capabilities {
            backend: Backend::LayerShell,
            available,
            exact_sizing: available,
            reduced_buffers: self.viewporter.is_some(),
            resident_connection: true,
            indexed_storage: false,
            presentation_verified: false,
            detail: format!(
                "{} outputs; opaque shm XRGB/ARGB; wp_viewporter {}; current + one candidate/retired per output; aggregate budget reserves shm backing plus one same-size graphics allocation per buffer (additional compositor replicas unobserved); stdin next/previous/pause/resume/refresh/inspect/stop; acceptance is not presentation",
                self.outputs.len(),
                if self.viewporter.is_some() {
                    "available"
                } else {
                    "absent (reduced textures refused)"
                }
            ),
        }
    }
    fn retained_bytes(&self) -> u64 {
        self.surfaces
            .values()
            .flat_map(|s| [s.current.as_ref(), s.retired.as_ref()])
            .flatten()
            .map(|b| b.bytes)
            .sum()
    }
    fn reap(&mut self) {
        for surface in self.surfaces.values_mut() {
            if surface
                .retired
                .as_ref()
                .is_some_and(|buffer| self.released.get(&buffer.id) == Some(&true))
            {
                let retired = surface.retired.take().expect("retired checked");
                retired.proxy.destroy();
                self.released.remove(&retired.id);
            }
        }
    }
    fn remove_closed(&mut self) {
        let closed: Vec<u32> = self
            .surfaces
            .iter()
            .filter_map(|(id, s)| s.closed.then_some(*id))
            .collect();
        for id in closed {
            if let Some(surface) = self.surfaces.remove(&id) {
                surface.layer.destroy();
                if let Some(viewport) = surface.viewport {
                    viewport.destroy();
                }
                surface.proxy.destroy();
                for buffer in [surface.current, surface.retired].into_iter().flatten() {
                    buffer.proxy.destroy();
                    self.released.remove(&buffer.id);
                }
            }
        }
        self.stopped = self.surfaces.is_empty();
    }
}

/// Dispatch with an actual finite socket deadline, rather than blocking_roundtrip
/// or a session-long watchdog thread. Disconnect always terminates residency.
fn dispatch(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
) -> Result<()> {
    if queue.dispatch_pending(state)? > 0 {
        return Ok(());
    }
    connection.flush()?;
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(state)?;
        return Ok(());
    };
    let mut poll = libc::pollfd {
        fd: connection.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll references one valid initialized descriptor.
    let count = unsafe {
        libc::poll(
            &mut poll,
            1,
            timeout.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if count < 0 {
        drop(guard);
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    } else if count > 0 {
        guard.read()?;
        queue.dispatch_pending(state)?;
    } else {
        drop(guard);
    }
    Ok(())
}
fn sync(connection: &Connection, queue: &mut EventQueue<State>, state: &mut State) -> Result<()> {
    state.sync_done = false;
    connection.display().sync(&queue.handle(), ());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !state.sync_done {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("compositor did not complete setup within ten seconds");
        }
        dispatch(connection, queue, state, remaining)?;
    }
    Ok(())
}
fn connect() -> Result<(Connection, EventQueue<State>, State)> {
    let connection = Connection::connect_to_env().context("connecting to WAYLAND_DISPLAY")?;
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    sync(&connection, &mut queue, &mut state)?;
    sync(&connection, &mut queue, &mut state)?; // bound output names/scale and shm formats
    Ok((connection, queue, state))
}
pub fn inspect() -> Result<Capabilities> {
    let (_, _, state) = connect()?;
    Ok(state.capabilities())
}

fn add_surface(state: &mut State, qh: &QueueHandle<State>, id: u32) -> Result<()> {
    let output = state.outputs.get(&id).context("output disappeared")?;
    let proxy = state
        .compositor
        .as_ref()
        .context("wl_compositor missing")?
        .create_surface(qh, id);
    let region = state
        .compositor
        .as_ref()
        .expect("checked compositor")
        .create_region(qh, ());
    proxy.set_input_region(Some(&region));
    region.destroy();
    let layer = state
        .shell
        .as_ref()
        .context("zwlr_layer_shell_v1 missing")?
        .get_layer_surface(
            &proxy,
            Some(&output.proxy),
            zwlr_layer_shell_v1::Layer::Background,
            "templar-wallpaper".into(),
            qh,
            id,
        );
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    let viewport = state
        .viewporter
        .as_ref()
        .map(|v| v.get_viewport(&proxy, qh, ()));
    proxy.commit(); // no buffer until Configure has been acknowledged
    state.surfaces.insert(
        id,
        Surface {
            proxy,
            layer,
            viewport,
            size: None,
            current: None,
            retired: None,
            dirty: true,
            closed: false,
        },
    );
    Ok(())
}
fn configured_size(state: &State, id: u32) -> Result<((u32, u32), u32)> {
    let output = state.outputs.get(&id).context("output disappeared")?;
    let mut logical = state
        .surfaces
        .get(&id)
        .and_then(|s| s.size)
        .context("layer surface not configured")?;
    let scale = output.scale.max(1);
    let mode = if output.rotated {
        (output.mode.1, output.mode.0)
    } else {
        output.mode
    };
    if logical.0 == 0 {
        logical.0 = mode.0 / scale;
    }
    if logical.1 == 0 {
        logical.1 = mode.1 / scale;
    }
    if logical.0 == 0 || logical.1 == 0 {
        bail!("compositor supplied no usable layer dimensions");
    }
    if logical.0 > i32::MAX as u32 || logical.1 > i32::MAX as u32 {
        bail!("logical dimensions exceed protocol limits");
    }
    Ok((logical, scale))
}
fn apply_output(
    state: &mut State,
    qh: &QueueHandle<State>,
    id: u32,
    source: &Path,
    decoder: &Path,
    base: &WallpaperConfig,
) -> Result<ApplyStatus> {
    state.reap();
    let output = state.outputs.get(&id).context("output disappeared")?;
    let config = crate::config::effective(base, &output.name);
    let surface = state.surfaces.get(&id).context("surface disappeared")?;
    if surface.retired.is_some() {
        bail!(
            "{}: previous buffer is unreleased; candidate slot unavailable, current wallpaper preserved (no release wait)",
            output.name
        );
    }
    let (logical, scale) = configured_size(state, id)?;
    let physical = (
        logical
            .0
            .checked_mul(scale)
            .context("output width overflow")?,
        logical
            .1
            .checked_mul(scale)
            .context("output height overflow")?,
    );
    let decoded = super::worker::decode(source, decoder, &config)?;
    let size = canvas_size(
        &decoded.raster,
        physical,
        config.sizing,
        config.texture,
        surface.viewport.is_some(),
    )?;
    let stride = size.0.checked_mul(4).context("buffer stride overflow")?;
    let bytes = u64::from(stride)
        .checked_mul(u64::from(size.1))
        .context("buffer length overflow")?;
    if bytes > i32::MAX as u64 || size.0 > i32::MAX as u32 || size.1 > i32::MAX as u32 {
        bail!("shm buffer exceeds protocol signed limits");
    }
    let retained = state.retained_bytes();
    let peak = retained
        .checked_add(bytes)
        .and_then(|b| b.checked_mul(2))
        .and_then(|b| b.checked_add(decoded.raster.bytes))
        .and_then(|b| b.checked_add(u64::from(stride)))
        .context("transition byte count overflow")?;
    if peak
        > base
            .transition_budget_bytes
            .min(config.transition_budget_bytes)
    {
        bail!(
            "aggregate shm + one graphics copy per buffer + decoded source + row require {peak} bytes; transition budget exceeded, current wallpaper preserved"
        );
    }
    let file = crate::raster::create_memfd()?;
    file.set_len(bytes)?;
    {
        // One source mapping plus one row, never an expanded CPU image mirror.
        let mapping = crate::raster::RasterMapping::new(&decoded.file, &decoded.raster)?;
        let sampler = Sampler::new(
            &decoded.raster,
            mapping.as_bytes(),
            physical,
            config.sizing,
            config.ground,
        )?;
        let mut row = vec![0; stride as usize];
        let mut writer = &file;
        for y in 0..size.1 {
            let py = (f64::from(y) + 0.5) * f64::from(physical.1) / f64::from(size.1);
            for x in 0..size.0 {
                let px = (f64::from(x) + 0.5) * f64::from(physical.0) / f64::from(size.0);
                let [r, g, b, _] = sampler.at(px, py);
                row[x as usize * 4..x as usize * 4 + 4]
                    .copy_from_slice(&u32::from_be_bytes([255, r, g, b]).to_ne_bytes());
            }
            writer.write_all(&row)?;
        }
    }
    drop(decoded); // no source raster or mapping is retained by this connection
    // wl_shm servers may mmap the pool read/write (notably libwayland-server).
    // Unlike the native raster wire, F_SEAL_WRITE is therefore incompatible.
    // Fix its extent, then close every client FD after construction: we never
    // mutate, punch holes in, or reuse backing while the compositor can use it.
    // SAFETY: fcntl operates on our owned memfd and scalar seal flags.
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let shm = state.shm.as_ref().context("wl_shm missing")?;
    let format = if state.formats.contains(&wl_shm::Format::Xrgb8888) {
        wl_shm::Format::Xrgb8888
    } else {
        wl_shm::Format::Argb8888
    };
    state.next_id = state
        .next_id
        .checked_add(1)
        .context("buffer generation overflow")?;
    let generation = state.next_id;
    let pool = shm.create_pool(file.as_fd(), bytes as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        size.0 as i32,
        size.1 as i32,
        stride as i32,
        format,
        qh,
        generation,
    );
    pool.destroy();
    drop(file); // client cannot mutate backing and maps nothing
    state.released.insert(generation, false);
    let surface = state
        .surfaces
        .get_mut(&id)
        .expect("surface retained during synchronous preparation");
    if let Some(viewport) = &surface.viewport {
        surface.proxy.set_buffer_scale(1);
        viewport.set_destination(logical.0 as i32, logical.1 as i32);
    } else {
        surface.proxy.set_buffer_scale(scale as i32);
    }
    surface.proxy.attach(Some(&buffer), 0, 0);
    if surface.proxy.version() >= 4 {
        surface.proxy.damage_buffer(0, 0, i32::MAX, i32::MAX);
    } else {
        surface.proxy.damage(0, 0, i32::MAX, i32::MAX);
    }
    surface.proxy.commit();
    if let Some(current) = surface.current.take() {
        if state.released.get(&current.id) == Some(&true) {
            current.proxy.destroy();
            state.released.remove(&current.id);
        } else {
            surface.retired = Some(current);
        }
    }
    surface.current = Some(Buffer {
        proxy: buffer,
        id: generation,
        bytes,
    });
    surface.dirty = false;
    Ok(ApplyStatus {
        backend: Backend::LayerShell,
        accepted: true,
        presented: false,
        source: path_bytes(source),
        buffer_bytes: bytes,
        retained_bytes: state.retained_bytes(),
        error: None,
    })
}
fn source_for(base: &WallpaperConfig, output: &str, shared: &Path) -> PathBuf {
    base.outputs
        .get(output)
        .and_then(|o| o.source.as_ref())
        .map_or_else(
            || shared.to_path_buf(),
            |bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes.clone())),
        )
}
fn apply_all(
    state: &mut State,
    qh: &QueueHandle<State>,
    source: &Path,
    decoder: &Path,
    config: &WallpaperConfig,
) -> Result<bool> {
    let ids: Vec<u32> = state.surfaces.keys().copied().collect();
    let mut accepted = false;
    for id in ids {
        let output = state.outputs.get(&id).context("output disappeared")?;
        let selected = source_for(config, &output.name, source);
        match apply_output(state, qh, id, &selected, decoder, config) {
            Ok(status) => {
                accepted = true;
                print_json(&status)?;
            }
            Err(error) => {
                if let Some(surface) = state.surfaces.get_mut(&id) {
                    if surface.retired.is_none() {
                        surface.dirty = false;
                    }
                }
                print_json(&ApplyStatus {
                    backend: Backend::LayerShell,
                    accepted: false,
                    presented: false,
                    source: path_bytes(&selected),
                    buffer_bytes: 0,
                    retained_bytes: state.retained_bytes(),
                    error: Some(error.to_string()),
                })?;
            }
        }
    }
    Ok(accepted)
}

/// Resident only for the life of this compositor connection. stdin is an optional
/// control channel; EOF disables it, not the wallpaper. No polling redraw timer.
pub fn run(request: &Request) -> Result<()> {
    crate::config::validate(&request.config)?;
    let (connection, mut queue, mut state) = connect()?;
    let capabilities = state.capabilities();
    if !capabilities.available {
        bail!("compositor lacks layer-shell, wl_shm formats, compositor, or an output");
    }
    print_json(&capabilities)?;
    if request.config.texture != templar_types::wallpaper::TextureLimit::Native
        && state.viewporter.is_none()
    {
        bail!("reduced buffers unavailable without wp_viewporter");
    }
    let selected: Vec<u32> = state
        .outputs
        .iter()
        .filter_map(|(id, output)| {
            request
                .output
                .as_ref()
                .is_none_or(|name| name == &output.name)
                .then_some(*id)
        })
        .collect();
    if selected.is_empty() {
        bail!("requested output is not advertised");
    }
    let qh = queue.handle();
    for id in selected {
        add_surface(&mut state, &qh, id)?;
    }
    let configure_deadline = Instant::now() + Duration::from_secs(10);
    while state
        .surfaces
        .values()
        .any(|s| s.size.is_none() && !s.closed)
    {
        let remaining = configure_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("layer-shell configure deadline expired");
        }
        dispatch(&connection, &mut queue, &mut state, remaining)?;
    }
    state.remove_closed();
    if state.stopped {
        bail!("compositor closed all wallpaper surfaces");
    }
    let mut selector = crate::selection::SourceSelector::new();
    let mut source = request.select(&mut selector)?;
    if !apply_all(&mut state, &qh, &source, &request.decoder, &request.config)? {
        bail!("no output accepted the initial wallpaper");
    }
    selector.record_success(&source);
    connection.flush()?;
    let interval = request.config.interval_seconds.map(Duration::from_secs);
    let mut deadline = interval.map(|interval| Instant::now() + interval);
    let mut paused = false;
    let mut input_open = true;
    let mut input = Vec::new();
    loop {
        queue.dispatch_pending(&mut state)?;
        state.reap();
        state.remove_closed();
        if state.stopped {
            break;
        }
        // Newly plugged outputs join this explicit generic owner. Removed outputs
        // release their surface resources without keeping any source raster cache.
        let new: Vec<u32> = state
            .outputs
            .iter()
            .filter_map(|(id, output)| {
                (!state.surfaces.contains_key(id)
                    && !state.closed_outputs.contains(id)
                    && request
                        .output
                        .as_ref()
                        .is_none_or(|name| name == &output.name))
                .then_some(*id)
            })
            .collect();
        for id in new {
            add_surface(&mut state, &qh, id)?;
        }
        let dirty: Vec<u32> = state
            .surfaces
            .iter()
            .filter_map(|(id, surface)| {
                (surface.dirty && surface.size.is_some() && surface.retired.is_none())
                    .then_some(*id)
            })
            .collect();
        for id in dirty {
            let selected = source_for(&request.config, &state.outputs[&id].name, &source);
            match apply_output(
                &mut state,
                &qh,
                id,
                &selected,
                &request.decoder,
                &request.config,
            ) {
                Ok(status) => print_json(&status)?,
                Err(error) => {
                    state.surfaces.get_mut(&id).expect("surface exists").dirty = false;
                    print_json(&ApplyStatus {
                        backend: Backend::LayerShell,
                        accepted: false,
                        presented: false,
                        source: path_bytes(&selected),
                        buffer_bytes: 0,
                        retained_bytes: state.retained_bytes(),
                        error: Some(error.to_string()),
                    })?;
                }
            }
        }
        let timed = !paused && deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if timed {
            match selector.select(
                &request.config,
                Some(&source),
                crate::selection::Direction::Next,
            ) {
                Ok(candidate) => {
                    if apply_all(
                        &mut state,
                        &qh,
                        &candidate,
                        &request.decoder,
                        &request.config,
                    )? {
                        selector.record_success(&candidate);
                        source = candidate;
                    }
                }
                Err(error) => print_json(
                    &serde_json::json!({"backend":"layer_shell", "accepted":false,"error":error.to_string()}),
                )?,
            }
            // Failed attempts do not change source/history or spin on an expired
            // deadline: the next scheduled attempt is one full interval later.
            deadline = interval.map(|interval| Instant::now() + interval);
            continue;
        }
        connection.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut polls = [
            libc::pollfd {
                fd: connection.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if input_open { 0 } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = if paused {
            -1
        } else {
            deadline.map_or(-1, |d| {
                d.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128) as i32
            })
        };
        // SAFETY: poll references this initialized two-element array.
        let count = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
        if count < 0 {
            drop(guard);
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if polls[0].revents != 0 {
            guard.read()?;
        } else {
            drop(guard);
        }
        queue.dispatch_pending(&mut state)?;
        state.reap();
        state.remove_closed();
        if state.stopped {
            break;
        }
        if polls[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut bytes = [0_u8; 4096];
            // SAFETY: fd 0 is the optional command input; the buffer is writable.
            let n = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n <= 0 {
                input_open = false;
            } else {
                if input.len() + n as usize > 65536 {
                    bail!("stdin command queue exceeds 64 KiB");
                }
                input.extend_from_slice(&bytes[..n as usize]);
                while let Some(end) = input.iter().position(|b| *b == b'\n') {
                    let command = String::from_utf8(input.drain(..=end).collect())?;
                    match command.trim() {
                        "stop" => return Ok(()),
                        "pause" => {
                            paused = true;
                            print_json(&serde_json::json!({"paused":true}))?;
                        }
                        "resume" => {
                            paused = false;
                            deadline = interval.map(|i| Instant::now() + i);
                            print_json(&serde_json::json!({"paused":false}))?;
                        }
                        "inspect" => print_json(
                            &serde_json::json!({"capabilities":state.capabilities(),"source":path_bytes(&source),"paused":paused,"retained_bytes":state.retained_bytes()}),
                        )?,
                        "next" | "previous" | "refresh" => {
                            if command.trim() == "refresh" {
                                selector.refresh();
                            }
                            let candidate = if command.trim() == "refresh" {
                                Ok(source.clone())
                            } else {
                                selector.select(
                                    &request.config,
                                    Some(&source),
                                    if command.trim() == "previous" {
                                        crate::selection::Direction::Previous
                                    } else {
                                        crate::selection::Direction::Next
                                    },
                                )
                            };
                            match candidate {
                                Ok(candidate) => {
                                    if apply_all(
                                        &mut state,
                                        &qh,
                                        &candidate,
                                        &request.decoder,
                                        &request.config,
                                    )? {
                                        selector.record_success(&candidate);
                                        source = candidate;
                                        deadline = interval.map(|i| Instant::now() + i);
                                    }
                                }
                                Err(error) => print_json(
                                    &serde_json::json!({"accepted":false,"error":error.to_string()}),
                                )?,
                            }
                        }
                        "" => {}
                        other => print_json(
                            &serde_json::json!({"accepted":false,"error":format!("unknown command: {other}")}),
                        )?,
                    }
                }
            }
        }
    }
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    let proxy = registry.bind(name, version.min(4), qh, name);
                    state.outputs.insert(
                        name,
                        Output {
                            proxy,
                            name: format!("wl-output-{name}"),
                            scale: 1,
                            mode: (0, 0),
                            rotated: false,
                        },
                    );
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    if output.proxy.version() >= 3 {
                        output.proxy.release();
                    }
                }
                state.closed_outputs.remove(&name);
                if let Some(surface) = state.surfaces.get_mut(&name) {
                    surface.closed = true;
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => output.name = name,
            wl_output::Event::Scale { factor } if factor > 0 => {
                output.scale = factor as u32;
                if let Some(surface) = state.surfaces.get_mut(id) {
                    surface.dirty = true;
                }
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) && width > 0 && height > 0 => {
                output.mode = (width as u32, height as u32)
            }
            wl_output::Event::Geometry {
                transform: WEnum::Value(transform),
                ..
            } => {
                output.rotated = matches!(
                    transform,
                    wl_output::Transform::_90
                        | wl_output::Transform::_270
                        | wl_output::Transform::Flipped90
                        | wl_output::Transform::Flipped270
                )
            }
            _ => {}
        }
    }
}
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(surface) = state.surfaces.get_mut(id) else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                surface.size = Some((width, height));
                surface.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                surface.closed = true;
                state.closed_outputs.insert(*id);
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_surface::WlSurface, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            if factor > 0 {
                if let Some(output) = state.outputs.get_mut(id) {
                    output.scale = factor as u32;
                }
                if let Some(surface) = state.surfaces.get_mut(id) {
                    surface.dirty = true;
                }
            }
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        generation: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(released) = state.released.get_mut(generation) {
                *released = true;
            }
        }
    }
}
impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format {
            format: WEnum::Value(format),
        } = event
        {
            if !state.formats.contains(&format) {
                state.formats.push(format);
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.sync_done = true;
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
