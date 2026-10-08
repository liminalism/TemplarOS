//! HomeGarden presentation evidence, deliberately NOT AppIdentity or provider authority.
//! A cgroup-escaped terminal child is associated only while every same-UID ancestry edge,
//! process lifetime and the terminal's current exact registered cgroup remain valid.
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    io::Read as _,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};
use templar_appd::{Registry, identity::InstanceRecord};
use templar_types::{AppCatalog as _, AppId, WindowClaims};

const MAX_ANCESTORS: usize = 32;
const MAX_PROCESSES: usize = 256;
const MAX_TASKS: usize = 32;
const MAX_INPUT: u64 = 32_768;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    parent: u32,
    start: u64,
    uid: u32,
}

fn text(path: &Path) -> Result<String> {
    let mut value = String::new();
    fs::File::open(path)?
        .take(MAX_INPUT + 1)
        .read_to_string(&mut value)?;
    ensure!(
        value.len() as u64 <= MAX_INPUT,
        "process evidence exceeds bound"
    );
    Ok(value)
}

fn snapshot(root: &Path, pid: u32) -> Result<Snapshot> {
    let base = root.join(pid.to_string());
    let stat = text(&base.join("stat"))?;
    // As in the catalogue, the final ')' closes comm, which may itself contain ')'.
    let (_, fields) = stat.rsplit_once(')').context("malformed process stat")?;
    let mut fields = fields.split_whitespace();
    let state = fields.next().context("missing process state")?;
    ensure!(
        !matches!(state, "Z" | "X" | "x"),
        "process is no longer live"
    );
    let parent = fields.next().context("missing process parent")?.parse()?;
    let start = fields
        .nth(17)
        .context("missing process start time")?
        .parse()?;
    let status = text(&base.join("status"))?;
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .context("missing process UID")?;
    let mut ids = ids.split_whitespace();
    let uid: u32 = ids.next().context("missing real UID")?.parse()?;
    for _ in 0..3 {
        ensure!(
            ids.next().context("missing process UID")?.parse::<u32>()? == uid,
            "process has differing credentials"
        );
    }
    ensure!(ids.next().is_none(), "unexpected process UID fields");
    Ok(Snapshot { parent, start, uid })
}

