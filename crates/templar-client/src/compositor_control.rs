//! Typed compositor-owner transport used by sessiond, not by untrusted Settings clients.
//! Wallpaper mutation transfers one borrowed sealed raster descriptor on the frame's first
//! successful sendmsg; activity and wallpaper subscriptions retain the authenticated peer pin.
use std::{os::{fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd}, unix::fs::MetadataExt}, path::{Path, PathBuf}, time::Duration};
use templar_types::compositor_control::{ActivityObservation, CONTROL_SOCKET_NAME, ControlError, ControlReply, ControlRequest, ControlResponse, MAX_CONTROL_FRAME};
use templar_types::wallpaper::WallpaperObservation;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::UnixStream};

#[derive(Debug, Clone)]
pub struct CompositorControl { path: PathBuf, authority_uid: Option<u32> }
struct OwnerConnection { socket: UnixStream, peer_pidfd: Option<OwnedFd> }
impl CompositorControl {
    pub fn from_runtime() -> Result<Self, ControlError> {
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| error("runtime_unavailable", "XDG_RUNTIME_DIR is not set"))?);
        let uid = rustix::process::geteuid().as_raw();
        let metadata = std::fs::symlink_metadata(&runtime).map_err(|failure| error("runtime_unavailable", failure.to_string()))?;
        if !runtime.is_absolute() || !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
            return Err(error("unsafe_runtime", "The compositor runtime must be an absolute, owned0700 nonsymlink directory"));
        }
        Ok(Self { path: runtime.join(CONTROL_SOCKET_NAME), authority_uid: Some(uid) })
    }
    /// Explicit owned transport fixture. Production authorities must use from_runtime and its
    /// kernel UID/PID/cgroup check, never choose this path as an unavailable-owner fallback.
    pub fn from_socket(path: impl AsRef<Path>) -> Self { Self { path: path.as_ref().to_owned(), authority_uid: None } }
    async fn connect(&self) -> Result<OwnerConnection, ControlError> {
        let socket = UnixStream::connect(&self.path).await.map_err(|failure| error("owner_unavailable", failure.to_string()))?;
        let mut peer_pidfd = None;
        if let Some(uid) = self.authority_uid {
            let pin = socket_peer_pidfd(&socket)?;
            require_live_peer(&pin)?;
            let peer = socket.peer_cred().map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
            let pid = peer.pid().filter(|pid| *pid > 0).ok_or_else(|| error("owner_unauthenticated", "The compositor socket has no live kernel peer PID"))?;
            if peer.uid() != uid { return Err(error("owner_unauthenticated", "The compositor socket peer UID does not own this session")); }
            let cgroup = tokio::time::timeout(Duration::from_secs(1), async {
                let file = tokio::fs::File::open(format!("/proc/{pid}/cgroup")).await.map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
                let mut bytes = Vec::with_capacity(4096);
                file.take(65537).read_to_end(&mut bytes).await.map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
                if bytes.len() > 65536 { return Err(error("owner_unauthenticated", "The kernel compositor cgroup record exceeds its bound")); }
                String::from_utf8(bytes).map_err(|failure| error("owner_unauthenticated", failure.to_string()))
            }).await.map_err(|_| error("owner_unauthenticated", "Reading the compositor kernel cgroup timed out"))??;
            if !is_compositor_cgroup(&cgroup) { return Err(error("owner_unauthenticated", "The socket peer is not the managed templar-comp.service compositor authority")); }
            require_live_peer(&pin)?;
            peer_pidfd = Some(pin);
        }
        Ok(OwnerConnection { socket, peer_pidfd })
    }
    pub async fn request(&self, request: ControlRequest) -> Result<ControlResponse, ControlError> {
        self.request_descriptor(request, None).await
    }
    /// Only SetWallpaper carries a descriptor. The caller retains ownership until this future
    /// completes; the peer receives its own close-on-exec duplicate of the sealed raster.
    pub async fn request_with_fd(&self, request: ControlRequest, descriptor: BorrowedFd<'_>) -> Result<ControlResponse, ControlError> {
        self.request_descriptor(request, Some(descriptor)).await
    }
    async fn request_descriptor(&self, request: ControlRequest, descriptor: Option<BorrowedFd<'_>>) -> Result<ControlResponse, ControlError> {
        validate_descriptor(&request, descriptor.is_some())?;
        tokio::time::timeout(Duration::from_secs(12), async {
            let mut socket = self.connect().await?;
            write_request_descriptor(&mut socket.socket, &request, descriptor).await?;
            read_reply(&mut socket).await?.result
        }).await.map_err(|_| error("owner_timeout", "The compositor-owner operation timed out; refresh its observation before retrying"))?
    }
    /// Initial observation and subsequent owner-authored changes share one stream. Drop aborts
    /// the owned reader; daemon reconnect policy remains explicit at the caller.
    pub async fn subscribe_activity(&self) -> Result<ActivitySubscription, ControlError> {
        let mut socket = tokio::time::timeout(Duration::from_secs(3), self.connect()).await
            .map_err(|_| error("owner_timeout", "Connecting and authenticating compositor activity timed out"))??;
        write_request(&mut socket.socket, &ControlRequest::SubscribeActivity).await?;
        let initial = tokio::time::timeout(Duration::from_secs(3), read_reply(&mut socket)).await
            .map_err(|_| error("owner_timeout", "Subscribing to compositor activity timed out"))??;
        let ControlResponse::Activity(initial) = initial.result? else { return Err(error("incompatible_owner", "The owner returned a different observation for activity")); };
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        sender.send(Ok(initial)).await.map_err(|_| error("subscription_closed", "The activity receiver closed"))?;
        let peer_pidfd = socket.peer_pidfd.as_ref().map(OwnedFd::try_clone).transpose().map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
        let task = tokio::spawn(async move {
            loop {
                let result = match read_reply(&mut socket).await {
                    Ok(reply) => match reply.result {
                        Ok(ControlResponse::Activity(observation)) => Ok(observation),
                        Ok(_) => Err(error("incompatible_owner", "The activity stream changed response type")),
                        Err(failure) => Err(failure),
                    },
                    Err(failure) => Err(failure),
                };
                let terminal = result.is_err();
                if sender.send(result).await.is_err() || terminal { break; }
            }
        });
        Ok(ActivitySubscription { receiver, task, peer_pidfd, terminated: false })
    }
    /// Includes the initial observation, followed by accepted/presented owner changes.
    /// Dropping the subscription aborts its reader and closes the authenticated connection.
    pub async fn subscribe_wallpaper(&self) -> Result<WallpaperSubscription, ControlError> {
        let mut socket = tokio::time::timeout(Duration::from_secs(3), self.connect()).await
            .map_err(|_| error("owner_timeout", "Connecting and authenticating compositor wallpaper timed out"))??;
        write_request(&mut socket.socket, &ControlRequest::SubscribeWallpaper).await?;
        let initial = tokio::time::timeout(Duration::from_secs(3), read_reply(&mut socket)).await
            .map_err(|_| error("owner_timeout", "Subscribing to compositor wallpaper timed out"))??;
        let ControlResponse::Wallpaper(initial) = initial.result? else { return Err(error("incompatible_owner", "The owner returned a different observation for wallpaper")); };
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        sender.send(Ok(initial)).await.map_err(|_| error("subscription_closed", "The wallpaper receiver closed"))?;
        let peer_pidfd = socket.peer_pidfd.as_ref().map(OwnedFd::try_clone).transpose().map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
        let task = tokio::spawn(async move {
            loop {
                let result = match read_reply(&mut socket).await {
                    Ok(reply) => match reply.result {
                        Ok(ControlResponse::Wallpaper(observation)) => Ok(observation),
                        Ok(_) => Err(error("incompatible_owner", "The wallpaper stream changed response type")),
                        Err(failure) => Err(failure),
                    },
                    Err(failure) => Err(failure),
                };
                let terminal = result.is_err();
                if sender.send(result).await.is_err() || terminal { break; }
            }
        });
        Ok(WallpaperSubscription { receiver, task, peer_pidfd, terminated: false })
    }
    /// Native-window inventory for the count-only metadata feed. The connection owns
    /// its compositor generation; disconnect clears every count from that generation.
    pub async fn subscribe_tab_windows(&self) -> Result<TabWindowSubscription, ControlError> {
        let mut socket = tokio::time::timeout(Duration::from_secs(3), self.connect()).await
            .map_err(|_| error("owner_timeout", "Connecting tab metadata timed out"))??;
        write_request(&mut socket.socket, &ControlRequest::SubscribeTabWindows).await?;
        let initial = tokio::time::timeout(Duration::from_secs(3), read_reply(&mut socket)).await
            .map_err(|_| error("owner_timeout", "Subscribing tab metadata timed out"))??;
        let ControlResponse::TabWindows(initial) = initial.result? else {
            return Err(error("incompatible_owner", "Expected native tab-window inventory"));
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        sender.send(Ok(initial)).await.map_err(|_| error("subscription_closed", "Tab inventory receiver closed"))?;
        let peer_pidfd = socket.peer_pidfd.as_ref().map(OwnedFd::try_clone).transpose().map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
        let task = tokio::spawn(async move {
            loop {
                let result = match read_reply(&mut socket).await {
                    Ok(reply) => match reply.result {
                        Ok(ControlResponse::TabWindows(windows)) => Ok(windows),
                        Ok(_) => Err(error("incompatible_owner", "Tab inventory stream changed response type")),
                        Err(failure) => Err(failure),
                    },
                    Err(failure) => Err(failure),
                };
                let terminal = result.is_err();
                if sender.send(result).await.is_err() || terminal { break; }
            }
        });
        Ok(TabWindowSubscription { receiver, task, peer_pidfd })
    }
}
#[derive(Debug)]
pub struct ActivitySubscription {
    receiver: tokio::sync::mpsc::Receiver<Result<ActivityObservation, ControlError>>,
    task: tokio::task::JoinHandle<()>,
    peer_pidfd: Option<OwnedFd>,
    terminated: bool,
}
impl ActivitySubscription {
    pub async fn next(&mut self) -> Option<Result<ActivityObservation, ControlError>> {
        if self.terminated { return None; }
        let mut newest = self.receiver.recv().await?;
        while let Ok(next) = self.receiver.try_recv() { newest = next; }
        if let Some(pin) = &self.peer_pidfd && let Err(failure) = require_live_peer(pin) { newest = Err(failure); }
        if newest.is_err() { self.terminated = true; self.receiver.close(); self.task.abort(); }
        Some(newest)
    }
}
impl Drop for ActivitySubscription { fn drop(&mut self) { self.task.abort(); } }

