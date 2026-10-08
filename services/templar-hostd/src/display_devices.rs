//! Authenticated, seat-scoped secondary DRM leases. One retained owner thread per
//! physical card performs open/master/close; a timeout never cancels kernel work
//! or releases admission for another thread behind a hung operation.
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fs,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::oneshot;

const ADMISSION_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DEVICE_OWNERS: usize = 8;
type Key = (String, String);
enum Operation {
    Duplicate {
        admission: Admission,
        reply: oneshot::Sender<Result<Descriptor>>,
    },
    Rearm {
        generation: u64,
        admission: Admission,
        reply: oneshot::Sender<Result<()>>,
    },
    Dispose(OwnedFd),
    Wake,
}
type Admission = Arc<AtomicBool>; // true means canceled/superseded
fn serial_after(serial: u32, previous: u32) -> bool {
    (serial.wrapping_sub(previous) as i32) > 0
}
struct Device {
    lease: String,
    pid: u32,
    start: u64,
    uid: u32,
    name: String,
    session: String,
    commands: mpsc::SyncSender<Operation>,
    busy: AtomicBool,
    retired: AtomicBool,
    revoked: AtomicBool,
    generation: AtomicU64,
    transfers: AtomicUsize,
    departed: tokio::sync::Notify,
}
#[derive(Clone, Default)]
pub(super) struct Devices {
    devices: Arc<Mutex<HashMap<Key, Arc<Device>>>>,
    admissions: Arc<Mutex<HashMap<Key, (String, u32, Admission)>>>,
}
impl std::fmt::Debug for Devices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Devices").finish_non_exhaustive()
    }
}
impl Device {
    fn revoke(&self, retire: bool) {
        if retire {
            self.retired.store(true, Ordering::Release);
        }
        self.revoked.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        // Wake is advisory: an already-full queue wakes the owner too. The flags
        // are checked after EVERY driver operation, including a late open.
        let _ = self.commands.try_send(Operation::Wake);
    }
    fn claim(&self) -> Result<()> {
        if self.retired.load(Ordering::Acquire)
            || self.transfers.load(Ordering::Acquire) != 0
            || self.busy.swap(true, Ordering::AcqRel)
        {
            bail!("secondary driver owner is retired or already executing an operation");
        }
        Ok(())
    }
}
/// Until the authenticated API has finished its post-open checks, retirement may
/// revoke master but must retain the root fd. Canceled/unpublishable duplicates
/// return to the same bounded driver owner instead of becoming a last close on
/// a D-Bus runtime thread.
pub(super) struct Descriptor {
    fd: Option<OwnedFd>,
    device: Arc<Device>,
    admission: Admission,
}
impl Descriptor {
    fn new(fd: OwnedFd, device: Arc<Device>, admission: Admission) -> Self {
        device.transfers.fetch_add(1, Ordering::AcqRel);
        Self {
            fd: Some(fd),
            device,
            admission,
        }
    }
    fn publishable(&self) -> Result<()> {
        if self.admission.load(Ordering::Acquire)
            || self.device.retired.load(Ordering::Acquire)
            || self.device.revoked.load(Ordering::Acquire)
        {
            bail!("descriptor handoff lost its authenticated lifetime");
        }
        Ok(())
    }
}
// zbus keeps the returned value alive across reply serialization AND send. Retain
// the root fd and handoff count through that boundary, not merely through API auth.
impl zbus::zvariant::Type for Descriptor {
    const SIGNATURE: &'static zbus::zvariant::Signature = &zbus::zvariant::Signature::Fd;
}
impl serde::Serialize for Descriptor {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.publishable().map_err(serde::ser::Error::custom)?;
        serde::Serialize::serialize(
            &zbus::zvariant::Fd::from(self.fd.as_ref().expect("owned reply descriptor").as_fd()),
            serializer,
        )
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            // If the queue is full, dropping this extra duplicate remains safe:
            // transfers is still nonzero, so the root fd cannot close yet.
            let _ = self.device.commands.try_send(Operation::Dispose(fd));
        }
        self.device.transfers.fetch_sub(1, Ordering::AcqRel);
        let _ = self.device.commands.try_send(Operation::Wake);
    }
}
/// Cancellation of an async bus request retires its driver owner just like a timeout.
struct RequestLife {
    device: Arc<Device>,
    completed: bool,
}
impl Drop for RequestLife {
    fn drop(&mut self) {
        if !self.completed {
            self.device.revoke(true);
        }
    }
}
/// Admission ends only AFTER the root descriptor and all driver cleanup have returned.
struct OwnerLife {
    registry: Devices,
    key: Key,
    device: Arc<Device>,
}
impl Drop for OwnerLife {
    fn drop(&mut self) {
        self.device.retired.store(true, Ordering::Release);
        let mut devices = self.registry.devices.lock();
        if devices
            .get(&self.key)
            .is_some_and(|device| Arc::ptr_eq(device, &self.device))
        {
            devices.remove(&self.key);
        }
        drop(devices);
        self.device.departed.notify_waiters();
    }
}