/// Also accepts the bus's ProcessFD: a queried numeric PID is not the bus peer's lifetime.
#[allow(unsafe_code)]
pub(super) fn require_live(fd: impl AsFd) -> Result<()> {
    let mut poll = libc::pollfd {
        fd: fd.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized descriptor is borrowed for this synchronous, zero-timeout poll.
    let result = unsafe { libc::poll(&mut poll, 1, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(result == 0 && poll.revents == 0, "process lifetime ended");
    Ok(())
}

struct Pin {
    pid: u32,
    observed: Snapshot,
    fd: OwnedFd,
}
impl Pin {
    #[allow(unsafe_code)]
    fn open(pid: u32, uid: u32) -> Result<Self> {
        ensure!(pid > 1 && pid <= i32::MAX as u32, "invalid process PID");
        // SAFETY: pidfd_open takes only a numeric PID and zero flags; successful fd is owned.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: the successful syscall returned a new descriptor, with one owner here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        require_live(&fd)?;
        let observed = snapshot(Path::new("/proc"), pid)?;
        ensure!(observed.uid == uid, "process is not the session user");
        require_live(&fd)?;
        Ok(Self { pid, observed, fd })
    }
    fn current(&self) -> bool {
        require_live(&self.fd).is_ok()
            && snapshot(Path::new("/proc"), self.pid).is_ok_and(|now| now == self.observed)
            && require_live(&self.fd).is_ok()
    }
}

pub(super) struct Association {
    pub record: InstanceRecord,
    chain: Vec<Pin>,
}
impl Association {
    pub fn pid(&self) -> u32 {
        self.chain[0].pid
    }
    pub fn is_descendant(&self) -> bool {
        self.chain.len() > 1
    }
    pub fn current(&self, registry: &Registry) -> bool {
        self.chain.iter().all(Pin::current)
            && self.chain.last().is_some_and(|anchor| {
                registered(registry, anchor.pid).is_some_and(|now| same_record(&now, &self.record))
            })
            && self.chain.iter().all(Pin::current)
    }
}
fn same_record(a: &InstanceRecord, b: &InstanceRecord) -> bool {
    a.id == b.id
        && a.app_id == b.app_id
        && a.control_group == b.control_group
        && a.systemd_unit == b.systemd_unit
}
fn registered(registry: &Registry, pid: u32) -> Option<InstanceRecord> {
    let identity = registry
        .resolve_window(pid, &WindowClaims::default())
        .ok()?;
    if !identity.confidence.is_verified() {
        return None;
    }
    let record = registry.instance(identity.instance_id?)?;
    (record.has_managed_identity() && identity.app_id.as_ref() == Some(&record.app_id))
        .then_some(record)
}
pub(super) fn terminal(
    registry: &Registry,
    record: &InstanceRecord,
    selected: Option<&AppId>,
) -> bool {
    selected == Some(&record.app_id)
        || registry
            .manifest(&record.app_id)
            .is_some_and(|manifest| manifest.categories.contains("TerminalEmulator"))
}

/// No name, scope name, claimed instance, cwd or historical parent ever grants a link.
pub(super) fn associate(
    registry: &Registry,
    pid: u32,
    uid: u32,
    selected: Option<&AppId>,
) -> Result<Association> {
    let mut next = pid;
    let mut chain: Vec<Pin> = Vec::new();
    for _ in 0..MAX_ANCESTORS {
        ensure!(
            !chain.iter().any(|pin| pin.pid == next),
            "cyclic process ancestry"
        );
        let pin = Pin::open(next, uid)?;
        ensure!(
            chain
                .last()
                .is_none_or(|child| pin.observed.start <= child.observed.start),
            "parent lifetime is newer than its child"
        );
        next = pin.observed.parent;
        let record = registered(registry, pin.pid);
        chain.push(pin);
        if let Some(record) = record {
            // A different managed app is a boundary, not a bridge to an older terminal ancestor.
            ensure!(
                terminal(registry, &record, selected),
                "registered ancestor is not a terminal"
            );
            let association = Association { record, chain };
            ensure!(
                association.current(registry),
                "process ancestry changed during association"
            );
            return Ok(association);
        }
        ensure!(next > 1, "no current registered terminal ancestor");
    }
    anyhow::bail!("terminal ancestry exceeds bound")
}

/// One bounded descendant walk per collection edge, using kernel task/children lists rather
/// than scanning /proc or scope names. Nothing is cached and no poller/timer is installed.
pub(super) fn discover(
    registry: &Registry,
    instances: &[super::ProcessEvidence],
    uid: u32,
    selected: Option<&AppId>,
) -> Vec<Association> {
    let mut result = Vec::new();
    let mut visited = BTreeSet::new();
    for (record, roots, _) in instances {
        if !terminal(registry, record, selected) {
            continue;
        }
        let mut pending: Vec<_> = roots.iter().copied().take(MAX_PROCESSES).collect();
        while let Some(pid) = pending.pop() {
            if visited.len() >= MAX_PROCESSES {
                return result;
            }
            if !visited.insert(pid) {
                continue;
            }
            let Ok(association) = associate(registry, pid, uid, selected) else {
                continue;
            };
            if !same_record(record, &association.record) {
                continue;
            }
            if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
                for task in tasks.take(MAX_TASKS).flatten() {
                    if let Ok(children) = text(&task.path().join("children")) {
                        for child in children
                            .split_whitespace()
                            .filter_map(|pid| pid.parse().ok())
                        {
                            if pending.len() + visited.len() >= MAX_PROCESSES {
                                break;
                            }
                            pending.push(child);
                        }
                    }
                }
            }
            if association.current(registry) {
                result.push(association);
            }
        }
    }
    result
}

pub(super) fn session_uid() -> Result<u32> {
    Ok(snapshot(Path::new("/proc"), std::process::id())?.uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn actual_kernel_pin_refuses_uid_reuse_reparenting_and_exit() {
        let mut child = Child(
            Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let uid = session_uid().unwrap();
        let pid = child.0.id();
        let mut pin = Pin::open(pid, uid).unwrap();
        assert_eq!(pin.observed.parent, std::process::id());
        assert!(pin.current());
        assert!(Pin::open(pid, uid.wrapping_add(1)).is_err());
        let real = pin.observed;
        // Even while the numeric PID is alive, cached lifetime or parent claims are refused.
        pin.observed.start = real.start.wrapping_add(1);
        assert!(!pin.current());
        pin.observed = real;
        pin.observed.parent = real.parent.wrapping_add(1);
        assert!(!pin.current());
        pin.observed = real;
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!pin.current());
        assert!(require_live(&pin.fd).is_err());
        assert!(Pin::open(pid, uid).is_err());
    }

    #[test]
    fn actual_unmanaged_child_and_forged_registered_scope_get_no_display_link() {
        let child = Child(
            Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let registry = Registry::default();
        let app: AppId = "org.example.Terminal".parse().unwrap();
        let instance = templar_types::InstanceId::new();
        let unit = templar_types::unit::unit_name(&app, instance);
        registry.register_instance(InstanceRecord {
            id: instance,
            app_id: app.clone(),
            systemd_unit: unit.clone(),
            control_group: format!(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}"
            )
            .into(),
        });
        let uid = session_uid().unwrap();
        assert!(associate(&registry, child.0.id(), uid, Some(&app)).is_err());
        let canonical = registry
            .resolve_window(child.0.id(), &WindowClaims::default())
            .unwrap();
        assert!(!canonical.confidence.is_verified());
        assert!(canonical.instance_id.is_none());
        assert!(canonical.app_id.is_none());
    }

    #[test]
    fn snapshots_require_live_same_uid_and_track_the_current_kernel_parent() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("788");
        fs::create_dir(&base).unwrap();
        let mut fields = vec!["0".to_owned(); 20];
        fields[0] = "S".into();
        fields[1] = "682".into();
        fields[19] = "35841".into();
        fs::write(
            base.join("stat"),
            format!("788 (bash (with) spaces)) {}", fields.join(" ")),
        )
        .unwrap();
        fs::write(base.join("status"), "Uid:\t1000\t1000\t1000\t1000\n").unwrap();
        let observed = snapshot(temp.path(), 788).unwrap();
        assert_eq!(
            observed,
            Snapshot {
                parent: 682,
                start: 35841,
                uid: 1000
            }
        );
        fields[1] = "1".into();
        fs::write(
            base.join("stat"),
            format!("788 (bash) {}", fields.join(" ")),
        )
        .unwrap();
        assert_ne!(snapshot(temp.path(), 788).unwrap(), observed);
        fields[1] = "682".into();
        fields[19] = "35842".into();
        fs::write(
            base.join("stat"),
            format!("788 (bash) {}", fields.join(" ")),
        )
        .unwrap();
        assert_ne!(snapshot(temp.path(), 788).unwrap(), observed);
        fields[0] = "Z".into();
        fs::write(
            base.join("stat"),
            format!("788 (bash) {}", fields.join(" ")),
        )
        .unwrap();
        assert!(snapshot(temp.path(), 788).is_err());
        fields[0] = "S".into();
        fs::write(
            base.join("stat"),
            format!("788 (bash) {}", fields.join(" ")),
        )
        .unwrap();
        fs::write(base.join("status"), "Uid:\t1000\t0\t1000\t1000\n").unwrap();
        assert!(snapshot(temp.path(), 788).is_err());
    }
}
