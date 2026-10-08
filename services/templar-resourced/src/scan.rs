//! Discover application, explicit work and Flatpak scopes from the cgroup tree alone.
//! A Flatpak unit name is only a scope claim; it never grants canonical application permissions.

use crate::cgroup::{CGROUP_ROOT, Cgroup};
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};
use templar_types::{ResourceId, ResourceIdentity, unit};
use std::os::unix::fs::MetadataExt;

/// One discovered instance cgroup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredInstance {
    pub identity: ResourceIdentity,
    /// Historical wire field name; this UUID keys one managed resource, not necessarily an app.
    pub instance_id: ResourceId,
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

/// Walk one owned slice at unit depth; never include session-critical units or nested parents.
pub fn discover(slice: &Path) -> Result<Vec<DiscoveredInstance>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(slice) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() { continue; }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let (instance_id, identity) = if let Some((app_id, instance_id)) = unit::parse_unit_name(name) {
            (instance_id.into(), ResourceIdentity::Application { app_id })
        } else if let Some((spawned_by, workload_id, class)) = unit::parse_workload_unit_name(name) {
            (workload_id.into(), ResourceIdentity::Workload { workload_id, spawned_by, class })
        } else if let Some(rest) = name.strip_prefix("app-flatpak-").and_then(|rest| rest.strip_suffix(".scope")) {
            let Some((claim, suffix)) = rest.rsplit_once('-') else { continue; };
            if claim.is_empty() || suffix.is_empty() { continue; }
            (ResourceId::external_unit(name, metadata.ino()), ResourceIdentity::Flatpak { claimed_app_id: claim.to_owned() })
        } else { continue; };
        found.push(DiscoveredInstance { identity, instance_id, cgroup: Cgroup::from_directory(path) });
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
        let app: templar_types::AppId = "org.templaros.Files".parse().unwrap();
        let id = templar_types::InstanceId::new();
        fs::create_dir_all(dir.join(unit::unit_name(&app, id))).unwrap();
        fs::create_dir_all(dir.join("app-flatpak-org.example-1234.scope")).unwrap();
        fs::write(dir.join("cgroup.procs"), "").unwrap();

        let found = discover(&dir).unwrap();
        assert_eq!(found.iter().filter(|row| matches!(&row.identity,
            ResourceIdentity::Flatpak { claimed_app_id } if claimed_app_id == "org.example")).count(), 1);
        let application = found.iter().find(|row| row.identity.application() == Some(&app)).unwrap();
        assert_eq!(application.instance_id, ResourceId::from(id));
        assert!(discover(&dir.join("missing")).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

}
