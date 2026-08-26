//! Self-describing systemd unit names for application launches.
//!
//! `app-legeos-<AppId>@<InstanceId>.service` follows systemd's desktop application unit naming
//! convention *and* lets any observer (notably `lege-resourced`) derive canonical identity from
//! the cgroup path alone, without a D-Bus round-trip.

use crate::{AppId, InstanceId};
use std::path::Path;

/// Prefix shared by every LegeOS-launched application unit.
pub const UNIT_PREFIX: &str = "app-legeos-";

/// The user target that *is* the graphical session: compositor, shell and daemons are all
/// `PartOf=` it, and `lege-session` blocks on it for the session's whole life.
///
/// Stopping it is how the session ends. `lege-session` waits on this target, so when it goes down
/// that process returns, runs its cleanup and exits — and the display manager, which is waiting on
/// *that* process, shows its greeter again. Ending the session any other way (asking logind to
/// terminate the whole scope, say) kills the display manager's own helper along with everything
/// else, which it reports as a crash rather than a logout, and then nothing brings the greeter
/// back: the user is left looking at a blank VT.
pub const SESSION_TARGET: &str = "legeos-session.target";

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
/// `/user.slice/user-1000.slice/user@1000.service/app.slice/app-legeos-org.legeos.Files@<id>.service`.
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
        let app: AppId = "org.legeos.Files".parse().unwrap();
        let instance = InstanceId::new();
        let unit = unit_name(&app, instance);
        assert!(unit.starts_with("app-legeos-org.legeos.Files@"));
        assert_eq!(parse_unit_name(&unit), Some((app, instance)));
    }

    #[test]
    fn cgroup_paths_yield_identity() {
        let app: AppId = "org.legeos.Files".parse().unwrap();
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
}
