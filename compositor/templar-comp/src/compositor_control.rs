//! Bounded authenticated local IO and sealed-raster mapping stay off the rendering thread.
//! Each header/body is received separately with ancillary data, so stream fragmentation and
//! coalescing cannot move a raster descriptor between frames. All effects execute on calloop.
use crate::state::{Backend, TemplarCompState};
use anyhow::{Context, Result, bail};
use smithay::reexports::calloop::{
    channel::{self, Event},
    timer::{TimeoutAction, Timer},
};
use std::{
    fs::{File, OpenOptions, Permissions},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use templar_types::compositor_control::{
    ActivityObservation, CONTROL_SOCKET_NAME, ControlError, ControlReply, ControlRequest,
    ControlResponse, MAX_CONTROL_FRAME,
};
use templar_wallpaper::raster::RasterMapping;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{oneshot, watch},
};

struct Envelope {
    request: ControlRequest,
    reply: oneshot::Sender<ControlReply>,
    subscription: Option<watch::Sender<Option<ControlReply>>>,
    mapping: Option<RasterMapping>,
    alive: Arc<AtomicBool>,
}
enum Command {
    Request(Envelope),
    TabDisconnected(uuid::Uuid),
}
struct TabFeedLife {
    sender: channel::Sender<Command>,
    owner: Option<uuid::Uuid>,
}
impl Drop for TabFeedLife {
    fn drop(&mut self) {
        if let Some(owner) = self.owner {
            let _ = self.sender.send(Command::TabDisconnected(owner));
        }
    }
}
struct RequestLife(Arc<AtomicBool>);
impl Drop for RequestLife {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
pub struct ControlServer {
    task: tokio::task::JoinHandle<()>,
    socket: PathBuf,
    inode: u64,
    _lock: File,
}
impl Drop for ControlServer {
    fn drop(&mut self) {
        self.task.abort();
        if std::fs::symlink_metadata(&self.socket)
            .is_ok_and(|metadata| metadata.ino() == self.inode && metadata.file_type().is_socket())
        {
            if let Err(error) = std::fs::remove_file(&self.socket) {
                tracing::warn!(%error, "removing owned compositor control socket");
            }
        }
    }
}
#[allow(unsafe_code)]
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn install<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    runtime: &tokio::runtime::Handle,
) -> Result<ControlServer> {
    let directory = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR")
            .context("compositor control requires XDG_RUNTIME_DIR")?,
    );
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        bail!("compositor runtime must be an owned, private, nonsymlink directory");
    }
    let lock_path = directory.join("templar-comp-control-v1.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        bail!("compositor control lock must be an owned private regular file");
    }
    lock.try_lock()
        .context("another compositor owns the native control endpoint")?;
    let socket = directory.join(CONTROL_SOCKET_NAME);
    match std::fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid() => {
            std::fs::remove_file(&socket)?
        }
        Ok(_) => bail!("refusing to replace a non-owned or nonsocket compositor endpoint"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    install_listener(
        state,
        runtime,
        listener,
        socket,
        lock,
        Authentication::SessionDaemon,
    )
}

#[derive(Clone, Copy)]
enum Authentication {
    SessionDaemon,
    Fixture,
}

/// Explicit nested-compositor fixture endpoint, never used for production discovery or as
/// an authentication fallback. The winit caller must opt in with its test-input socket.
pub fn install_fixture<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    runtime: &tokio::runtime::Handle,
    path: &Path,
) -> Result<ControlServer> {
    if !path.is_absolute()
        || path
            .file_name()
            .is_none_or(|name| name != "templar-wallpaper-fixture-control.sock")
    {
        bail!("wallpaper fixture control requires its explicit absolute fixture socket path");
    }
    let directory = path
        .parent()
        .context("wallpaper fixture control requires a private parent directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        bail!("wallpaper fixture control parent must be an owned, private, nonsymlink directory");
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path.with_extension("lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        bail!("wallpaper fixture control lock must be an owned private regular file");
    }
    lock.try_lock()
        .context("another nested compositor owns the wallpaper fixture endpoint")?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid() => {
            std::fs::remove_file(path)?
        }
        Ok(_) => bail!("refusing to replace a non-owned or nonsocket wallpaper fixture endpoint"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    install_listener(
        state,
        runtime,
        listener,
        path.to_owned(),
        lock,
        Authentication::Fixture,
    )
}

fn install_listener<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    runtime: &tokio::runtime::Handle,
    listener: std::os::unix::net::UnixListener,
    socket: PathBuf,
    lock: File,
    authentication: Authentication,
) -> Result<ControlServer> {
    std::fs::set_permissions(&socket, Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let inode = std::fs::symlink_metadata(&socket)?.ino();
    let listener = {
        let _entered = runtime.enter();
        UnixListener::from_std(listener)?
    };
    let (sender, receiver) = channel::channel::<Command>();
    state
        .loop_handle
        .insert_source(receiver, |event, (), state| {
            if let Event::Msg(Command::TabDisconnected(owner)) = &event {
                crate::tab_metadata::disconnect(state, *owner);
            }
            if let Event::Msg(Command::Request(envelope)) = event {
                if !envelope.alive.load(Ordering::Acquire) {
                    return;
                }
                let mut reply = Some(envelope.reply);
                let response = dispatch(
                    state,
                    envelope.request,
                    envelope.subscription,
                    envelope.mapping,
                    &mut reply,
                );
                if let Some(reply) = reply {
                    let result = response.and_then(|response| {
                        response.ok_or_else(|| {
                            error(
                                "completion_missing",
                                "An operational request did not install its owned completion",
                            )
                        })
                    });
                    let _ = reply.send(ControlReply { result });
                }
                maintain(state);
            }
        })
        .map_err(|error| anyhow::anyhow!("inserting compositor control requests: {error}"))?;
    let task = runtime.spawn(async move {
        let permits = Arc::new(tokio::sync::Semaphore::new(4));
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = match accepted { Ok(socket) => socket, Err(error) => { tracing::warn!(%error, "accepting native compositor control"); break; } };
                    let Ok(permit) = permits.clone().try_acquire_owned() else { drop(socket); continue; };
                    let sender = sender.clone();
                    clients.spawn(async move { let _permit = permit; if let Err(error) = serve(socket, sender, authentication).await { tracing::debug!(%error, "compositor control connection ended"); } });
                },
                _ = clients.join_next(), if !clients.is_empty() => {},
            }
        }
    });
    Ok(ControlServer {
        task,
        socket,
        inode,
        _lock: lock,
    })
}

