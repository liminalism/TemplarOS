//! The real [`Systemctl`], [`BusProbe`], [`Clock`] and [`Stamps`] the activation engine drives.
//!
//! Thin on purpose: every judgement lives in `activate.rs`, and this file only knows how to ask
//! systemd and the bus a question. Both are asked through the same [`CommandRunner`] seam the
//! archive uses, so a deployment's whole argv is visible in one place and testable in another.

use super::{
    activate::{BusProbe, Clock, Stamps, Systemctl},
    cmd::CommandRunner,
};
use anyhow::{Context, Result, bail};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use templar_build::{RunningStamp, components::Scope};

/// `systemctl`, in whichever manager a component's unit lives in.
#[derive(Debug)]
pub struct RealSystemctl {
    pub runner: Arc<dyn CommandRunner>,
    /// Working directory for the invocations — irrelevant to systemctl, but the runner asks.
    pub dir: PathBuf,
    /// The desktop user whose `--user` manager to reach, when this process is root.
    ///
    /// `templar dev deploy` requires root (it needs it for apt), but a `--user` unit lives in the
    /// *desktop user's* manager, not root's own — root has no session bus of its own, so a plain
    /// `systemctl --user` under `sudo` fails with "DBUS_SESSION_BUS_ADDRESS and XDG_RUNTIME_DIR
    /// not defined". `--machine=<user>@.host` is systemd's supported way to cross that gap.
    pub target_user: Option<String>,
}

impl RealSystemctl {
    fn scope_flag(scope: Scope) -> &'static str {
        match scope {
            // A component with no unit never reaches here; `--user` is the harmless default.
            Scope::System => "--system",
            Scope::User | Scope::None => "--user",
        }
    }

    fn systemctl(&self, scope: Scope, args: &[&str]) -> Result<std::process::Output> {
        let machine = machine_arg(scope, self.target_user.as_deref());
        let mut all = vec![Self::scope_flag(scope), "--no-pager"];
        if let Some(machine) = &machine {
            all.push(machine);
        }
        all.extend_from_slice(args);
        self.runner
            .run(&self.dir, "systemctl", &all)
            .context("running systemctl")
    }
}

/// The desktop user to reach for a `--user` call, when this process was started with `sudo`.
///
/// `sudo` sets `SUDO_USER` to the account that invoked it; a login shell already running as
/// root (no `sudo` involved) leaves it unset, which correctly means "nothing to cross into".
#[must_use]
pub fn sudo_target_user() -> Option<String> {
    std::env::var("SUDO_USER").ok()
}

/// A user's numeric uid, read from `/etc/passwd` — not through libc, which the CLI does not
/// otherwise link (the same reasoning `install::is_root` gives for reading `/proc/self/status`
/// directly).
fn uid_for_user(user: &str) -> Option<u32> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    uid_for_user_in(&passwd, user)
}

fn uid_for_user_in(passwd: &str, user: &str) -> Option<u32> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == user)
            .then(|| fields.nth(1))
            .flatten()
            .and_then(|uid| uid.parse().ok())
    })
}

/// `--machine=<user>@.host`, for a `--user` call this process cannot otherwise reach.
///
/// Only applies to `Scope::User`: a system-manager call from root already has the access it
/// needs, and a target user of "root" (or none, e.g. run outside `sudo`) means there is nothing
/// to cross into.
fn machine_arg(scope: Scope, target_user: Option<&str>) -> Option<String> {
    if !matches!(scope, Scope::User | Scope::None) {
        return None;
    }
    let user = target_user?;
    (!user.is_empty() && user != "root").then(|| format!("--machine={user}@.host"))
}

