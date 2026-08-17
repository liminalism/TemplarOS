//! Thin cgroup-v2 access wrapper.
//!
//! In production, `resourced` runs inside the properly delegated user cgroup subtree (its own
//! units are owned by the user), or changes user-unit properties through systemd's D-Bus API.
//! Control files are never made writable by arbitrary applications.

use anyhow::{Context, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// Root of the unified hierarchy.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// One cgroup directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cgroup {
    directory: PathBuf,
}

impl Cgroup {
    /// From systemd's `ControlGroup` property (`/user.slice/.../foo.service`).
    pub fn from_systemd_control_group(control_group: &Path) -> Self {
        let relative = control_group.strip_prefix("/").unwrap_or(control_group);
        Self {
            directory: Path::new(CGROUP_ROOT).join(relative),
        }
    }

    /// From an absolute directory under the cgroup filesystem.
    pub fn from_directory(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The systemd-style control-group path (`/user.slice/...`).
    pub fn control_group_path(&self) -> PathBuf {
        self.directory.strip_prefix(CGROUP_ROOT).map_or_else(
            |_| self.directory.clone(),
            |relative| Path::new("/").join(relative),
        )
    }

    pub fn memory_current(&self) -> Result<u64> {
        self.read_u64("memory.current")
    }

    pub fn memory_swap_current(&self) -> Result<u64> {
        self.read_u64("memory.swap.current")
    }

    pub fn memory_events(&self) -> Result<String> {
        self.read_text("memory.events.local")
    }

    pub fn memory_pressure(&self) -> Result<String> {
        self.read_text("memory.pressure")
    }

    pub fn io_pressure(&self) -> Result<String> {
        self.read_text("io.pressure")
    }

    pub fn set_memory_low(&self, bytes: u64) -> Result<()> {
        self.write_control("memory.low", &bytes.to_string())
    }

    pub fn set_memory_high(&self, bytes: Option<u64>) -> Result<()> {
        self.write_control("memory.high", &max_or(bytes))
    }

    pub fn set_memory_max(&self, bytes: Option<u64>) -> Result<()> {
        self.write_control("memory.max", &max_or(bytes))
    }

    /// `memory.oom.group=1` makes the workload indivisible for an in-cgroup OOM decision.
    pub fn set_group_oom(&self, enabled: bool) -> Result<()> {
        self.write_control("memory.oom.group", if enabled { "1" } else { "0" })
    }

    /// Proactive reclaim of `bytes` from this group (`memory.reclaim`).
    pub fn request_reclaim(&self, bytes: u64) -> Result<()> {
        self.write_control("memory.reclaim", &bytes.to_string())
    }

    /// `cgroup.kill` terminates the entire subtree with SIGKILL and handles concurrent forks —
    /// far more reliable than enumerating PIDs.
    pub fn kill_all(&self) -> Result<()> {
        self.write_control("cgroup.kill", "1")
    }

    fn read_text(&self, name: &str) -> Result<String> {
        let path = self.directory.join(name);
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }

    fn read_u64(&self, name: &str) -> Result<u64> {
        let text = self.read_text(name)?;
        text.trim()
            .parse()
            .with_context(|| format!("parsing {name}: {text:?}"))
    }

    fn write_control(&self, name: &str, value: &str) -> Result<()> {
        let path = self.directory.join(name);
        // Important for memory.high and memory.max: never make resourced synchronously reclaim
        // the target application's memory.
        let mut file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(value.as_bytes())
            .with_context(|| format!("writing {value:?} to {}", path.display()))
    }
}

fn max_or(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "max".to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_group_paths_round_trip() {
        let cg = Cgroup::from_systemd_control_group(Path::new(
            "/user.slice/user-1000.slice/app.slice/x.service",
        ));
        assert_eq!(
            cg.directory(),
            Path::new("/sys/fs/cgroup/user.slice/user-1000.slice/app.slice/x.service")
        );
        assert_eq!(
            cg.control_group_path(),
            PathBuf::from("/user.slice/user-1000.slice/app.slice/x.service")
        );
    }
}