async fn authenticate(socket: &UnixStream) -> Result<()> {
    let peer = socket.peer_cred()?;
    if peer.uid() != uid() {
        bail!("native control peer has a different UID");
    }
    let pid = peer
        .pid()
        .filter(|pid| *pid > 0)
        .context("native control peer has no kernel PID")?;
    let file = tokio::fs::File::open(format!("/proc/{pid}/cgroup")).await?;
    let mut bytes = Vec::new();
    file.take(16_385).read_to_end(&mut bytes).await?;
    if bytes.len() > 16_384 {
        bail!("native control peer cgroup record exceeds its bound");
    }
    let cgroups = std::str::from_utf8(&bytes)?;
    if !cgroups.lines().any(|line| {
        line.splitn(3, ':').nth(2).is_some_and(|path| {
            path.split('/')
                .any(|part| part == "templar-sessiond.service")
        })
    }) {
        bail!("native control is restricted to the sessiond kernel cgroup");
    }
    Ok(())
}
async fn serve(
    mut socket: UnixStream,
    sender: channel::Sender<Command>,
    authentication: Authentication,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        match authentication {
            Authentication::SessionDaemon => authenticate(&socket).await,
            Authentication::Fixture => {
                if socket.peer_cred()?.uid() != uid() {
                    bail!("wallpaper fixture control peer has a different UID");
                }
                Ok(())
            }
        }
    })
    .await??;
    let (request, descriptor) =
        match tokio::time::timeout(Duration::from_secs(2), read_request(&socket)).await? {
            Ok(frame) => frame,
            Err(failure) => {
                write_reply(
                    &mut socket,
                    &ControlReply {
                        result: Err(error("invalid_descriptor", failure.to_string())),
                    },
                )
                .await?;
                return Ok(());
            }
        };
    let mut unexpected = [0; 1];
    let (request, mapping) = if let Some(file) = descriptor {
        let mapping = tokio::task::spawn_blocking(move || {
            let ControlRequest::SetWallpaper { image } = &request else {
                bail!("a raster descriptor accompanied a non-wallpaper request");
            };
            let minimum = image
                .raster
                .bytes
                .checked_mul(2)
                .and_then(|bytes| {
                    bytes.checked_add(
                        if image.raster.format == templar_types::wallpaper::PixelFormat::Indexed8 {
                            1024
                        } else {
                            0
                        },
                    )
                })
                .context("wallpaper candidate envelope overflow")?;
            if minimum > image.transition_budget_bytes {
                bail!(
                    "Wallpaper backing + candidate texture alone require {minimum} bytes; transition limit is {}",
                    image.transition_budget_bytes
                );
            }
            let mapping = RasterMapping::new(&file, &image.raster)?;
            Ok::<_, anyhow::Error>((request, Some(mapping)))
        });
        let result = tokio::select! {
            biased;
            _ = receive_ready(&socket, &mut unexpected) => bail!("Wallpaper peer ended the request before raster import completed"),
            result = mapping => result?,
        };
        match result {
            Ok(mapped) => mapped,
            Err(failure) => {
                write_reply(
                    &mut socket,
                    &ControlReply {
                        result: Err(error("invalid_raster", failure.to_string())),
                    },
                )
                .await?;
                return Ok(());
            }
        }
    } else {
        (request, None)
    };
    ensure_request_open(&socket)?;
    let subscribe_tabs = matches!(request, ControlRequest::SubscribeTabWindows);
    let subscribe = matches!(
        request,
        ControlRequest::SubscribeActivity
            | ControlRequest::SubscribeWallpaper
            | ControlRequest::SubscribeTabWindows
    );
    let (subscription_sender, mut subscription) = watch::channel(None);
    let (reply, response) = oneshot::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let _life = RequestLife(alive.clone());
    let mut tab_life = TabFeedLife {
        sender: sender.clone(),
        owner: None,
    };
    sender
        .send(Command::Request(Envelope {
            request,
            reply,
            subscription: subscribe.then_some(subscription_sender),
            mapping,
            alive,
        }))
        .map_err(|_| anyhow::anyhow!("compositor event loop stopped"))?;
    let response = tokio::select! {
        biased;
        _ = receive_ready(&socket, &mut unexpected) => bail!("Compositor peer ended the request before completion"),
        response = tokio::time::timeout(Duration::from_secs(10), response) => response??,
    };
    if subscribe_tabs && let Ok(ControlResponse::TabWindows(windows)) = &response.result {
        tab_life.owner = Some(windows.owner);
    }
    write_reply(&mut socket, &response).await?;
    if subscribe {
        loop {
            tokio::select! {
                changed = subscription.changed() => {
                    if changed.is_err() { break; }
                    let reply = subscription.borrow_and_update().clone();
                    if let Some(reply) = reply { write_reply(&mut socket, &reply).await?; }
                },
                _ = receive_ready(&socket, &mut unexpected) => break,
            }
        }
    }
    Ok(())
}