impl Systemctl for RealSystemctl {
    fn restart(&self, scope: Scope, unit: &str) -> Result<()> {
        let output = self.systemctl(scope, &["restart", unit])?;
        if output.status.success() {
            Ok(())
        } else {
            bail!(
                "systemctl could not restart {unit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
    }

    fn is_active(&self, scope: Scope, unit: &str) -> Result<bool> {
        // `is-active` exits non-zero for an inactive unit, which is an answer rather than a
        // failure — so the exit status is deliberately not checked here, only the word it printed.
        let output = self.systemctl(scope, &["is-active", unit])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim() == "active")
    }

    fn restart_count(&self, scope: Scope, unit: &str) -> Result<u32> {
        let output = self.systemctl(scope, &["show", "--property=NRestarts", "--value", unit])?;
        let text = String::from_utf8_lossy(&output.stdout);
        // An absent or unparsable NRestarts reads as zero: the property is missing on unit types
        // that cannot restart, and a settle window comparing zero with zero is simply a window
        // that proves nothing rather than an error that stops a deployment.
        Ok(text.trim().parse().unwrap_or(0))
    }
}

/// Whether a bus name has an owner.
#[derive(Debug)]
pub struct RealBusProbe {
    pub runner: Arc<dyn CommandRunner>,
    pub dir: PathBuf,
    /// See [`RealSystemctl::target_user`]: the session bus this probes is the desktop user's,
    /// not root's, when this process is running under `sudo`.
    pub target_user: Option<String>,
}

impl BusProbe for RealBusProbe {
    fn answers(&self, bus_name: &str) -> bool {
        let machine = machine_arg(Scope::User, self.target_user.as_deref());
        let mut args = vec!["--user", "--no-pager"];
        if let Some(machine) = &machine {
            args.push(machine);
        }
        args.extend_from_slice(&[
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            "s",
            bus_name,
        ]);
        let Ok(output) = self.runner.run(&self.dir, "busctl", &args) else {
            return false;
        };
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("true")
    }
}

/// The wall clock.
#[derive(Debug, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    fn sleep_ms(&self, millis: u64) {
        std::thread::sleep(std::time::Duration::from_millis(millis));
    }
}

/// The runtime stamps, read fresh each time — which is the point, since the gate is waiting for
/// one of them to change.
#[derive(Debug, Default)]
pub struct RealStamps {
    /// See [`RealSystemctl::target_user`]: a user-scope component's stamp lives in the desktop
    /// user's own runtime directory (`/run/user/<uid>/templaros/build`), not root's, when this
    /// process is running under `sudo` — root has no `XDG_RUNTIME_DIR` of its own, so reading
    /// only `templar_build::stamp_dir()` finds a system-scope component's stamp but never a
    /// user-scope one, which is most of the matrix.
    pub target_user: Option<String>,
}

impl Stamps for RealStamps {
    fn read(&self) -> BTreeMap<String, RunningStamp> {
        // An explicit single-directory override (tests, `templar dev status --stamps`) takes
        // priority over any user-crossing logic below.
        if std::env::var_os("TEMPLAROS_BUILD_STAMP_DIR").is_some() {
            return templar_build::read_stamps();
        }
        let mut stamps = templar_build::read_stamps_in(
            &PathBuf::from("/run/templaros/build"),
            &templar_build::pid_is_alive,
        );
        let user_dir = self
            .target_user
            .as_deref()
            .and_then(uid_for_user)
            .map_or_else(templar_build::stamp_dir, |uid| {
                PathBuf::from(format!("/run/user/{uid}/templaros/build"))
            });
        stamps.extend(templar_build::read_stamps_in(
            &user_dir,
            &templar_build::pid_is_alive,
        ));
        stamps
    }
}

#[cfg(test)]
mod tests {
    use super::super::cmd::FakeCommandRunner;
    use super::*;

    fn systemctl() -> (RealSystemctl, Arc<FakeCommandRunner>) {
        let runner = Arc::new(FakeCommandRunner::default());
        (
            RealSystemctl {
                runner: runner.clone(),
                dir: PathBuf::from("/"),
                target_user: None,
            },
            runner,
        )
    }

    #[test]
    fn a_user_unit_is_restarted_in_the_user_manager() {
        let (systemctl, runner) = systemctl();
        systemctl
            .restart(Scope::User, "templar-shell.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec!["systemctl --user --no-pager restart templar-shell.service"]
        );
    }