#[derive(Debug)]
pub struct WallpaperSubscription {
    receiver: tokio::sync::mpsc::Receiver<Result<WallpaperObservation, ControlError>>,
    task: tokio::task::JoinHandle<()>,
    peer_pidfd: Option<OwnedFd>,
    terminated: bool,
}
impl WallpaperSubscription {
    pub async fn next(&mut self) -> Option<Result<WallpaperObservation, ControlError>> {
        if self.terminated { return None; }
        let mut newest = self.receiver.recv().await?;
        while let Ok(next) = self.receiver.try_recv() { newest = next; }
        if let Some(pin) = &self.peer_pidfd && let Err(failure) = require_live_peer(pin) { newest = Err(failure); }
        if newest.is_err() { self.terminated = true; self.receiver.close(); self.task.abort(); }
        Some(newest)
    }
}
impl Drop for WallpaperSubscription { fn drop(&mut self) { self.task.abort(); } }

#[derive(Debug)]
pub struct TabWindowSubscription {
    receiver: tokio::sync::mpsc::Receiver<Result<templar_types::compositor_control::TabWindows, ControlError>>,
    task: tokio::task::JoinHandle<()>,
    peer_pidfd: Option<OwnedFd>,
}
impl TabWindowSubscription {
    pub async fn next(&mut self) -> Result<templar_types::compositor_control::TabWindows, ControlError> {
        let next = self.receiver.recv().await.ok_or_else(|| error("subscription_closed", "Tab inventory connection ended"))?;
        if let Some(pin) = &self.peer_pidfd { require_live_peer(pin)?; }
        next
    }
}
impl Drop for TabWindowSubscription { fn drop(&mut self) { self.task.abort(); } }