struct ReceivedChunk {
    bytes: usize,
    descriptor: Option<OwnedFd>,
}

#[allow(unsafe_code)]
fn receive_chunk(
    socket: &UnixStream,
    bytes: &mut [u8],
    control: &mut [usize],
) -> std::io::Result<ReceivedChunk> {
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: The data and aligned control buffers are live, exclusive writable borrows.
    // Every descriptor installed by recvmsg is immediately owned, before flags or protocol
    // validation can fail. Linux closes descriptors that do not fit the ancillary buffer.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(control);
        let count = libc::recvmsg(
            socket.as_raw_fd(),
            &mut message,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
        );
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut descriptor = None;
        let mut invalid = false;
        let mut ancillary = libc::CMSG_FIRSTHDR(&message);
        while !ancillary.is_null() {
            let offset = ancillary
                .cast::<u8>()
                .offset_from(message.msg_control.cast::<u8>()) as usize;
            let header = libc::CMSG_LEN(0) as usize;
            let length = (*ancillary).cmsg_len;
            if length < header || length > message.msg_controllen.saturating_sub(offset) {
                invalid = true;
                break;
            }
            if (*ancillary).cmsg_level == libc::SOL_SOCKET
                && (*ancillary).cmsg_type == libc::SCM_RIGHTS
            {
                let payload = length - header;
                if payload % std::mem::size_of::<libc::c_int>() != 0 {
                    invalid = true;
                }
                for index in 0..payload / std::mem::size_of::<libc::c_int>() {
                    let raw = std::ptr::read_unaligned(
                        libc::CMSG_DATA(ancillary).cast::<libc::c_int>().add(index),
                    );
                    if raw < 0 {
                        invalid = true;
                        continue;
                    }
                    let owned = OwnedFd::from_raw_fd(raw);
                    if descriptor.is_some() {
                        invalid = true;
                        drop(owned);
                    } else {
                        descriptor = Some(owned);
                    }
                }
            } else {
                invalid = true;
            }
            ancillary = libc::CMSG_NXTHDR(&message, ancillary);
        }
        if message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 || invalid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Truncated, duplicate, or unexpected compositor request ancillary data",
            ));
        }
        Ok(ReceivedChunk {
            bytes: count as usize,
            descriptor,
        })
    }
}
async fn receive_ready(socket: &UnixStream, bytes: &mut [u8]) -> std::io::Result<ReceivedChunk> {
    // Linux permits at most 253 SCM_RIGHTS descriptors in one message. This buffer is large
    // enough to own and close all of them even when the peer violates the one-descriptor rule.
    let mut control = [0usize; 256];
    loop {
        socket.readable().await?;
        match socket.try_io(tokio::io::Interest::READABLE, || {
            receive_chunk(socket, bytes, &mut control)
        }) {
            Err(failure)
                if matches!(
                    failure.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            result => return result,
        }
    }
}
/// Check the kernel directly after background work; epoll may not yet have delivered EOF.
fn ensure_request_open(socket: &UnixStream) -> Result<()> {
    let mut unexpected = [0; 1];
    let mut control = [0usize; 256];
    loop {
        match receive_chunk(socket, &mut unexpected, &mut control) {
            Err(failure) if failure.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(failure) => return Err(failure).context("checking compositor request liveness"),
            Ok(_) => {
                bail!("Compositor peer disconnected or sent additional data before completion")
            }
        }
    }
}
async fn receive_exact(
    socket: &UnixStream,
    mut bytes: &mut [u8],
    descriptor: &mut Option<OwnedFd>,
) -> Result<()> {
    while !bytes.is_empty() {
        let chunk = receive_ready(socket, bytes).await?;
        if let Some(received) = chunk.descriptor {
            if descriptor.is_some() {
                bail!("a compositor request frame carried duplicate descriptors");
            }
            *descriptor = Some(received);
        }
        if chunk.bytes == 0 {
            bail!("compositor request ended before its bounded frame completed");
        }
        bytes = &mut bytes[chunk.bytes..];
    }
    Ok(())
}
async fn read_request(socket: &UnixStream) -> Result<(ControlRequest, Option<File>)> {
    let mut descriptor = None;
    let mut header = [0; 4];
    // Never read beyond the current header/body boundary: ancillary data on coalesced
    // neighboring frames must not be mistaken for this frame's raster.
    receive_exact(socket, &mut header, &mut descriptor).await?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_CONTROL_FRAME {
        bail!("invalid bounded compositor request frame");
    }
    let mut bytes = vec![0; length];
    receive_exact(socket, &mut bytes, &mut descriptor).await?;
    let request: ControlRequest = serde_json::from_slice(&bytes)?;
    if matches!(request, ControlRequest::SetWallpaper { .. }) != descriptor.is_some() {
        bail!(
            "exactly one raster descriptor is required for SetWallpaper, and descriptors are forbidden for other requests"
        );
    }
    Ok((request, descriptor.map(File::from)))
}
async fn write_reply(socket: &mut UnixStream, reply: &ControlReply) -> Result<()> {
    let bytes = serde_json::to_vec(reply)?;
    if bytes.len() > MAX_CONTROL_FRAME {
        bail!("compositor observation exceeds its bounded frame");
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        socket
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await?;
        socket.write_all(&bytes).await
    })
    .await??;
    Ok(())
}
fn error(code: &str, message: impl Into<String>) -> ControlError {
    ControlError {
        code: code.into(),
        message: message.into(),
    }
}
fn native_error(failure: impl std::fmt::Display) -> ControlError {
    error("operation_failed", failure.to_string())
}
pub fn refresh_inventory<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    topology_event: bool,
) {
    let inventory = state.backend_data.display_inventory();
    let outputs: Vec<String> = inventory
        .iter()
        .filter(|display| display.connected)
        .map(|display| display.id.0.clone())
        .chain(state.space.outputs().map(|output| output.name()))
        .collect();
    state.wallpaper.retain_outputs(&outputs);
    state
        .display_control
        .replace_inventory(inventory, topology_event);
    state.display_control.replace_failures(
        state.backend_data.display_failures(),
        state.backend_data.unknown_physical_outputs(),
    );
    state.wallpaper.bind_outputs(
        state
            .space
            .outputs()
            .map(|output| {
                let name = output.name();
                let id = state
                    .display_control
                    .output_id_for_alias(&name)
                    .cloned()
                    .unwrap_or_else(|| templar_types::display::DisplayId(name.clone()));
                (name, id)
            })
            .collect(),
    );
    state
        .wallpaper
        .fence_topology(state.display_control.revision());
}

