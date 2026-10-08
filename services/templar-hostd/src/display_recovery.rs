//! Root-owned, boot-local crash breaker shared by the greeter and user compositor.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};
use zbus::message::Header;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Marker {
    pid: u32,
    start: u64,
    uid: u32,
    lease: String,
    sender: String,
    session: String,
    pending: BTreeSet<String>,
    recovery: bool,
}
impl Marker {
    fn owner_matches(&self, pid: u32, uid: u32, start: u64, sender: &str, session: &str) -> bool {
        (
            self.pid,
            self.uid,
            self.start,
            self.sender.as_str(),
            self.session.as_str(),
        ) == (pid, uid, start, sender, session)
    }
}
#[derive(Debug)]
pub struct DisplayRecoveryApi {
    directory: PathBuf,
    connection: zbus::Connection,
    serial: Mutex<()>,
    devices: super::display_devices::Devices,
}
impl DisplayRecoveryApi {
    pub fn new(root: &Path, connection: zbus::Connection) -> Result<Self> {
        let directory = root.join("run/templaros/display-recovery");
        for parent in [
            root.to_path_buf(),
            root.join("run"),
            root.join("run/templaros"),
        ] {
            if !parent.exists() {
                fs::create_dir(&parent)?;
            }
            let metadata = fs::symlink_metadata(&parent)?;
            if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                bail!(
                    "display recovery ancestors must be root-owned real directories without group/other write access"
                );
            }
        }
        fs::create_dir_all(&directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != 0 {
            bail!("display recovery directory must be a root-owned real directory");
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            directory,
            connection,
            serial: Mutex::new(()),
            devices: super::display_devices::Devices::default(),
        })
    }
    async fn caller_process(&self, header: &Header<'_>) -> Result<(u32, u32, u64)> {
        let sender = header
            .sender()
            .context("missing authenticated bus sender")?;
        let bus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        let pid = bus
            .get_connection_unix_process_id(sender.clone().into())
            .await?;
        let uid = bus.get_connection_unix_user(sender.clone().into()).await?;
        Ok((pid, uid, process_start(pid)?))
    }
    async fn caller_identity(&self, header: &Header<'_>) -> Result<(u32, u32, u64)> {
        let (pid, uid, start) = self.caller_process(header).await?;
        let exe = fs::metadata(format!("/proc/{pid}/exe"))?;
        let installed = fs::metadata("/usr/bin/templar-comp")?;
        if !installed.is_file()
            || exe.dev() != installed.dev()
            || exe.ino() != installed.ino()
            || installed.uid() != 0
            || installed.mode() & 0o022 != 0
        {
            bail!("caller is not the installed root-owned compositor executable");
        }
        Ok((pid, uid, start))
    }
    async fn caller(&self, header: &Header<'_>, seat: &str) -> Result<(u32, u32, u64, String)> {
        if seat.is_empty()
            || seat.len() > 64
            || !seat
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            bail!("invalid seat identity");
        }
        let (pid, uid, start) = self.caller_identity(header).await?;
        let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        let session_comp = session_service(uid, &cgroup);
        let manager = zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await?;
        let identity = if session_comp {
            let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSeat", &(seat,)).await?;
            let proxy = zbus::Proxy::new(
                &self.connection,
                "org.freedesktop.login1",
                path.as_str(),
                "org.freedesktop.login1.Seat",
            )
            .await?;
            let (_, active): (String, zbus::zvariant::OwnedObjectPath) =
                proxy.get_property("ActiveSession").await?;
            let session = zbus::Proxy::new(
                &self.connection,
                "org.freedesktop.login1",
                active.as_str(),
                "org.freedesktop.login1.Session",
            )
            .await?;
            let (owner, _): (u32, zbus::zvariant::OwnedObjectPath) =
                session.get_property("User").await?;
            if owner != uid {
                bail!("compositor does not own this seat's active login session");
            }
            if !session.get_property::<bool>("Active").await? {
                bail!("compositor seat session is inactive");
            }
            active.to_string()
        } else {
            let path: zbus::zvariant::OwnedObjectPath =
                manager.call("GetSessionByPID", &(pid,)).await?;
            let session = zbus::Proxy::new(
                &self.connection,
                "org.freedesktop.login1",
                path.as_str(),
                "org.freedesktop.login1.Session",
            )
            .await?;
            let class: String = session.get_property("Class").await?;
            let (actual, _): (String, zbus::zvariant::OwnedObjectPath) =
                session.get_property("Seat").await?;
            if class != "greeter" || actual != seat {
                bail!("caller is neither the seat compositor service nor its logind greeter");
            }
            let active: bool = session.get_property("Active").await?;
            if !active {
                bail!("greeter is not active on its seat");
            }
            path.to_string()
        };
        Ok((pid, uid, start, identity))
    }
    fn path(&self, seat: &str) -> PathBuf {
        self.directory.join(seat)
    }
    fn load(&self, seat: &str) -> Result<Option<Marker>> {
        let path = self.path(seat);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
            Ok(metadata) => {
                if !metadata.is_file()
                    || metadata.uid() != 0
                    || metadata.mode() & 0o077 != 0
                    || metadata.len() > 65536
                {
                    bail!("untrusted display recovery marker");
                }
                Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
            }
        }
    }
    fn save(&self, seat: &str, marker: &Marker) -> Result<()> {
        let temporary = self.directory.join(format!(".{seat}.new"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(marker)?)?;
        file.sync_all()?;
        fs::rename(temporary, self.path(seat))?;
        fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}
pub(super) fn process_start(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .context("invalid process stat")?
        .1
        .split_whitespace()
        .nth(19)
        .context("missing process start time")?
        .parse()
        .context("invalid process start time")
}
fn session_service(uid: u32, cgroup: &str) -> bool {
    let expected = format!(
        "/user.slice/user-{uid}.slice/user@{uid}.service/session.slice/templar-comp.service"
    );
    cgroup
        .lines()
        .any(|line| line.strip_prefix("0::") == Some(expected.as_str()))
}
fn next_marker(
    previous: Option<Marker>,
    pid: u32,
    uid: u32,
    start: u64,
    sender: &str,
    session: String,
) -> Marker {
    let mut recovery = false;
    let pending = match previous {
        Some(previous) if previous.owner_matches(pid, uid, start, sender, &session) => {
            return previous;
        }
        Some(previous) => {
            recovery = previous.recovery || !previous.pending.is_empty();
            previous.pending
        }
        None => BTreeSet::new(),
    };
    Marker {
        pid,
        uid,
        start,
        sender: sender.into(),
        lease: format!("{pid}-{start}-{sender}-{session}"),
        session,
        pending,
        recovery,
    }
}
fn notice(marker: &Marker) -> String {
    if marker.recovery {
        "Display recovery: a previous secondary-GPU attempt did not present a frame. This login uses the primary GPU only; external GPU outputs remain unavailable. Reboot clears this boot-local safety gate. This is not a diagnosis of the Oct 7 Mesa/watchdog stall.".into()
    } else {
        String::new()
    }
}
fn denied(error: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::AccessDenied(error.to_string())
}
#[zbus::interface(name = "org.templaros.Host.DisplayRecovery1")]
impl DisplayRecoveryApi {
    async fn start(
        &self,
        seat: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<(String, String)> {
        let (pid, uid, start, session) = self.caller(&header, seat).await.map_err(denied)?;
        let sender = header
            .sender()
            .context("missing bus name")
            .map_err(denied)?
            .as_str();
        let observed = self.load(seat).map_err(denied)?;
        let previous_live = if let Some(previous) = &observed {
            if previous.uid == uid
                && previous.session == session
                && process_start(previous.pid).ok() == Some(previous.start)
            {
                let bus = zbus::Proxy::new(
                    &self.connection,
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                )
                .await
                .map_err(|error| denied(error.into()))?;
                bus.call::<_, _, u32>("GetConnectionUnixProcessID", &(previous.sender.as_str(),))
                    .await
                    .ok()
                    == Some(previous.pid)
            } else {
                false
            }
        } else {
            false
        };
        let _guard = self
            .serial
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("display recovery authority poisoned".into()))?;
        let mut previous = self.load(seat).map_err(denied)?;
        let unchanged = match (&observed, &previous) {
            (None, None) => true,
            (Some(old), Some(current)) => {
                old.owner_matches(
                    current.pid,
                    current.uid,
                    current.start,
                    &current.sender,
                    &current.session,
                ) && old.lease == current.lease
            }
            _ => false,
        };
        if !unchanged {
            return Err(zbus::fdo::Error::Failed(
                "seat lease changed while authenticating Start".into(),
            ));
        }
        if previous_live
            && previous
                .as_ref()
                .is_some_and(|previous| !previous.owner_matches(pid, uid, start, sender, &session))
        {
            return Err(zbus::fdo::Error::Failed(
                "another live compositor bus lease owns this active login session".into(),
            ));
        }
        let retired = previous
            .as_mut()
            .filter(|previous| !previous.owner_matches(pid, uid, start, sender, &session))
            .map(|previous| std::mem::take(&mut previous.lease));
        let marker = next_marker(previous, pid, uid, start, sender, session);
        self.save(seat, &marker).map_err(denied)?;
        if let Some(retired) = retired {
            self.devices.retire_lease(seat, &retired);
        }
        let explanation = notice(&marker);
        Ok((marker.lease, explanation))
    }
    async fn begin_secondary(
        &self,
        seat: &str,
        lease: &str,
        gpu: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let (pid, uid, start, session) = self.caller(&header, seat).await.map_err(denied)?;
        if gpu.is_empty() || gpu.len() > 4096 {
            return Err(zbus::fdo::Error::InvalidArgs("invalid GPU identity".into()));
        }
        super::display_devices::secondary_path(seat, gpu).map_err(denied)?;
        let _guard = self
            .serial
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("display recovery authority poisoned".into()))?;
        let mut marker = self
            .load(seat)
            .map_err(denied)?
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("no recovery lease".into()))?;
        if !marker.owner_matches(
            pid,
            uid,
            start,
            header.sender().map_or("", |sender| sender.as_str()),
            &session,
        ) || marker.lease != lease
            || marker.recovery
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "secondary GPU is not admitted by this live session/seat/name lease".into(),
            ));
        }
        marker.pending.insert(gpu.into());
        self.save(seat, &marker).map_err(denied)?;
        self.devices
            .begin(seat, gpu, lease, header.primary().serial_num().get())
            .map_err(denied)
    }
    async fn first_presented(
        &self,
        seat: &str,
        lease: &str,
        gpu: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let (pid, uid, start, session) = self.caller(&header, seat).await.map_err(denied)?;
        let _guard = self
            .serial
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("display recovery authority poisoned".into()))?;
        let mut marker = self
            .load(seat)
            .map_err(denied)?
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("no recovery lease".into()))?;
        if !marker.owner_matches(
            pid,
            uid,
            start,
            header.sender().map_or("", |sender| sender.as_str()),
            &session,
        ) || marker.lease != lease
            || marker.recovery
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "presentation belongs to a stale session/seat/name recovery lease".into(),
            ));
        }
        if !self
            .devices
            .admitted(seat, gpu, lease, header.primary().serial_num().get())
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "presentation belongs to a canceled, superseded or revoked device admission".into(),
            ));
        }
        marker.pending.remove(gpu);
        // Retain the live lease; only a real presented-frame report removes a pending GPU.
        self.save(seat, &marker).map_err(denied)
    }
    async fn acquire_secondary(
        &self,
        seat: &str,
        lease: &str,
        gpu: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<super::display_devices::Descriptor> {
        let owner = self.caller(&header, seat).await.map_err(denied)?;
        let sender = header
            .sender()
            .context("missing authenticated bus name")
            .map_err(denied)?
            .as_str();
        {
            let _guard = self.serial.lock().map_err(|_| {
                zbus::fdo::Error::Failed("display recovery authority poisoned".into())
            })?;
            let marker = self
                .load(seat)
                .map_err(denied)?
                .ok_or_else(|| zbus::fdo::Error::AccessDenied("no recovery lease".into()))?;
            if !marker.owner_matches(owner.0, owner.1, owner.2, sender, &owner.3)
                || marker.lease != lease
                || marker.recovery
                || !marker.pending.contains(gpu)
            {
                return Err(zbus::fdo::Error::AccessDenied(
                    "durable live secondary marker required before driver entry".into(),
                ));
            }
        }
        let fd = self
            .devices
            .acquire(
                &self.connection,
                seat,
                gpu,
                lease,
                owner.0,
                owner.2,
                owner.1,
                sender,
                header.primary().serial_num().get(),
            )
            .await
            .map_err(denied)?;
        let current = self.caller(&header, seat).await;
        let current_marker = self.load(seat);
        if current.ok().as_ref() != Some(&owner)
            || !current_marker.ok().flatten().is_some_and(|marker| {
                marker.owner_matches(owner.0, owner.1, owner.2, sender, &owner.3)
                    && marker.lease == lease
                    && !marker.recovery
                    && marker.pending.contains(gpu)
            })
        {
            // The unpublishable descriptor returns to its retained root driver
            // lane; revocation cannot make this the bus thread's last close.
            drop(fd);
            self.devices.close(seat, gpu, lease);
            return Err(zbus::fdo::Error::AccessDenied(
                "late descriptor result lost its authenticated caller/seat/marker lifetime".into(),
            ));
        }
        Ok(fd)
    }
    async fn resume_secondary(
        &self,
        seat: &str,
        lease: &str,
        gpu: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let owner = self.caller(&header, seat).await.map_err(denied)?;
        let sender = header
            .sender()
            .context("missing authenticated bus name")
            .map_err(denied)?
            .as_str();
        {
            let marker = self
                .load(seat)
                .map_err(denied)?
                .ok_or_else(|| zbus::fdo::Error::AccessDenied("no recovery lease".into()))?;
            if !marker.owner_matches(owner.0, owner.1, owner.2, sender, &owner.3)
                || marker.lease != lease
                || marker.recovery
                || !marker.pending.contains(gpu)
            {
                return Err(zbus::fdo::Error::AccessDenied(
                    "resume requires a fresh durable marker from this active seat lease".into(),
                ));
            }
        }
        self.devices
            .rearm(
                &self.connection,
                seat,
                gpu,
                lease,
                owner.0,
                owner.2,
                sender,
                header.primary().serial_num().get(),
            )
            .await
            .map_err(denied)?;
        if self.caller(&header, seat).await.ok().as_ref() != Some(&owner) {
            self.devices.close(seat, gpu, lease);
            return Err(zbus::fdo::Error::AccessDenied(
                "late secondary rearm lost its authenticated caller".into(),
            ));
        }
        Ok(())
    }
    async fn close_secondary(
        &self,
        seat: &str,
        lease: &str,
        gpu: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        // Cleanup grants nothing. The original authenticated PID/start/name lease
        // remains able to retire its fd after seat deactivation or a package upgrade.
        if seat.is_empty()
            || seat.len() > 64
            || !seat
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(zbus::fdo::Error::InvalidArgs(
                "invalid seat identity".into(),
            ));
        }
        let (pid, uid, start) = self.caller_process(&header).await.map_err(denied)?;
        let sender = header.sender().map_or("", |sender| sender.as_str());
        let _guard = self
            .serial
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("display recovery authority poisoned".into()))?;
        let pending = self.load(seat).map_err(denied)?.is_some_and(|marker| {
            (marker.pid, marker.uid, marker.start) == (pid, uid, start)
                && marker.lease == lease
                && marker.sender == sender
                && (marker.pending.contains(gpu)
                    || super::display_devices::secondary_path(seat, gpu).is_ok())
        });
        if !pending && !self.devices.owns(seat, gpu, lease, pid, start, sender) {
            return Err(zbus::fdo::Error::AccessDenied(
                "descriptor/admission belongs to another process/name lease".into(),
            ));
        }
        self.devices
            .close_call(seat, gpu, lease, header.primary().serial_num().get())
            .map_err(denied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_identity_includes_start_time() {
        assert!(process_start(std::process::id()).unwrap() > 0);
    }
    #[test]
    fn recovery_notice_does_not_claim_a_root_cause() {
        let marker = Marker {
            pid: 1,
            uid: 1,
            start: 1,
            sender: ":1.1".into(),
            session: "greeter".into(),
            lease: "1-1".into(),
            pending: BTreeSet::from(["gpu".into()]),
            recovery: true,
        };
        assert!(notice(&marker).contains("not a diagnosis"));
        assert!(notice(&marker).contains("primary GPU only"));
    }
    #[test]
    fn repeated_start_keeps_live_marker_but_next_name_or_greeter_gates_pending_gpu() {
        let mut marker = next_marker(None, 12, 1000, 34, ":1.2", "user-session".into());
        marker.pending.insert("secondary".into());
        let repeated = next_marker(
            Some(marker.clone()),
            12,
            1000,
            34,
            ":1.2",
            "user-session".into(),
        );
        assert!(!repeated.recovery);
        assert_eq!(repeated.lease, marker.lease);
        let next = next_marker(Some(marker), 13, 120, 35, ":1.3", "greeter-session".into());
        assert!(next.recovery);
        assert!(next.pending.contains("secondary"));
    }
    #[test]
    fn healthy_greeter_handoff_and_same_uid_new_login_get_distinct_session_leases() {
        let greeter = next_marker(None, 10, 120, 20, ":1.1", "greeter-session".into());
        let user = next_marker(Some(greeter), 11, 1000, 21, ":1.2", "user-session".into());
        assert!(!user.recovery);
        assert!(!user.owner_matches(11, 1000, 21, ":1.2", "next-user-session"));
        let previous_lease = user.lease.clone();
        let next = next_marker(Some(user), 11, 1000, 21, ":1.2", "next-user-session".into());
        assert!(!next.recovery);
        assert_ne!(next.lease, previous_lease);
    }
    #[test]
    fn service_cgroup_is_exact_not_a_component_or_uid_guess() {
        assert!(session_service(
            1000,
            "0::/user.slice/user-1000.slice/user@1000.service/session.slice/templar-comp.service\n"
        ));
        assert!(!session_service(
            1001,
            "0::/user.slice/user-1000.slice/user@1000.service/session.slice/templar-comp.service\n"
        ));
        assert!(!session_service(
            1000,
            "0::/arbitrary/templar-comp.service\n"
        ));
        assert!(!session_service(
            1000,
            "0::/user.slice/user-1000.slice/user@1000.service/session.slice/templar-comp.service/child\n"
        ));
    }
}