fn error(code: &str, message: impl Into<String>) -> ControlError { ControlError { code: code.into(), message: message.into() } }
fn validate_descriptor(request: &ControlRequest, present: bool) -> Result<(), ControlError> {
    if matches!(request, ControlRequest::SetWallpaper { .. }) != present {
        return Err(error("invalid_descriptor", "Exactly one raster descriptor is required for SetWallpaper, and descriptors are forbidden for other requests"));
    }
    Ok(())
}
async fn write_request(socket: &mut UnixStream, request: &ControlRequest) -> Result<(), ControlError> {
    write_request_descriptor(socket, request, None).await
}
async fn write_request_descriptor(socket: &mut UnixStream, request: &ControlRequest, descriptor: Option<BorrowedFd<'_>>) -> Result<(), ControlError> {
    validate_descriptor(request, descriptor.is_some())?;
    let bytes = serde_json::to_vec(request).map_err(|failure| error("encode_failed", failure.to_string()))?;
    if bytes.len() > MAX_CONTROL_FRAME { return Err(error("request_too_large", "The compositor request exceeds its bounded frame size")); }
    let header = (bytes.len() as u32).to_be_bytes();
    let mut written = 0;
    if let Some(descriptor) = descriptor {
        loop {
            socket.writable().await.map_err(|failure| error("owner_io", failure.to_string()))?;
            match socket.try_io(tokio::io::Interest::WRITABLE, || send_frame_start(socket, &header, &bytes, descriptor)) {
                Ok(0) => return Err(error("owner_io", "The compositor socket accepted no request bytes")),
                Ok(count) => { written = count; break; },
                Err(failure) if matches!(failure.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {},
                Err(failure) => return Err(error("owner_io", failure.to_string())),
            }
        }
    }
    // A successful partial sendmsg already transferred the descriptor. Never resend it.
    if written < header.len() {
        socket.write_all(&header[written..]).await.map_err(|failure| error("owner_io", failure.to_string()))?;
    }
    socket.write_all(&bytes[written.saturating_sub(header.len())..]).await.map_err(|failure| error("owner_io", failure.to_string()))
}
#[allow(unsafe_code)]
fn send_frame_start(socket: &UnixStream, header: &[u8; 4], bytes: &[u8], descriptor: BorrowedFd<'_>) -> std::io::Result<usize> {
    let mut vectors = [
        libc::iovec { iov_base: header.as_ptr().cast_mut().cast(), iov_len: header.len() },
        libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() },
    ];
    // usize gives the control buffer cmsghdr alignment, unlike a byte array.
    let mut control = [0usize; 4];
    // SAFETY: All buffers remain live through sendmsg. The control buffer has sufficient size
    // and alignment for one cmsghdr and descriptor; iovec payloads are only read by the kernel.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = vectors.as_mut_ptr();
        message.msg_iovlen = vectors.len();
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as usize;
        let ancillary = libc::CMSG_FIRSTHDR(&raw const message);
        (*ancillary).cmsg_level = libc::SOL_SOCKET;
        (*ancillary).cmsg_type = libc::SCM_RIGHTS;
        (*ancillary).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(ancillary).cast::<libc::c_int>(), descriptor.as_raw_fd());
        let count = libc::sendmsg(socket.as_raw_fd(), &raw const message, libc::MSG_NOSIGNAL);
        if count < 0 { Err(std::io::Error::last_os_error()) } else { Ok(count as usize) }
    }
}
async fn read_reply(connection: &mut OwnerConnection) -> Result<ControlReply, ControlError> {
    if let Some(pin) = &connection.peer_pidfd { require_live_peer(pin)?; }
    let socket = &mut connection.socket;
    let mut header = [0; 4];
    socket.read_exact(&mut header).await.map_err(|failure| error("owner_io", failure.to_string()))?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_CONTROL_FRAME { return Err(error("incompatible_owner", "The compositor returned an invalid bounded frame size")); }
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await.map_err(|failure| error("owner_io", failure.to_string()))?;
    let reply = serde_json::from_slice(&bytes).map_err(|failure| error("incompatible_owner", failure.to_string()))?;
    // An exited peer may leave a buffered reply. It must not attest current lock coverage or
    // physical effects, nor may a reused numeric PID attest the original socket's identity.
    if let Some(pin) = &connection.peer_pidfd { require_live_peer(pin)?; }
    Ok(reply)
}

