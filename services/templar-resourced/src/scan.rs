//! Discover application instances from the cgroup tree alone.
//!
//! Because launch units are self-describing, the governor never needs `appd` to know *which*
//! application a cgroup belongs to. `appd` only enriches (resource class, checkpoint support,
//! foreground state comes from the compositor).

use crate::cgroup::{CGROUP_ROOT, Cgroup};
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};
use templar_types::{AppId, InstanceId, unit};

/// One discovered instance cgroup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredInstance {
    pub app_id: AppId,
    pub instance_id: InstanceId,
    pub cgroup: Cgroup,
}

/// The `app.slice` of the current user's manager: application launches only.
pub fn user_app_slice(uid: u32) -> PathBuf {
    Path::new(CGROUP_ROOT).join(format!(
        "user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
    ))
}

/// The `session.slice` of the current user's manager: session-critical infrastructure
/// (compositor, shell, audio, `templar-resourced` itself — see [`crate::protect`]), never
/// application instances. Same self-describing-path derivation as `user_app_slice`: no D-Bus
/// round-trip, no dependency on systemd having answered a query (ADR-0002).
pub fn user_session_slice(uid: u32) -> PathBuf {
    Path::new(CGROUP_ROOT).join(format!(
        "user.slice/user-{uid}.slice/user@{uid}.service/session.slice"
    ))
}

/// Walk `slice` (non-recursively beyond unit depth) and return every TemplarOS-launched instance.
pub fn discover(slice: &Path) -> Result<Vec<DiscoveredInstance>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(slice) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error.into()),
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some((app_id, instance_id)) = unit::parse_unit_name(name) {
            found.push(DiscoveredInstance {
                app_id,
                instance_id,
                cgroup: Cgroup::from_directory(path),
            });
        }
    }

    found.sort_by_key(|instance| instance.instance_id);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_only_self_describing_units() {
        let dir =
            std::env::temp_dir().join(format!("templar-resourced-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let id = InstanceId::new();
        fs::create_dir_all(dir.join(unit::unit_name(&app, id))).unwrap();
        fs::create_dir_all(dir.join("app-flatpak-org.example-1234.scope")).unwrap();
        fs::write(dir.join("cgroup.procs"), "").unwrap();

        let found = discover(&dir).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].app_id, app);
        assert_eq!(found[0].instance_id, id);
        assert!(discover(&dir.join("missing")).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_and_session_slices_are_distinct_siblings() {
        let app = user_app_slice(1000);
        let session = user_session_slice(1000);
        assert_ne!(app, session);
        assert_eq!(app.parent(), session.parent());
        assert!(app.ends_with("app.slice"));
        assert!(session.ends_with("session.slice"));
    }
}
