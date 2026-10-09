//! Self-describing systemd unit names for application launches.
//!
//! `app-templaros-<AppId>@<InstanceId>.service` follows systemd's desktop application unit naming
//! convention *and* lets any observer (notably `templar-resourced`) derive canonical identity from
//! the cgroup path alone, without a D-Bus round-trip.

use crate::{AppId, InstanceId};
use std::path::Path;

/// Prefix shared by every TemplarOS-launched application unit.
pub const UNIT_PREFIX: &str = "app-templaros-";

pub const WORKLOAD_UNIT_PREFIX: &str = "workload-templaros-";

/// A separate sibling scope, never a child of the terminal's application cgroup.
pub fn workload_unit_name(
    parent: Option<InstanceId>,
    workload: crate::WorkloadId,
    class: crate::WorkloadClass,
) -> String {
    match parent {
        Some(parent) => format!(
            "{WORKLOAD_UNIT_PREFIX}{}-{}-{}.scope",
            class.as_str(),
            parent.uuid().simple(),
            workload.compact()
        ),
        None => format!(
            "{WORKLOAD_UNIT_PREFIX}{}-unknown-{}.scope",
            class.as_str(),
            workload.compact()
        ),
    }
}

pub fn parse_workload_unit_name(
    name: &str,
) -> Option<(Option<InstanceId>, crate::WorkloadId, crate::WorkloadClass)> {
    let rest = name
        .strip_prefix(WORKLOAD_UNIT_PREFIX)?
        .strip_suffix(".scope")?;
    let (rest, class) = if let Some(rest) = rest.strip_prefix("interactive-") {
        (rest, crate::WorkloadClass::Interactive)
    } else {
        (
            rest.strip_prefix("background-compute-")?,
            crate::WorkloadClass::BackgroundCompute,
        )
    };
    let (parent, workload) = rest.rsplit_once('-')?;
    if workload.len() != 32 || !workload.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let parent = if parent == "unknown" {
        None
    } else {
        if parent.len() != 32 || !parent.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(InstanceId::parse(parent)?)
    };
    Some((parent, crate::WorkloadId::parse(workload)?, class))
}

pub fn parse_workload_cgroup_path(
    path: &Path,
) -> Option<(Option<InstanceId>, crate::WorkloadId, crate::WorkloadClass)> {
    path.components()
        .rev()
        .find_map(|component| parse_workload_unit_name(component.as_os_str().to_str()?))
}

/// The user target that *is* the graphical session: compositor, shell and daemons are all
/// `PartOf=` it, and `templar-session` blocks on it for the session's whole life.
///
/// Stopping it is how the session ends. `templar-session` waits on this target, so when it goes down
/// that process returns, runs its cleanup and exits — and the display manager, which is waiting on
/// *that* process, shows its greeter again. Ending the session any other way (asking logind to
/// terminate the whole scope, say) kills the display manager's own helper along with everything
/// else, which it reports as a crash rather than a logout, and then nothing brings the greeter
/// back: the user is left looking at a blank VT.
pub const SESSION_TARGET: &str = "templaros-session.target";

/// Build the transient unit name for one launch.
pub fn unit_name(app: &AppId, instance: InstanceId) -> String {
    format!(
        "{UNIT_PREFIX}{}@{}.service",
        escape(app.as_str()),
        instance.compact()
    )
}

/// Recover `(AppId, InstanceId)` from a unit name produced by [`unit_name`].
pub fn parse_unit_name(unit: &str) -> Option<(AppId, InstanceId)> {
    let rest = unit.strip_prefix(UNIT_PREFIX)?.strip_suffix(".service")?;
    let (app, instance) = rest.rsplit_once('@')?;
    let app: AppId = unescape(app).parse().ok()?;
    let instance = InstanceId::parse(instance)?;
    Some((app, instance))
}

/// Recover identity from a cgroup path such as
/// `/user.slice/user-1000.slice/user@1000.service/app.slice/app-templaros-org.templaros.Files@<id>.service`.
pub fn parse_cgroup_path(path: &Path) -> Option<(AppId, InstanceId)> {
    path.components()
        .rev()
        .find_map(|component| parse_unit_name(component.as_os_str().to_str()?))
}

/// systemd unit names may not contain `@` (instance separator) or `/`; `AppId` only contains
/// `[A-Za-z0-9._-]`, so no escaping is required today. Kept as a seam for future alphabets.
fn escape(app: &str) -> &str {
    app
}

fn unescape(app: &str) -> &str {
    app
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn unit_names_round_trip() {
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let instance = InstanceId::new();
        let unit = unit_name(&app, instance);
        assert_eq!(parse_unit_name(&unit), Some((app, instance)));
    }

    #[test]
    fn cgroup_paths_yield_identity() {
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let instance = InstanceId::new();
        let path = PathBuf::from(format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/{}/child",
            unit_name(&app, instance)
        ));
        assert_eq!(parse_cgroup_path(&path), Some((app, instance)));
        assert_eq!(
            parse_cgroup_path(Path::new("/system.slice/ssh.service")),
            None
        );
    }

    #[test]
    fn workload_scope_identity_keeps_parent_separate() {
        let parent = InstanceId::parse("11111111111111111111111111111111").unwrap();
        let workload = crate::WorkloadId::parse("22222222222222222222222222222222").unwrap();
        let class = crate::WorkloadClass::BackgroundCompute;
        let scope = workload_unit_name(Some(parent), workload, class);
        assert_eq!(
            parse_workload_unit_name(&scope),
            Some((Some(parent), workload, class))
        );
        assert_eq!(parse_unit_name(&scope), None);
        let path = PathBuf::from(format!("/user.slice/background.slice/{scope}/workers"));
        assert_eq!(
            parse_workload_cgroup_path(&path),
            Some((Some(parent), workload, class))
        );
        assert_eq!(parse_cgroup_path(&path), None);
        let unknown = workload_unit_name(None, workload, crate::WorkloadClass::Interactive);
        assert_eq!(
            parse_workload_unit_name(&unknown),
            Some((None, workload, crate::WorkloadClass::Interactive))
        );
    }

    #[test]
    fn malformed_workload_scope_never_becomes_identity() {
        for name in [
            "workload-templaros-unknown-22222222222222222222222222222222.service",
            "workload-templaros-unknown-22222222222222222222222222222222.scope.extra",
            "workload-templaros-unknown-2.scope",
            "workload-templaros-forged-22222222222222222222222222222222.scope",
            "app-flatpak-org.example.App-123.scope",
        ] {
            assert_eq!(parse_workload_unit_name(name), None);
        }
    }
}