#[allow(unsafe_code)]
fn socket_peer_pidfd(socket: &UnixStream) -> Result<OwnedFd, ControlError> {
    let mut fd: libc::c_int = -1;
    let mut length = std::mem::size_of_val(&fd) as libc::socklen_t;
    // SAFETY: The socket remains borrowed/live. Both output pointers refer to initialized,
    // properly aligned stack values of their exact ABI sizes. On success SO_PEERPIDFD creates
    // one owned descriptor for THIS socket's kernel peer (Linux6.5+), not a numeric PID lookup.
    let status = unsafe { libc::getsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, linux_raw_sys::net::SO_PEERPIDFD as libc::c_int, (&raw mut fd).cast(), &raw mut length) };
    if status != 0 { return Err(error("owner_unauthenticated", format!("Pinning the compositor socket's kernel peer is unavailable: {}", std::io::Error::last_os_error()))); }
    if fd < 0 { return Err(error("owner_unauthenticated", "The kernel returned no compositor peer pidfd")); }
    // SAFETY: Successful SO_PEERPIDFD transfers this new descriptor to us exactly once.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if length as usize != std::mem::size_of_val(&fd) { return Err(error("owner_unauthenticated", "The kernel peer-pidfd ABI size differs")); }
    Ok(owned)
}
fn require_live_peer(pin: &OwnedFd) -> Result<(), ControlError> {
    let mut descriptors = [rustix::event::PollFd::new(pin, rustix::event::PollFlags::IN)];
    let zero = rustix::event::Timespec { tv_sec: 0, tv_nsec: 0 };
    rustix::event::poll(&mut descriptors, Some(&zero)).map_err(|failure| error("owner_unauthenticated", failure.to_string()))?;
    if !descriptors[0].revents().is_empty() { return Err(error("owner_gone", "The pinned compositor peer exited; its buffered state is no longer authoritative")); }
    Ok(())
}