/// Compositor-owned last resort after genuine hotplug: if the settled topology left zero
/// enabled outputs with a usable connected display remaining, re-enable one through the normal
/// operational machinery — the same validate/apply/readback/completion path as a sessiond
/// restore, but with a receiver nobody listens on.
///
/// This is what recovers the laptop panel when a TV-only session loses its HDMI cable with no
/// matching saved profile and no settings client alive to ask. Only hotplug completion sites
/// call this — settled scans, device removal, settled secondary snapshots — never transaction
/// applies, so it cannot fight an in-flight change; the controller additionally refuses while
/// headless policy or any transaction owns the outputs.
pub fn maybe_emergency_fallback<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    if !state.backend_data.session_active() {
        return;
    }
    let Some(profile) = state.display_control.emergency_fallback() else {
        return;
    };
    let survivor = profile
        .outputs
        .iter()
        .find(|output| output.enabled)
        .map(|output| output.id.0.clone())
        .unwrap_or_default();
    let revision = state.display_control.revision();
    let prepared = match state
        .display_control
        .prepare_restore(revision, profile, true)
    {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(%error, "emergency display fallback failed validation; screens stay dark");
            return;
        }
    };
    if let Err(error) = apply_or_restore(state, &prepared.target, &prepared.before) {
        tracing::warn!(%error, "emergency display fallback failed to apply; screens stay dark");
        return;
    }
    refresh_inventory(state, false);
    // Nobody listens: completion send failures are already tolerated (`let _ =` at every
    // completion site), and the presented frames are the recovery, not the reply.
    let (sender, _) = oneshot::channel();
    let token = state.display_control.begin_operational(
        prepared.before,
        prepared.target,
        None,
        sender,
        Instant::now(),
    );
    B::request_redraw(state);
    if let Err(error) = arm_transaction(state, token) {
        tracing::warn!(%error, "emergency display fallback could not arm its completion");
    } else {
        tracing::warn!(
            %survivor,
            "emergency display fallback engaged: hotplug left no enabled output"
        );
    }
}
fn observation<B: Backend + 'static>(state: &TemplarCompState<B>) -> ActivityObservation {
    let capability = state.display_control.capability();
    ActivityObservation {
        revision: state.display_control.activity_revision(),
        display_revision: state.display_control.revision(),
        input_revision: state.input_control.revision(),
        activity_serial: state.idle.activity_serial(),
        idle_elapsed_ms: state.idle.idle_elapsed_ms(Instant::now()),
        session_active: state.backend_data.session_active(),
        locked: state.session_lock.locked,
        lock_generation: state.session_lock.frame_generation(),
        protected_covered: state.session_lock.protected_covered(),
        idle: state.idle.blanked(),
        blanked: state.backend_data.outputs_blanked(),
        inhibited: state.idle.inhibited(),
        idle_timeout_ms: state
            .idle
            .timeout()
            .map(|timeout| timeout.as_millis().min(u128::from(u64::MAX)) as u64),
        outputs: state.display_control.activity_outputs(),
        idle_control: capability.clone(),
        input_control: capability.clone(),
        display_control: if state.backend_data.unknown_physical_outputs() {
            templar_types::settings::Capability::Unavailable { reason: "A physical output image cannot be attested; inspect Displays failures and physical readback".into() }
        } else {
            capability
        },
    }
}
fn displays<B: Backend + 'static>(state: &TemplarCompState<B>) -> ControlResponse {
    ControlResponse::Displays(state.display_control.observation(
        state.backend_data.session_active(),
        state.session_lock.locked,
        Instant::now(),
    ))
}
fn restore_actual<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    profile: &templar_types::display::DisplayProfile,
) -> Result<(), ControlError> {
    if B::defer_restore(state, profile, None).map_err(native_error)? {
        return Err(error(
            "rollback_pending",
            "Cross-device rollback is owned asynchronously; TEST_ONLY, presentation and physical readback are still pending",
        ));
    }
    let result = B::apply_displays(state, profile)
        .and_then(|()| B::verify_displays(state, profile))
        .map_err(native_error);
    refresh_inventory(state, false);
    B::request_redraw(state);
    result
}
fn apply_or_restore<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    target: &templar_types::display::DisplayProfile,
    before: &templar_types::display::DisplayProfile,
) -> Result<(), ControlError> {
    if let Err(failure) = B::apply_displays(state, target) {
        let restored = restore_actual(state, before);
        return Err(error(
            "apply_failed",
            format!(
                "{failure}; cross-device rollback: {}",
                restored.map_or_else(
                    |failure| failure.to_string(),
                    |()| "applied and read back; repaint requested".into()
                )
            ),
        ));
    }
    Ok(())
}
fn rollback<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    token: uuid::Uuid,
) -> Result<(), ControlError> {
    let profile = state
        .display_control
        .rollback(token)
        .map_err(native_error)?;
    if B::defer_restore(state, &profile, Some(token)).map_err(native_error)? {
        return Ok(());
    }
    match restore_actual(state, &profile) {
        Ok(()) => {
            state.display_control.reverted(token, &profile);
            Ok(())
        }
        Err(failure) => {
            state
                .display_control
                .rollback_failed(token, failure.message.clone());
            tracing::warn!(message = %failure.message, "native display rollback requires backend recovery");
            Err(failure)
        }
    }
}
fn abort_operation<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    token: uuid::Uuid,
    code: &str,
    cause: &str,
) -> Result<(), ControlError> {
    let completion = state.display_control.take_completion(token);
    let restored = rollback(state, token);
    if let Some(completion) = completion {
        let detail = if state.backend_data.display_restore_pending() {
            "owned asynchronous rollback pending; old safe outputs retained".into()
        } else {
            restored.as_ref().map_or_else(
                |failure| failure.to_string(),
                |()| "applied and read back; repaint requested".into(),
            )
        };
        let _ = completion.send(ControlReply {
            result: Err(error(code, format!("{cause}; rollback: {detail}"))),
        });
    }
    restored
}
fn arm_transaction<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    token: uuid::Uuid,
) -> Result<(), ControlError> {
    match state.loop_handle.insert_source(
        Timer::from_duration(crate::display_control::PRESENTATION_TIMEOUT),
        move |_, (), state| {
            maintain(state);
            match state.display_control.deadline(token) {
                Some(deadline) => TimeoutAction::ToDuration(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(500)),
                ),
                None => TimeoutAction::Drop,
            }
        },
    ) {
        Ok(_) => Ok(()),
        Err(failure) => {
            let message = failure.to_string();
            abort_operation(state, token, "timer_failed", &message)?;
            Err(error("timer_failed", message))
        }
    }
}
pub(crate) fn complete_native_restore<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    token: Option<uuid::Uuid>,
    profile: &templar_types::display::DisplayProfile,
    completion: Option<oneshot::Sender<ControlReply>>,
    result: Result<(), templar_types::display::DisplayError>,
) {
    match &result {
        Ok(()) => {
            if let Some(token) = token {
                state.display_control.reverted(token, profile);
            }
        }
        Err(failure) => {
            if let Some(token) = token {
                state
                    .display_control
                    .rollback_failed(token, failure.to_string());
            }
        }
    }
    refresh_inventory(state, false);
    B::request_redraw(state);
    if let Some(completion) = completion {
        let result = result.map(|()| displays(state)).map_err(native_error);
        let _ = completion.send(ControlReply { result });
    }
}