pub(super) async fn active_session(
    connection: &zbus::Connection,
    seat: &str,
) -> Result<(u32, String)> {
    let manager = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSeat", &(seat,)).await?;
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        path.as_str(),
        "org.freedesktop.login1.Seat",
    )
    .await?;
    let (_, active): (String, zbus::zvariant::OwnedObjectPath) =
        proxy.get_property("ActiveSession").await?;
    let session = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        active.as_str(),
        "org.freedesktop.login1.Session",
    )
    .await?;
    let (uid, _): (u32, zbus::zvariant::OwnedObjectPath) = session.get_property("User").await?;
    if !session.get_property::<bool>("Active").await? {
        bail!("seat has no active session");
    }
    Ok((uid, active.to_string()))
}

/// Resolve only trusted seat cards, with the same boot-VGA/render/alphabetical
/// primary preference as Smithay. Never accept a caller-supplied device path.
pub(super) fn secondary_path(seat: &str, gpu: &str) -> Result<PathBuf> {
    let mut cards = Vec::new();
    for entry in fs::read_dir("/sys/class/drm")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name
            .strip_prefix("card")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let path = PathBuf::from("/dev/dri").join(&name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.file_type().is_char_device()
            || metadata.uid() != 0
            || libc::major(metadata.rdev()) != 226
            || libc::minor(metadata.rdev()) >= 128
        {
            continue;
        }
        let udev = match fs::read_to_string(format!(
            "/run/udev/data/c226:{}",
            libc::minor(metadata.rdev())
        )) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let actual_seat = udev
            .lines()
            .find_map(|line| line.strip_prefix("E:ID_SEAT="))
            .unwrap_or("seat0");
        if actual_seat != seat {
            continue;
        }
        let device = fs::canonicalize(entry.path().join("device"))?;
        let identity = device
            .strip_prefix("/sys/devices")
            .unwrap_or(&device)
            .to_string_lossy()
            .into_owned();
        let boot =
            fs::read_to_string(device.join("boot_vga")).is_ok_and(|value| value.trim() == "1");
        let render = fs::read_dir(device.join("drm")).is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        });
        cards.push((path, identity, boot, render));
    }
    cards.sort_by(|left, right| left.0.cmp(&right.0));
    let primary = primary_index(&cards, |card| (card.2, card.3));
    let requested = cards
        .iter()
        .position(|card| card.1 == gpu)
        .context("GPU is not a trusted seat DRM card")?;
    if Some(requested) == primary {
        bail!("secondary descriptor API cannot grant the primary card");
    }
    Ok(cards[requested].0.clone())
}
fn primary_index<T>(cards: &[T], flags: impl Fn(&T) -> (bool, bool)) -> Option<usize> {
    cards
        .iter()
        .position(|card| flags(card).0)
        .or_else(|| cards.iter().position(|card| flags(card).1))
        .or_else(|| (!cards.is_empty()).then_some(0))
}