fn is_compositor_cgroup(record: &str) -> bool {
    record.lines().any(|line| line.splitn(3, ':').nth(2).is_some_and(|path| path.split('/').any(|component| component == "templar-comp.service")))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use templar_types::{display::DisplayId, settings::Revision, wallpaper::{PixelFormat, RasterDescriptor, SizingMode, WallpaperImage}};

    fn wallpaper_request(output: String) -> ControlRequest {
        ControlRequest::SetWallpaper { image: WallpaperImage {
            revision: Revision::new_owner(), display_revision: Revision::new_owner(),
            output: DisplayId(output), raster: RasterDescriptor {
                version: 1, width: 1, height: 1, source_width: 1, source_height: 1,
                stride: 4, format: PixelFormat::Rgba8, palette: Vec::new(), bytes: 4,
            }, sizing: SizingMode::Fill, ground: [0, 0, 0, 255], transition_budget_bytes: 4096,
        } }
    }
    #[allow(unsafe_code)]
    async fn receive_test_bytes(socket: &UnixStream, mut bytes: &mut [u8], descriptors: &mut Vec<OwnedFd>) {
        while !bytes.is_empty() {
            socket.readable().await.unwrap();
            let result = socket.try_io(tokio::io::Interest::READABLE, || {
                let mut control = [0usize; 128];
                let mut vector = libc::iovec { iov_base: bytes.as_mut_ptr().cast(), iov_len: bytes.len() };
                // SAFETY: This fixture lends initialized, aligned buffers to recvmsg and
                // immediately owns every received descriptor before any assertions.
                unsafe {
                    let mut message: libc::msghdr = std::mem::zeroed();
                    message.msg_iov = &raw mut vector;
                    message.msg_iovlen = 1;
                    message.msg_control = control.as_mut_ptr().cast();
                    message.msg_controllen = std::mem::size_of_val(&control);
                    let count = libc::recvmsg(socket.as_raw_fd(), &raw mut message, libc::MSG_CMSG_CLOEXEC);
                    if count < 0 { return Err(std::io::Error::last_os_error()); }
                    let mut ancillary = libc::CMSG_FIRSTHDR(&raw const message);
                    while !ancillary.is_null() {
                        assert_eq!((*ancillary).cmsg_level, libc::SOL_SOCKET);
                        assert_eq!((*ancillary).cmsg_type, libc::SCM_RIGHTS);
                        let payload = (*ancillary).cmsg_len - libc::CMSG_LEN(0) as usize;
                        for index in 0..payload / std::mem::size_of::<libc::c_int>() {
                            let bytes = std::ptr::read(libc::CMSG_DATA(ancillary)
                                .add(index * std::mem::size_of::<libc::c_int>())
                                .cast::<[u8; std::mem::size_of::<libc::c_int>()]>());
                            descriptors.push(OwnedFd::from_raw_fd(libc::c_int::from_ne_bytes(bytes)));
                        }
                        ancillary = libc::CMSG_NXTHDR(&raw const message, ancillary);
                    }
                    assert_eq!(message.msg_flags & libc::MSG_CTRUNC, 0);
                    Ok(count as usize)
                }
            });
            match result {
                Ok(count) => { assert_ne!(count, 0); bytes = &mut bytes[count..]; },
                Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => {},
                Err(failure) => panic!("fixture receiving request: {failure}"),
            }
        }
    }

    #[tokio::test]
    async fn descriptor_contract_rejects_wrong_methods_before_connecting() {
        let control = CompositorControl::from_socket("/no-such-control-socket");
        let descriptor = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(control.request_with_fd(ControlRequest::Activity, descriptor.as_fd()).await.unwrap_err().code, "invalid_descriptor");
        assert_eq!(control.request(wallpaper_request("missing-descriptor".into())).await.unwrap_err().code, "invalid_descriptor");
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn partial_send_transfers_the_descriptor_only_once() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let size: libc::c_int = 4096;
        // SAFETY: setsockopt reads one initialized integer from its exact ABI-sized pointer.
        assert_eq!(unsafe { libc::setsockopt(sender.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&raw const size).cast(), std::mem::size_of_val(&size) as libc::socklen_t) }, 0);
        let request = wallpaper_request("x".repeat(100_000));
        let expected = request.clone();
        let descriptor = std::fs::File::open("/dev/null").unwrap();
        let writer = tokio::spawn(async move {
            write_request_descriptor(&mut sender, &request, Some(descriptor.as_fd())).await.unwrap();
        });
        let mut descriptors = Vec::new();
        let mut header = [0; 4];
        receive_test_bytes(&receiver, &mut header, &mut descriptors).await;
        let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
        receive_test_bytes(&receiver, &mut bytes, &mut descriptors).await;
        assert_eq!(serde_json::from_slice::<ControlRequest>(&bytes).unwrap(), expected);
        assert_eq!(descriptors.len(), 1, "a successful partial write resent SCM_RIGHTS");
        writer.await.unwrap();
    }

    async fn fixture_reply(socket: &mut UnixStream, observation: WallpaperObservation) {
        let bytes = serde_json::to_vec(&ControlReply { result: Ok(ControlResponse::Wallpaper(observation)) }).unwrap();
        socket.write_all(&(bytes.len() as u32).to_be_bytes()).await.unwrap();
        socket.write_all(&bytes).await.unwrap();
    }
    #[tokio::test]
    async fn wallpaper_subscription_includes_initial_and_changed_observations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let initial = WallpaperObservation { revision: Revision::new_owner(), outputs: Vec::new(), indexed_supported: true };
        let changed = WallpaperObservation { revision: initial.revision.at_generation(1), ..initial.clone() };
        let expected_initial = initial.clone();
        let expected_changed = changed.clone();
        let (release, released) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = [0; 4];
            socket.read_exact(&mut header).await.unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            socket.read_exact(&mut bytes).await.unwrap();
            assert_eq!(serde_json::from_slice::<ControlRequest>(&bytes).unwrap(), ControlRequest::SubscribeWallpaper);
            fixture_reply(&mut socket, initial).await;
            released.await.unwrap();
            fixture_reply(&mut socket, changed).await;
            let mut unexpected = [0];
            let _ = socket.read(&mut unexpected).await;
        });
        let mut subscription = CompositorControl::from_socket(&path).subscribe_wallpaper().await.unwrap();
        assert_eq!(subscription.next().await.unwrap().unwrap(), expected_initial);
        release.send(()).unwrap();
        assert_eq!(subscription.next().await.unwrap().unwrap(), expected_changed);
        drop(subscription);
        owner.await.unwrap();
    }
    #[test]
    fn authority_requires_an_exact_kernel_unit_component() {
        assert!(is_compositor_cgroup("0::/user.slice/user-1000.slice/user@1000.service/session.slice/templar-comp.service\n"));
        assert!(!is_compositor_cgroup("0::/session.slice/not-templar-comp.service\n"));
        assert!(!is_compositor_cgroup("0::/session.slice/templar-comp.service.attacker\n"));
        assert!(!is_compositor_cgroup("0::/session.slice/templar-comp.service-other/templar-sessiond.service\n"));
    }
}
