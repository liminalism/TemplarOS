//! Optional cgroup PSI notifications release unused accelerators. The kernel wakes this thread
//! only on pressure or shutdown; no sampling timer and no SQLite work in the pressure callback.
use std::{
    fs::OpenOptions,
    io::{self, Write as _},
    os::fd::OwnedFd,
    path::PathBuf,
    thread::JoinHandle,
};
use templar_file_index::Index;

#[derive(Debug)]
pub struct PressureMonitor {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

fn pressure_path(cgroups: &str) -> io::Result<PathBuf> {
    let path = cgroups
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "cgroup v2 membership unavailable",
            )
        })?;
    // This is kernel-owned input. Refuse traversal rather than resolve it outside cgroupfs.
    if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cgroup v2 membership",
        ));
    }
    Ok(PathBuf::from("/sys/fs/cgroup")
        .join(path.trim_start_matches('/'))
        .join("memory.pressure"))
}

impl PressureMonitor {
    pub fn spawn(index: Index) -> io::Result<Self> {
        let path = pressure_path(&std::fs::read_to_string("/proc/self/cgroup")?)?;
        let mut pressure = OpenOptions::new().read(true).write(true).open(&path)?;
        // 5% partial memory stalls in two seconds. Unprivileged PSI monitors require windows
        // that are multiples of two seconds. Closing this descriptor unregisters the trigger.
        pressure.write_all(b"some 100000 2000000\0")?;
        Self::monitor(index, pressure.into(), rustix::event::PollFlags::PRI)
    }

    fn monitor(
        index: Index,
        pressure: OwnedFd,
        event: rustix::event::PollFlags,
    ) -> io::Result<Self> {
        let stop = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)?;
        let stopping = rustix::io::dup(&stop)?;
        let thread = std::thread::Builder::new()
            .name("templar-index-psi".into())
            .spawn(move || {
                loop {
                    let mut fds = [
                        rustix::event::PollFd::new(&pressure, event),
                        rustix::event::PollFd::new(&stopping, rustix::event::PollFlags::IN),
                    ];
                    match rustix::event::poll(&mut fds, None) {
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(error) => {
                            tracing::warn!(%error, "memory pressure monitor stopped");
                            break;
                        }
                        Ok(_) => {}
                    }
                    if !fds[1].revents().is_empty() {
                        break;
                    }
                    let observed = fds[0].revents();
                    if observed.intersects(
                        rustix::event::PollFlags::ERR
                            | rustix::event::PollFlags::HUP
                            | rustix::event::PollFlags::NVAL,
                    ) {
                        tracing::warn!("memory pressure event source closed; stopping monitor");
                        break;
                    }
                    if observed.contains(event) {
                        // The IN source exists only in the owned eventfd fixture.
                        if event == rustix::event::PollFlags::IN {
                            let _ = rustix::io::read(&pressure, &mut [0u8; 8]);
                        }
                        let (roots, bytes) = index.evict_unused_search_arrays();
                        tracing::info!(
                            roots,
                            bytes,
                            "released unused search shards for memory pressure"
                        );
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for PressureMonitor {
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
    fn cgroup_paths_use_unified_membership_and_refuse_traversal() {
        assert_eq!(
            pressure_path("0::/user.slice/owned.scope\n").unwrap(),
            PathBuf::from("/sys/fs/cgroup/user.slice/owned.scope/memory.pressure")
        );
        assert_eq!(
            pressure_path("0::/\n").unwrap(),
            PathBuf::from("/sys/fs/cgroup/memory.pressure")
        );
        assert!(pressure_path("1:memory:/legacy\n").is_err());
        assert!(pressure_path("0::/../../proc\n").is_err());
    }
    #[test]
    fn an_owned_pressure_event_releases_arrays_and_shutdown_wakes_the_idle_monitor() {
        let directory = tempfile::tempdir().unwrap();
        let tree = directory.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("hit"), b"h").unwrap();
        let index = Index::open(
            &directory.path().join("index.sqlite3"),
            templar_file_index::IndexConfig {
                search_arrays: true,
                ..Default::default()
            },
        )
        .unwrap();
        let root = index
            .add_root(
                &templar_file_index::NativePath::from_path(&tree),
                templar_file_index::RootPolicy::default_home(),
            )
            .unwrap();
        index
            .scan_root(root.root, templar_file_index::ScanOptions::default())
            .unwrap();
        index.prepare_search_arrays().unwrap();
        let trigger = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC).unwrap();
        let monitor = PressureMonitor::monitor(
            index.clone(),
            rustix::io::dup(&trigger).unwrap(),
            rustix::event::PollFlags::IN,
        )
        .unwrap();
        rustix::io::write(&trigger, &1u64.to_ne_bytes()).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while index.search_array_stats().unwrap().bytes != 0 {
            assert!(std::time::Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(index.search_array_stats().unwrap().retained_bytes, 0);
        let start = std::time::Instant::now();
        drop(monitor);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