    #[test]
    fn a_user_unit_is_restarted_via_machine_when_running_as_root() {
        // Under `sudo`, this process has no session bus of its own; `--machine=<user>@.host` is
        // how it reaches the desktop user's `--user` manager instead of failing with "
        // DBUS_SESSION_BUS_ADDRESS and XDG_RUNTIME_DIR not defined".
        let (mut systemctl, runner) = systemctl();
        systemctl.target_user = Some("dk".to_owned());
        systemctl
            .restart(Scope::User, "templar-resourced.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec![
                "systemctl --user --no-pager --machine=dk@.host restart templar-resourced.service"
            ]
        );
    }

    #[test]
    fn a_system_unit_never_takes_a_machine_flag() {
        // `--system` from root already has the access it needs; a `--machine` there would be a
        // pointless, possibly wrong, deviation.
        let (mut systemctl, runner) = systemctl();
        systemctl.target_user = Some("dk".to_owned());
        systemctl
            .restart(Scope::System, "templar-hostd.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec!["systemctl --system --no-pager restart templar-hostd.service"]
        );
    }

    #[test]
    fn a_target_user_of_root_takes_no_machine_flag() {
        // Run outside `sudo` (or as root's own session), there is nothing to cross into.
        let (mut systemctl, runner) = systemctl();
        systemctl.target_user = Some("root".to_owned());
        systemctl
            .restart(Scope::User, "templar-shell.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec!["systemctl --user --no-pager restart templar-shell.service"]
        );
    }

    #[test]
    fn a_system_unit_is_restarted_in_the_system_manager() {
        let (systemctl, runner) = systemctl();
        systemctl
            .restart(Scope::System, "templar-hostd.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec!["systemctl --system --no-pager restart templar-hostd.service"]
        );
    }

    #[test]
    fn is_active_reads_the_word_not_the_exit_code() {
        // `systemctl is-active` exits non-zero for an inactive unit. That is an answer, not a
        // failure, and treating it as one would fail every gate on a unit that is merely down.
        let (systemctl, runner) = systemctl();
        runner.say("systemctl", "active\n");
        assert!(
            systemctl
                .is_active(Scope::User, "templar-shell.service")
                .unwrap()
        );
        runner.say("systemctl", "failed\n");
        assert!(
            !systemctl
                .is_active(Scope::User, "templar-shell.service")
                .unwrap()
        );
    }

    #[test]
    fn a_missing_restart_count_reads_as_zero_rather_than_failing() {
        let (systemctl, runner) = systemctl();
        runner.say("systemctl", "\n");
        assert_eq!(
            systemctl
                .restart_count(Scope::User, "templar-shell.service")
                .unwrap(),
            0
        );
        runner.say("systemctl", "4\n");
        assert_eq!(
            systemctl
                .restart_count(Scope::User, "templar-shell.service")
                .unwrap(),
            4
        );
    }

    #[test]
    fn a_bus_name_answers_only_when_it_has_an_owner() {
        let runner = Arc::new(FakeCommandRunner::default());
        let probe = RealBusProbe {
            runner: runner.clone(),
            dir: PathBuf::from("/"),
            target_user: None,
        };
        runner.say("busctl", "b true\n");
        assert!(probe.answers("org.templaros.Resources"));
        runner.say("busctl", "b false\n");
        assert!(!probe.answers("org.templaros.Resources"));
        assert!(runner.lines()[0].contains("NameHasOwner"));
    }

    #[test]
    fn a_users_uid_is_read_from_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\ndk:x:1000:1000:dk,,,:/home/dk:/bin/bash\n";
        assert_eq!(uid_for_user_in(passwd, "dk"), Some(1000));
        assert_eq!(uid_for_user_in(passwd, "root"), Some(0));
        assert_eq!(uid_for_user_in(passwd, "nobody-here"), None);
    }
}
