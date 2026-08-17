//! Discover application instances from the cgroup tree alone.
//!
//! Because launch units are self-describing, the governor never needs `appd` to know *which*
//! application a cgroup belongs to. `appd` only enriches (resource class, checkpoint support,
//! foreground state comes from the compositor).

use crate::cgroup::{CGROUP_ROOT, Cgroup};
use anyhow::Result;
use lege_types::{AppId, InstanceId, unit};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// One discovered instance cgroup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredInstance {
    pub app_id: AppId,
    pub instance_id: InstanceId,
    pub cgroup: Cgroup,
}

/// The `app.slice` of the current user's manager.
pub fn user_app_slice(uid: u32) -> PathBuf {
    Path::new(CGROUP_ROOT).join(format!(
        "user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
    ))
}

/// Walk `slice` (non-recursively beyond unit depth) and return every LegeOS-launched instance.
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
        let dir = std::env::temp_dir().join(format!("lege-resourced-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let app: AppId = "org.legeos.Files".parse().unwrap();
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
}
