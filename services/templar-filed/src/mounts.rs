//! Mount-table observation is event driven. No polling timer and no filesystem acquisition on
//! the async executor; the watcher owns repair and rearming after these wakeups.
use crate::observer::{Command, WatcherControl};
use std::{
    fs::File,
    io::{self, Read as _, Seek as _},
    os::fd::OwnedFd,
    thread::JoinHandle,
};
use templar_file_index::{
    Index, NativePath,
    volume::{MountRecord, parse_mountinfo},
};

#[derive(Debug)]
pub struct MountMonitor {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

fn read_table(file: &mut File) -> io::Result<Vec<MountRecord>> {
    file.rewind()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let table = parse_mountinfo(&bytes);
    if table.is_empty() {
        return Err(io::Error::other("mountinfo contained no valid records"));
    }
    Ok(table)
}

fn observation_failed(index: &Index, detail: &str) {
    index.clear_mount_locations();
    if let Ok(roots) = index.roots() {
        for root in roots {
            let _ = index.mark_offline_reason(
                root.root,
                &templar_file_index::OfflineReason::PresenceUnknown {
                    detail: detail.to_owned(),
                },
            );
        }
    }
}

/// Changed mount points include removals: an unmounted boundary must be re-enumerated too.
#[must_use]
pub fn changed_mount_points(before: &[MountRecord], after: &[MountRecord]) -> Vec<NativePath> {
    let mut changed = Vec::new();
    for record in before.iter().chain(after) {
        if (!before.contains(record) || !after.contains(record))
            && !changed.contains(&record.mount_point)
        {
            changed.push(record.mount_point.clone());
        }
    }
    changed
}

impl MountMonitor {
    pub fn spawn(index: Index, control: WatcherControl) -> io::Result<Self> {
        let mut file = File::open("/proc/self/mountinfo")?;
        let mut before = read_table(&mut file)?;
        let stop = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)?;
        let stopping = rustix::io::dup(&stop)?;
        let thread=std::thread::Builder::new().name("templar-index-mounts".into()).spawn(move || {
            if let Err(error) = index.observe_mount_locations(&before) {
                index.clear_mount_locations();
                tracing::warn!(%error, "initial volume locations could not be observed");
            }
            loop {
                let mut fds=[rustix::event::PollFd::new(&file, rustix::event::PollFlags::PRI | rustix::event::PollFlags::ERR),
                    rustix::event::PollFd::new(&stopping, rustix::event::PollFlags::IN)];
                match rustix::event::poll(&mut fds,None) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => { tracing::warn!(%error,"mount observation stopped"); observation_failed(&index, &error.to_string()); break; },
                    Ok(_) => {},
                }
                if !fds[1].revents().is_empty() { break; }
                let after=match read_table(&mut file) {
                    Ok(table) => table,
                    Err(error) => { tracing::warn!(%error,"mountinfo could not be re-read; refusing a busy retry loop"); observation_failed(&index, &error.to_string()); break; },
                };
                let points=changed_mount_points(&before,&after);
                if let Err(error) = index.observe_mount_locations(&after) {
                    index.clear_mount_locations();
                    tracing::warn!(%error, "volume locations could not be observed");
                }
                if let Ok(roots)=index.roots() {
                    for root in roots {
                        match templar_file_index::volume::presence(&index,root.root) {
                            Ok(presence) => control.send(Command::VolumeChange(root.root,presence)),
                            Err(error) => {
                                let _=index.mark_offline_reason(root.root,&templar_file_index::OfflineReason::PresenceUnknown { detail:error.to_string() });
                            },
                        }
                        for point in &points {
                            if point != &root.path && point.starts_with(&root.path) {
                                control.send(Command::MountBoundary(root.root,point.clone()));
                            }
                        }
                    }
                }
                before=after;
            }
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for MountMonitor {
    fn drop(&mut self) {
        let _ = rustix::io::write(&self.stop, &1u64.to_ne_bytes());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mount_over_and_unmount_both_schedule_the_shadowed_native_subtree() {
        let base = parse_mountinfo(b"1 0 8:1 / / rw - ext4 /dev/a rw\n");
        let mounted=parse_mountinfo(b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 8:2 / /project/build\\040tree rw - ext4 /dev/b rw\n");
        let expected = vec![NativePath::from_bytes(b"/project/build tree".to_vec())];
        assert_eq!(changed_mount_points(&base, &mounted), expected);
        assert_eq!(changed_mount_points(&mounted, &base), expected);
        assert!(changed_mount_points(&mounted, &mounted).is_empty());
    }
}