/// Resume only an owned display request whose secondary TEST_ONLY results arrived.
pub(crate) fn resume_deferred<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    request: ControlRequest,
    completion: oneshot::Sender<ControlReply>,
) {
    let mut reply = Some(completion);
    let response = dispatch(state, request, None, None, &mut reply);
    if let Some(reply) = reply {
        let result = response.and_then(|response| {
            response.ok_or_else(|| {
                error(
                    "completion_missing",
                    "Native worker completion was not owned",
                )
            })
        });
        let _ = reply.send(ControlReply { result });
    }
    maintain(state);
}

fn dispatch<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    request: ControlRequest,
    subscription: Option<watch::Sender<Option<ControlReply>>>,
    mapping: Option<RasterMapping>,
    reply: &mut Option<oneshot::Sender<ControlReply>>,
) -> Result<Option<ControlResponse>, ControlError> {
    if B::defer_control(state, &request, reply)? {
        return Ok(None);
    }
    let active = state.backend_data.session_active();
    let locked = state.session_lock.locked;
    match request {
        ControlRequest::OpenLauncher {
            mode,
            scope_uri,
            query,
        } => {
            if !active || locked {
                return Err(error(
                    "session_unavailable",
                    "Launcher requires an active unlocked session",
                ));
            }
            if query.len() > 4096 || query.contains('\0') {
                return Err(error(
                    "invalid_query",
                    "Launcher query must be bounded literal text",
                ));
            }
            if let Some(uri) = &scope_uri {
                if uri.len() > 65536 || !valid_launcher_scope(uri) {
                    return Err(error(
                        "invalid_scope",
                        "Launcher scope must be an absolute local directory file URI",
                    ));
                }
            }
            if !crate::protocols_shell::send_launcher_request(
                state,
                mode,
                scope_uri.as_deref().unwrap_or(""),
                &query,
            ) {
                return Err(error(
                    "shell_unavailable",
                    "No launcher-capable trusted shell is connected",
                ));
            }
            Ok(Some(ControlResponse::Activity(observation(state))))
        }
        ControlRequest::SubscribeTabWindows => {
            crate::tab_metadata::subscribe(
                state,
                subscription.ok_or_else(|| {
                    error(
                        "subscription_missing",
                        "Tab windows require an owned subscription",
                    )
                })?,
            );
            Ok(Some(ControlResponse::TabWindows(
                crate::tab_metadata::observation(state),
            )))
        }
        ControlRequest::SetTabCounts { owner, counts } => {
            crate::tab_metadata::set_counts(state, owner, counts)?;
            Ok(Some(ControlResponse::TabWindows(
                crate::tab_metadata::observation(state),
            )))
        }
        ControlRequest::Displays => Ok(Some(displays(state))),
        ControlRequest::ValidateDisplays { expected, profile } => {
            state
                .display_control
                .validate(expected, &profile, active, locked)
                .map_err(native_error)?;
            B::validate_displays(state, &profile).map_err(native_error)?;
            Ok(Some(displays(state)))
        }
        ControlRequest::PreviewDisplays { expected, profile } => {
            let prepared = state
                .display_control
                .prepare(expected, profile, active, locked)
                .map_err(native_error)?;
            apply_or_restore(state, &prepared.target, &prepared.before)?;
            let token = prepared.token;
            refresh_inventory(state, false);
            state.display_control.begin(prepared, Instant::now());
            B::request_redraw(state);
            arm_transaction(state, token)?;
            Ok(Some(displays(state)))
        }
        ControlRequest::ConfirmDisplays { expected, token } => {
            let profile = state
                .display_control
                .confirmation_target(expected, token, Instant::now())
                .map_err(native_error)?;
            B::verify_displays(state, profile).map_err(native_error)?;
            state
                .display_control
                .confirm(expected, token, Instant::now())
                .map_err(native_error)?;
            Ok(Some(displays(state)))
        }
        ControlRequest::RevertDisplays { token } => {
            if !state.display_control.interactive_token(token) {
                return Err(error(
                    "operational_transaction",
                    "Only the compositor may complete or abort an operational transaction",
                ));
            }
            rollback(state, token)?;
            Ok(Some(displays(state)))
        }
        ControlRequest::RestoreConfirmedDisplays { expected, profile } => {
            let prepared = state
                .display_control
                .prepare_restore(expected, profile, active)
                .map_err(native_error)?;
            apply_or_restore(state, &prepared.target, &prepared.before)?;
            refresh_inventory(state, false);
            let token = state.display_control.begin_operational(
                prepared.before,
                prepared.target,
                None,
                reply.take().expect("owned control completion"),
                Instant::now(),
            );
            B::request_redraw(state);
            arm_transaction(state, token)?;
            Ok(None)
        }
        ControlRequest::SetPanelClosed {
            expected,
            closed,
            allow_headless,
        } => {
            if expected != state.display_control.revision() {
                return Err(error(
                    "stale_revision",
                    "The compositor output owner or topology changed",
                ));
            }
            if let Some(token) = state.display_control.pending_token() {
                abort_operation(
                    state,
                    token,
                    "superseded_lid",
                    "The logind-owned lid policy superseded the display transaction",
                )?;
            }
            let prepared = state
                .display_control
                .prepare_panel(
                    state.display_control.revision(),
                    closed,
                    allow_headless,
                    active,
                )
                .map_err(native_error)?;
            apply_or_restore(state, &prepared.target, &prepared.before)?;
            refresh_inventory(state, false);
            let token = state.display_control.begin_operational(
                prepared.before,
                prepared.target,
                Some(prepared.closed),
                reply.take().expect("owned control completion"),
                Instant::now(),
            );
            B::request_redraw(state);
            arm_transaction(state, token)?;
            Ok(None)
        }
        ControlRequest::Identify {
            expected,
            outputs,
            calibration,
        } => {
            if !active || locked {
                return Err(error(
                    "session_unavailable",
                    "Identify requires an active, unlocked session",
                ));
            }
            state
                .display_control
                .identify(expected, outputs, calibration, Instant::now())
                .map_err(native_error)?;
            crate::chrome::clear_identification(&mut state.chrome);
            B::request_redraw(state);
            Ok(Some(displays(state)))
        }
        ControlRequest::Inputs => Ok(Some(ControlResponse::Inputs(
            state
                .input_control
                .observation(&state.input_devices, active, locked),
        ))),
        ControlRequest::ConfigureInput {
            expected,
            id,
            configuration,
        } => state
            .input_control
            .configure(
                &mut state.input_devices,
                expected,
                &id,
                &configuration,
                active,
                locked,
            )
            .map(|observation| Some(ControlResponse::Inputs(observation)))
            .map_err(native_error),
        ControlRequest::ResetInput { expected, id } => state
            .input_control
            .reset(&mut state.input_devices, expected, &id, active, locked)
            .map(|observation| Some(ControlResponse::Inputs(observation)))
            .map_err(native_error),
        ControlRequest::Wallpaper | ControlRequest::SubscribeWallpaper => {
            state.wallpaper.publish();
            if let Some(sender) = subscription {
                state.wallpaper.subscribe(sender);
            }
            Ok(Some(ControlResponse::Wallpaper(
                state.wallpaper.observation(),
            )))
        }
        ControlRequest::SetWallpaper { image } => {
            if !active || locked {
                return Err(error(
                    "session_unavailable",
                    "Setting wallpaper requires an active, unlocked session",
                ));
            }
            if image.display_revision != state.display_control.revision() {
                return Err(error(
                    "stale_revision",
                    "The compositor output owner or topology changed",
                ));
            }
            if !state
                .space
                .outputs()
                .any(|output| output.name() == image.output.0)
                && !state
                    .display_control
                    .output_id_for_alias(&image.output.0)
                    .is_some_and(|id| id == &image.output)
            {
                return Err(error(
                    "unknown_output",
                    "The wallpaper target output is no longer present",
                ));
            }
            let mapping = mapping.ok_or_else(|| {
                error(
                    "invalid_descriptor",
                    "The wallpaper request has no validated raster mapping",
                )
            })?;
            state
                .wallpaper
                .accept(image, mapping)
                .map_err(native_error)?;
            state.wallpaper.publish();
            B::request_redraw(state);
            Ok(Some(ControlResponse::Wallpaper(
                state.wallpaper.observation(),
            )))
        }
        ControlRequest::ClearWallpaper { revision, output } => {
            // Resource release cannot expose pixels, including while locked or inactive.
            state
                .wallpaper
                .clear(revision, output)
                .map_err(native_error)?;
            state.wallpaper.publish();
            B::request_redraw(state);
            Ok(Some(ControlResponse::Wallpaper(
                state.wallpaper.observation(),
            )))
        }
        ControlRequest::Activity | ControlRequest::SubscribeActivity => {
            publish_activity(state);
            if let Some(sender) = subscription {
                state.display_control.subscribe_activity(sender);
            }
            Ok(Some(ControlResponse::Activity(observation(state))))
        }
        ControlRequest::SetIdleTimeout {
            expected,
            timeout_ms,
        } => {
            publish_activity(state);
            if expected != state.display_control.activity_revision() {
                return Err(error(
                    "stale_revision",
                    "The compositor activity owner changed",
                ));
            }
            let timeout = timeout_ms.map(Duration::from_millis);
            if timeout.is_some_and(|timeout| {
                timeout.is_zero() || Instant::now().checked_add(timeout).is_none()
            }) {
                return Err(error(
                    "invalid_timeout",
                    "Use None for never; the idle timeout must fit the native monotonic clock",
                ));
            }
            let _ = state.idle.set_timeout(timeout);
            state.display_control.policy_epoch_changed();
            if timeout.is_none() && state.idle.wake() {
                B::set_blanked(state, false);
                state.idle_notifier.set_idle(false);
            }
            state.arm_idle_timer();
            publish_activity(state);
            Ok(Some(ControlResponse::Activity(observation(state))))
        }
    }
}
pub(crate) fn publish_activity<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    let timeout_ms = state
        .idle
        .timeout()
        .map(|timeout| timeout.as_millis().min(u128::from(u64::MAX)) as u64);
    let generation = state.session_lock.frame_generation();
    let covered = state.session_lock.protected_covered();
    if !state
        .display_control
        .protection_is_current(generation, covered)
        && state.session_lock.locked
    {
        state.arm_idle_timer();
    }
    if state.display_control.activity_is_current(
        state.backend_data.session_active(),
        state.session_lock.locked,
        generation,
        covered,
        state.idle.blanked(),
        state.backend_data.outputs_blanked(),
        state.idle.inhibited(),
        timeout_ms,
        state.input_control.revision(),
        state.idle.activity_serial(),
    ) {
        return;
    }
    let observation = observation(state);
    state.display_control.publish_activity(observation);
}
pub fn maintain<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    if state
        .session_lock
        .sync_physical_state(state.backend_data.unknown_physical_outputs())
        && state.session_lock.locked
    {
        B::request_redraw(state);
        state.arm_idle_timer();
    }
    let now = Instant::now();
    if !state.backend_data.session_active()
        || state.session_lock.locked && !state.display_control.pending_allows_locked()
    {
        state.display_control.invalidate_preview();
    }
    if state.backend_data.session_active()
        && let Some(token) = state.display_control.operational_ready(now)
    {
        let verified = state
            .display_control
            .preview_target(token)
            .map_err(native_error)
            .and_then(|profile| B::verify_displays(state, profile).map_err(native_error));
        match verified {
            Ok(()) => {
                if let Some(reply) = state.display_control.complete_operational(token) {
                    let _ = reply.send(ControlReply {
                        result: Ok(displays(state)),
                    });
                }
            }
            Err(failure) => {
                let _ = abort_operation(state, token, "readback_failed", &failure.message);
            }
        }
    }
    if state.backend_data.session_active()
        && let Some(token) = state.display_control.rollback_due(now)
    {
        let _ = abort_operation(
            state,
            token,
            "presentation_timeout",
            "The display transaction expired or its topology/session was invalidated",
        );
    }
    if state
        .display_control
        .identify
        .as_ref()
        .is_some_and(|identify| {
            now >= identify.until
                || !state.backend_data.session_active()
                || state.session_lock.locked
        })
    {
        state.display_control.identify = None;
        crate::chrome::clear_identification(&mut state.chrome);
        B::request_redraw(state);
    }
    publish_activity(state);
    state.wallpaper.publish();
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;
    use std::{
        io::Read,
        os::fd::{AsFd, BorrowedFd},
    };
    use templar_types::{
        display::DisplayId,
        settings::Revision,
        wallpaper::{PixelFormat, RasterDescriptor, SizingMode, WallpaperImage},
    };

    fn wallpaper_request(output: String) -> ControlRequest {
        ControlRequest::SetWallpaper {
            image: WallpaperImage {
                revision: Revision::new_owner(),
                display_revision: Revision::new_owner(),
                output: DisplayId(output),
                raster: RasterDescriptor {
                    version: 1,
                    width: 1,
                    height: 1,
                    source_width: 1,
                    source_height: 1,
                    stride: 4,
                    format: PixelFormat::Rgba8,
                    palette: Vec::new(),
                    bytes: 4,
                },
                sizing: SizingMode::Fill,
                ground: [0, 0, 0, 255],
                transition_budget_bytes: 4096,
            },
        }
    }
    fn frame(request: &ControlRequest) -> Vec<u8> {
        let bytes = serde_json::to_vec(request).unwrap();
        let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&bytes);
        frame
    }
    fn resource() -> (OwnedFd, std::os::unix::net::UnixStream) {
        let (descriptor, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        (descriptor.into(), peer)
    }
    fn assert_closed(mut peer: std::os::unix::net::UnixStream) {
        assert_eq!(
            peer.read(&mut [0]).unwrap(),
            0,
            "a rejected frame leaked a received descriptor"
        );
    }
    #[allow(unsafe_code)]
    fn send_descriptors(socket: &UnixStream, bytes: &[u8], descriptors: &[BorrowedFd<'_>]) {
        let mut control = [0usize; 32];
        let mut vector = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: The test borrows live descriptors and buffers for the duration of sendmsg.
        unsafe {
            let size = descriptors.len() * std::mem::size_of::<libc::c_int>();
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = &mut vector;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(size as u32) as usize;
            assert!(message.msg_controllen <= std::mem::size_of_val(&control));
            let ancillary = libc::CMSG_FIRSTHDR(&message);
            (*ancillary).cmsg_level = libc::SOL_SOCKET;
            (*ancillary).cmsg_type = libc::SCM_RIGHTS;
            (*ancillary).cmsg_len = libc::CMSG_LEN(size as u32) as usize;
            for (index, descriptor) in descriptors.iter().enumerate() {
                std::ptr::write_unaligned(
                    libc::CMSG_DATA(ancillary).cast::<libc::c_int>().add(index),
                    descriptor.as_raw_fd(),
                );
            }
            assert_eq!(
                libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
                bytes.len() as isize
            );
        }
    }
    #[allow(unsafe_code)]
    fn assert_cloexec(file: &File) {
        // SAFETY: fcntl only inspects this live borrowed descriptor.
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }

    #[tokio::test]
    async fn coalesced_frames_keep_descriptor_with_its_wallpaper() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let request = wallpaper_request("test-output".into());
        let (descriptor, peer) = resource();
        sender
            .write_all(&frame(&ControlRequest::Activity))
            .await
            .unwrap();
        send_descriptors(&sender, &frame(&request), &[descriptor.as_fd()]);
        sender
            .write_all(&frame(&ControlRequest::Wallpaper))
            .await
            .unwrap();
        drop(descriptor);
        let (first, descriptor) = read_request(&receiver).await.unwrap();
        assert_eq!(first, ControlRequest::Activity);
        assert!(descriptor.is_none());
        let (second, descriptor) = read_request(&receiver).await.unwrap();
        assert_eq!(second, request);
        let descriptor = descriptor.unwrap();
        assert_cloexec(&descriptor);
        drop(descriptor);
        let (third, descriptor) = read_request(&receiver).await.unwrap();
        assert_eq!(third, ControlRequest::Wallpaper);
        assert!(descriptor.is_none());
        assert_closed(peer);
    }

    #[tokio::test]
    async fn fragmented_header_and_body_preserve_descriptor() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let request = wallpaper_request("fragmented".into());
        let bytes = frame(&request);
        let (descriptor, peer) = resource();
        send_descriptors(&sender, &bytes[..1], &[descriptor.as_fd()]);
        drop(descriptor);
        let writer = tokio::spawn(async move {
            for chunk in bytes[1..].chunks(3) {
                sender.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let (received, descriptor) = read_request(&receiver).await.unwrap();
        assert_eq!(received, request);
        assert_cloexec(descriptor.as_ref().unwrap());
        drop(descriptor);
        writer.await.unwrap();
        assert_closed(peer);
    }

    #[tokio::test]
    async fn descriptors_are_required_only_for_set_wallpaper() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender
            .write_all(&frame(&wallpaper_request("missing".into())))
            .await
            .unwrap();
        assert!(read_request(&receiver).await.is_err());
        let (descriptor, peer) = resource();
        send_descriptors(
            &sender,
            &frame(&ControlRequest::Activity),
            &[descriptor.as_fd()],
        );
        drop(descriptor);
        assert!(read_request(&receiver).await.is_err());
        assert_closed(peer);
    }

    #[tokio::test]
    async fn duplicate_descriptors_in_one_message_are_all_closed() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (descriptor, peer) = resource();
        send_descriptors(
            &sender,
            &frame(&wallpaper_request("duplicate".into())),
            &[descriptor.as_fd(), descriptor.as_fd()],
        );
        drop(descriptor);
        assert!(read_request(&receiver).await.is_err());
        assert_closed(peer);
    }

    #[tokio::test]
    async fn duplicate_descriptors_in_separate_chunks_are_all_closed() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let bytes = frame(&wallpaper_request("duplicate-chunks".into()));
        let (descriptor, peer) = resource();
        send_descriptors(&sender, &bytes[..1], &[descriptor.as_fd()]);
        send_descriptors(&sender, &bytes[1..], &[descriptor.as_fd()]);
        drop(descriptor);
        assert!(read_request(&receiver).await.is_err());
        assert_closed(peer);
    }

    #[tokio::test]
    async fn truncated_ancillary_closes_delivered_and_kernel_discarded_descriptors() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (descriptor, peer) = resource();
        send_descriptors(
            &sender,
            b"x",
            &[descriptor.as_fd(), descriptor.as_fd(), descriptor.as_fd()],
        );
        drop(descriptor);
        let mut control = [0usize; 3];
        assert!(receive_chunk(&receiver, &mut [0], &mut control).is_err());
        assert_closed(peer);
    }

    #[tokio::test]
    async fn invalid_json_and_incomplete_frames_close_descriptors() {
        for incomplete in [false, true] {
            let (mut sender, receiver) = UnixStream::pair().unwrap();
            let (descriptor, peer) = resource();
            send_descriptors(
                &sender,
                &[0, 0, 0, if incomplete { 2 } else { 1 }, b'!'],
                &[descriptor.as_fd()],
            );
            drop(descriptor);
            sender.shutdown().await.unwrap();
            assert!(read_request(&receiver).await.is_err());
            assert_closed(peer);
        }
    }
}

fn valid_launcher_scope(uri: &str) -> bool {
    templar_types::uri_list::decode_uri(uri).is_some_and(|path| {
        path.is_absolute() && !std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).contains(&0)
    })
}