// Linux DRM_IO('d', 0x1e/0x1f), no pointer payload. This is always a verified
// root-opened DRM card retained on its dedicated driver-owner thread.
fn master(fd: &OwnedFd, acquire: bool) -> Result<()> {
    let request = if acquire { 0x641e } else { 0x641f } as libc::c_ulong;
    #[allow(unsafe_code)]
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn open(path: PathBuf, expected_device: u64) -> Result<OwnedFd> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let actual = file.metadata()?;
    if !actual.file_type().is_char_device() || actual.rdev() != expected_device || actual.uid() != 0
    {
        bail!("DRM node changed during admission");
    }
    let fd: OwnedFd = file.into();
    master(&fd, true)?;
    Ok(fd)
}
fn driver_owner(
    life: OwnerLife,
    path: PathBuf,
    expected_device: u64,
    admission: Admission,
    receiver: mpsc::Receiver<Operation>,
    reply: oneshot::Sender<Result<Descriptor>>,
) {
    let device = &life.device;
    if admission.load(Ordering::Acquire) || device.retired.load(Ordering::Acquire) {
        let _ = reply.send(Err(anyhow::anyhow!(
            "secondary admission was canceled before driver entry"
        )));
        return;
    }
    let fd = match open(path, expected_device) {
        Ok(fd) => fd,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
    // A timeout or client/seat death while open blocked must never publish its
    // late descriptor. Cleanup also stays on this SAME admitted thread.
    if admission.load(Ordering::Acquire)
        || device.retired.load(Ordering::Acquire)
        || device.revoked.load(Ordering::Acquire)
    {
        let _ = reply.send(Err(anyhow::anyhow!(
            "late open lost its authenticated lifetime"
        )));
        let _ = master(&fd, false);
        drop(fd);
        return;
    }
    let result = fd
        .try_clone()
        .map(|fd| Descriptor::new(fd, device.clone(), admission.clone()))
        .map_err(anyhow::Error::from);
    device.busy.store(false, Ordering::Release);
    if reply.send(result).is_err() {
        device.revoke(true);
    }
    let mut owns_master = true;
    loop {
        if device.revoked.load(Ordering::Acquire) && owns_master {
            if let Err(error) = master(&fd, false) {
                tracing::warn!(%error, "secondary DRM master revocation failed; lease retired");
                device.retired.store(true, Ordering::Release);
            }
            owns_master = false;
        }
        if device.retired.load(Ordering::Acquire) && device.transfers.load(Ordering::Acquire) == 0 {
            break;
        }
        let Ok(operation) = receiver.recv() else {
            break;
        };
        match operation {
            Operation::Wake => {}
            Operation::Dispose(fd) => drop(fd),
            Operation::Duplicate { admission, reply } => {
                let result = if !admission.load(Ordering::Acquire)
                    && owns_master
                    && !device.retired.load(Ordering::Acquire)
                    && !device.revoked.load(Ordering::Acquire)
                {
                    fd.try_clone()
                        .map(|fd| Descriptor::new(fd, device.clone(), admission))
                        .map_err(anyhow::Error::from)
                } else {
                    Err(anyhow::anyhow!("secondary descriptor is no longer active"))
                };
                device.busy.store(false, Ordering::Release);
                if reply.send(result).is_err() {
                    device.revoke(true);
                }
            }
            Operation::Rearm {
                generation,
                admission,
                reply,
            } => {
                let mut result = if admission.load(Ordering::Acquire)
                    || device.retired.load(Ordering::Acquire)
                    || device.revoked.load(Ordering::Acquire)
                    || generation != device.generation.load(Ordering::Acquire)
                {
                    Err(anyhow::anyhow!("secondary rearm lost its seat lifetime"))
                } else {
                    master(&fd, true)
                };
                owns_master |= result.is_ok();
                if admission.load(Ordering::Acquire)
                    || device.retired.load(Ordering::Acquire)
                    || device.revoked.load(Ordering::Acquire)
                    || generation != device.generation.load(Ordering::Acquire)
                {
                    if owns_master {
                        let _ = master(&fd, false);
                        owns_master = false;
                    }
                    result = Err(anyhow::anyhow!("late secondary rearm was revoked"));
                }
                device.busy.store(false, Ordering::Release);
                if reply.send(result).is_err() {
                    device.revoke(true);
                }
            }
        }
    }
    if owns_master {
        let _ = master(&fd, false);
    }
    drop(fd);
    // OwnerLife is intentionally dropped AFTER all potentially blocking cleanup.
    drop(receiver);
    drop(life);
}
impl Devices {
    pub fn retire_lease(&self, seat: &str, lease: &str) {
        for ((owner_seat, _), (owner, _, canceled)) in self.admissions.lock().iter() {
            if owner_seat == seat && owner == lease {
                canceled.store(true, Ordering::Release);
            }
        }
        for ((owner_seat, _), device) in self.devices.lock().iter() {
            if owner_seat == seat && device.lease == lease {
                device.revoke(true);
            }
        }
    }
    pub fn begin(&self, seat: &str, gpu: &str, lease: &str, serial: u32) -> Result<()> {
        let key = (seat.into(), gpu.into());
        let mut admissions = self.admissions.lock();
        if let Some((owner, previous, _)) = admissions.get(&key) {
            if owner == lease && !serial_after(serial, *previous) {
                bail!("stale BeginSecondary was superseded by a newer authenticated call");
            }
        }
        admissions.retain(|_, (_, _, canceled)| !canceled.load(Ordering::Acquire));
        if !admissions.contains_key(&key) && admissions.len() >= MAX_DEVICE_OWNERS {
            bail!("bounded secondary admission identities are occupied");
        }
        if let Some((_, _, previous)) = admissions.insert(
            key,
            (lease.into(), serial, Arc::new(AtomicBool::new(false))),
        ) {
            previous.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn admission(&self, seat: &str, gpu: &str, lease: &str, serial: u32) -> Result<Admission> {
        self.admissions.lock().get(&(seat.into(), gpu.into()))
            .filter(|(owner, begin, canceled)| owner == lease && serial_after(serial, *begin) && !canceled.load(Ordering::Acquire))
            .map(|(_, _, canceled)| canceled.clone()).context("secondary admission was canceled/superseded or requires a fresh durable BeginSecondary")
    }
    pub fn admitted(&self, seat: &str, gpu: &str, lease: &str, serial: u32) -> bool {
        self.admission(seat, gpu, lease, serial).is_ok()
            && self
                .devices
                .lock()
                .get(&(seat.into(), gpu.into()))
                .is_some_and(|device| {
                    device.lease == lease
                        && !device.retired.load(Ordering::Acquire)
                        && !device.revoked.load(Ordering::Acquire)
                })
    }
    fn lookup(
        &self,
        seat: &str,
        gpu: &str,
        lease: &str,
        pid: u32,
        start: u64,
        name: &str,
    ) -> Result<Arc<Device>> {
        let device = self
            .devices
            .lock()
            .get(&(seat.into(), gpu.into()))
            .cloned()
            .context("secondary descriptor lease is unavailable")?;
        if device.lease != lease
            || device.pid != pid
            || device.start != start
            || device.name != name
        {
            bail!("descriptor belongs to another authenticated process/name lease");
        }
        Ok(device)
    }
    pub async fn acquire(
        &self,
        connection: &zbus::Connection,
        seat: &str,
        gpu: &str,
        lease: &str,
        pid: u32,
        start: u64,
        uid: u32,
        name: &str,
        serial: u32,
    ) -> Result<Descriptor> {
        let admission = self.admission(seat, gpu, lease, serial)?;
        let (owner, session) = active_session(connection, seat).await?;
        if admission.load(Ordering::Acquire)
            || owner != uid
            || super::display_recovery::process_start(pid)? != start
        {
            bail!("descriptor owner is no longer this active seat process");
        }
        let key = (seat.to_owned(), gpu.to_owned());
        let retiring = self.devices.lock().get(&key).cloned();
        if let Some(previous) = retiring
            .filter(|previous| previous.lease != lease && previous.retired.load(Ordering::Acquire))
        {
            // Normal greeter -> user handoff may overlap process lifetimes. Wait
            // asynchronously for the ONE retired owner's actual cleanup, never
            // remove its slot or start another driver thread behind it.
            let departed = previous.departed.notified();
            tokio::pin!(departed);
            let _ = departed.as_mut().enable();
            let occupied = self
                .devices
                .lock()
                .get(&key)
                .is_some_and(|device| Arc::ptr_eq(device, &previous));
            if occupied {
                tokio::time::timeout(ADMISSION_TIMEOUT, departed).await
                    .context("previous session device owner remains occupied; its kernel cleanup is NOT canceled")?;
            }
            let (current_uid, current_session) = active_session(connection, seat).await?;
            if admission.load(Ordering::Acquire)
                || current_uid != uid
                || current_session != session
                || super::display_recovery::process_start(pid).ok() != Some(start)
            {
                bail!("new device admission lost its active session during prior-owner cleanup");
            }
        }
        let exists = self.devices.lock().contains_key(&key);
        let (device, receiver) = if exists {
            let device = self.lookup(seat, gpu, lease, pid, start, name)?;
            if device.session != session || device.revoked.load(Ordering::Acquire) {
                bail!("descriptor lease is inactive or belongs to another session");
            }
            device.claim()?;
            let (reply, receiver) = oneshot::channel();
            if device
                .commands
                .try_send(Operation::Duplicate {
                    admission: admission.clone(),
                    reply,
                })
                .is_err()
            {
                device.revoke(true);
                bail!("secondary driver owner is unavailable");
            }
            (device, receiver)
        } else {
            let path = secondary_path(seat, gpu)?;
            let expected_device = fs::symlink_metadata(&path)?.rdev();
            let (commands, operations) = mpsc::sync_channel(2);
            let device = Arc::new(Device {
                lease: lease.into(),
                pid,
                start,
                uid,
                name: name.into(),
                session,
                commands,
                busy: AtomicBool::new(true),
                retired: AtomicBool::new(false),
                revoked: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                transfers: AtomicUsize::new(0),
                departed: tokio::sync::Notify::new(),
            });
            {
                let mut devices = self.devices.lock();
                if devices.contains_key(&key) || devices.len() >= MAX_DEVICE_OWNERS {
                    bail!("bounded per-device driver admission is occupied");
                }
                devices.insert(key.clone(), device.clone());
            }
            let (reply, receiver) = oneshot::channel();
            let life = OwnerLife {
                registry: self.clone(),
                key: key.clone(),
                device: device.clone(),
            };
            std::thread::Builder::new()
                .name(format!("secondary-drm-{pid}"))
                .spawn({
                    let admission = admission.clone();
                    move || driver_owner(life, path, expected_device, admission, operations, reply)
                })
                .context("starting bounded secondary DRM owner")?;
            self.watch(connection.clone(), key, device.clone());
            (device, receiver)
        };
        let mut life = RequestLife {
            device: device.clone(),
            completed: false,
        };
        let fd = tokio::time::timeout(ADMISSION_TIMEOUT, receiver)
            .await
            .context("secondary DRM admission timed out; isolated owner retired, NOT canceled")??;
        let fd = fd?;
        if admission.load(Ordering::Acquire)
            || device.retired.load(Ordering::Acquire)
            || device.revoked.load(Ordering::Acquire)
        {
            // Descriptor retirement is delivered to the retained driver owner.
            drop(fd);
            bail!("late secondary descriptor belongs to a revoked lifetime");
        }
        life.completed = true;
        Ok(fd)
    }
    pub async fn rearm(
        &self,
        connection: &zbus::Connection,
        seat: &str,
        gpu: &str,
        lease: &str,
        pid: u32,
        start: u64,
        name: &str,
        serial: u32,
    ) -> Result<()> {
        let admission = self.admission(seat, gpu, lease, serial)?;
        let device = self.lookup(seat, gpu, lease, pid, start, name)?;
        if admission.load(Ordering::Acquire)
            || active_session(connection, seat).await? != (device.uid, device.session.clone())
            || super::display_recovery::process_start(pid).ok() != Some(start)
        {
            bail!("descriptor cannot be rearmed for another/inactive login session");
        }
        device.claim()?;
        device.revoked.store(false, Ordering::Release);
        let generation = device.generation.load(Ordering::Acquire);
        let (reply, receiver) = oneshot::channel();
        let mut life = RequestLife {
            device: device.clone(),
            completed: false,
        };
        device
            .commands
            .try_send(Operation::Rearm {
                generation,
                admission: admission.clone(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("secondary owner command queue is unavailable"))?;
        tokio::time::timeout(ADMISSION_TIMEOUT, receiver)
            .await
            .context(
                "secondary master operation timed out; retained owner retired, NOT canceled",
            )???;
        if admission.load(Ordering::Acquire)
            || device.retired.load(Ordering::Acquire)
            || device.revoked.load(Ordering::Acquire)
            || generation != device.generation.load(Ordering::Acquire)
            || active_session(connection, seat).await? != (device.uid, device.session.clone())
        {
            bail!("late secondary rearm lost its active-seat lifetime");
        }
        life.completed = true;
        Ok(())
    }
    pub fn owns(
        &self,
        seat: &str,
        gpu: &str,
        lease: &str,
        pid: u32,
        start: u64,
        name: &str,
    ) -> bool {
        self.lookup(seat, gpu, lease, pid, start, name).is_ok()
    }
    pub fn close_call(&self, seat: &str, gpu: &str, lease: &str, serial: u32) -> Result<()> {
        let key = (seat.into(), gpu.into());
        let mut admissions = self.admissions.lock();
        if let Some((owner, previous, _)) = admissions.get(&key) {
            if owner != lease || !serial_after(serial, *previous) {
                bail!("stale CloseSecondary cannot retire a newer authenticated admission");
            }
        }
        admissions
            .retain(|entry, (_, _, canceled)| entry == &key || !canceled.load(Ordering::Acquire));
        if !admissions.contains_key(&key) && admissions.len() >= MAX_DEVICE_OWNERS {
            bail!("bounded admission cancellation identities are occupied");
        }
        if let Some((_, _, canceled)) =
            admissions.insert(key, (lease.into(), serial, Arc::new(AtomicBool::new(true))))
        {
            canceled.store(true, Ordering::Release);
        }
        drop(admissions);
        self.close(seat, gpu, lease);
        Ok(())
    }
    pub fn close(&self, seat: &str, gpu: &str, lease: &str) {
        if let Some((_, _, canceled)) = self
            .admissions
            .lock()
            .get(&(seat.into(), gpu.into()))
            .filter(|(owner, _, _)| owner == lease)
        {
            canceled.store(true, Ordering::Release);
        }
        if let Some(device) = self
            .devices
            .lock()
            .get(&(seat.into(), gpu.into()))
            .cloned()
            .filter(|device| device.lease == lease)
        {
            device.revoke(true);
        }
    }
    fn watch(&self, connection: zbus::Connection, key: Key, device: Arc<Device>) {
        tokio::spawn(async move {
            let monitor = async {
                let manager = zbus::Proxy::new(&connection, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager").await?;
                let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSeat", &(key.0.as_str(),)).await?;
                let proxy = zbus::Proxy::new(&connection, "org.freedesktop.login1", path.as_str(), "org.freedesktop.login1.Seat").await?;
                let bus = zbus::Proxy::new(&connection, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus").await?;
                let mut changes = proxy.receive_property_changed::<(String, zbus::zvariant::OwnedObjectPath)>("ActiveSession").await;
                loop {
                    if device.retired.load(Ordering::Acquire) { break; }
                    let owner = tokio::time::timeout(Duration::from_secs(1), bus.call::<_, _, u32>("GetConnectionUnixProcessID", &(device.name.as_str(),))).await;
                    if !matches!(owner, Ok(Ok(pid)) if pid == device.pid)
                        || super::display_recovery::process_start(device.pid).ok() != Some(device.start) { break; }
                    let active = tokio::time::timeout(Duration::from_secs(1), active_session(&connection, &key.0)).await;
                    if !matches!(active, Ok(Ok((uid, ref session))) if uid == device.uid && session == &device.session)
                        && !device.revoked.load(Ordering::Acquire) { device.revoke(false); }
                    match tokio::time::timeout(Duration::from_secs(1), changes.next()).await {
                        Ok(Some(change)) => { let _ = change.get().await?; },
                        Ok(None) => break,
                        Err(_) => {},
                    }
                }
                anyhow::Ok(())
            }.await;
            if let Err(error) = monitor {
                tracing::warn!(%error, seat = %key.0, gpu = %key.1, "secondary descriptor lifetime monitor failed closed");
            }
            device.revoke(true);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    fn device() -> Arc<Device> {
        let (commands, _) = mpsc::sync_channel(2);
        Arc::new(Device {
            lease: "lease".into(),
            pid: 1,
            start: 2,
            uid: 3,
            name: ":1.4".into(),
            session: "session".into(),
            commands,
            busy: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            transfers: AtomicUsize::new(0),
            departed: tokio::sync::Notify::new(),
        })
    }
    #[test]
    fn primary_is_never_a_secondary_candidate() {
        assert_eq!(
            primary_index(&[(false, true), (true, true)], |card| *card),
            Some(1)
        );
        assert_eq!(
            primary_index(&[(false, false), (false, true)], |card| *card),
            Some(1)
        );
        assert_eq!(primary_index(&[(false, false)], |card| *card), Some(0));
        assert_eq!(primary_index::<(bool, bool)>(&[], |card| *card), None);
    }
    #[test]
    fn canceled_request_retires_owner_and_supersedes_late_rearm() {
        let device = device();
        device.claim().unwrap();
        let generation = device.generation.load(Ordering::Acquire);
        drop(RequestLife {
            device: device.clone(),
            completed: false,
        });
        assert!(device.retired.load(Ordering::Acquire));
        assert!(device.revoked.load(Ordering::Acquire));
        assert_ne!(generation, device.generation.load(Ordering::Acquire));
        assert!(device.claim().is_err());
    }
    #[test]
    fn close_retains_admission_until_driver_cleanup_really_finishes() {
        let registry = Devices::default();
        let key = ("seat0".into(), "gpu".into());
        let device = device();
        registry.devices.lock().insert(key.clone(), device.clone());
        let life = OwnerLife {
            registry: registry.clone(),
            key: key.clone(),
            device: device.clone(),
        };
        let departed = device.departed.notified();
        tokio::pin!(departed);
        departed.as_mut().enable();
        registry.close(&key.0, &key.1, "lease");
        assert!(registry.devices.lock().contains_key(&key));
        assert!(registry.owns(&key.0, &key.1, "lease", 1, 2, ":1.4"));
        assert!(!registry.owns(&key.0, &key.1, "lease", 1, 2, ":1.5"));
        assert!(departed.as_mut().now_or_never().is_none());
        drop(life);
        assert!(!registry.devices.lock().contains_key(&key));
        assert!(departed.as_mut().now_or_never().is_some());
    }
    #[test]
    fn authenticated_call_order_rejects_late_open_rearm_and_presentation() {
        let devices = Devices::default();
        devices.begin("seat0", "gpu", "lease", 10).unwrap();
        let first = devices.admission("seat0", "gpu", "lease", 11).unwrap();
        devices.close_call("seat0", "gpu", "lease", 12).unwrap();
        assert!(first.load(Ordering::Acquire));
        assert!(devices.admission("seat0", "gpu", "lease", 11).is_err());
        assert!(devices.begin("seat0", "gpu", "lease", 10).is_err());
        devices.begin("seat0", "gpu", "lease", 14).unwrap();
        assert!(devices.admission("seat0", "gpu", "lease", 11).is_err());
        assert!(devices.close_call("seat0", "gpu", "lease", 12).is_err());
        assert!(devices.admission("seat0", "gpu", "lease", 15).is_ok());
        assert!(serial_after(1, u32::MAX));
        assert!(!serial_after(u32::MAX, 1));
    }
    #[test]
    fn unpublished_descriptor_blocks_parallel_claim_and_canceled_publication() {
        let device = device();
        let canceled = Arc::new(AtomicBool::new(false));
        let fd = std::fs::File::open("/dev/null").unwrap().into();
        let descriptor = Descriptor::new(fd, device.clone(), canceled.clone());
        assert!(device.claim().is_err());
        canceled.store(true, Ordering::Release);
        assert!(descriptor.publishable().is_err());
        drop(descriptor);
        assert_eq!(device.transfers.load(Ordering::Acquire), 0);
    }
    #[test]
    fn released_identities_do_not_exhaust_lifetime_admission_capacity() {
        let devices = Devices::default();
        for serial in 1..=24 {
            let gpu = format!("gpu-{serial}");
            devices.begin("seat0", &gpu, "lease", serial * 2).unwrap();
            devices
                .close_call("seat0", &gpu, "lease", serial * 2 + 1)
                .unwrap();
        }
        assert!(devices.admissions.lock().len() <= MAX_DEVICE_OWNERS);
    }
}
