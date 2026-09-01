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
use lege_build::{RunningStamp, components::Scope};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// `systemctl`, in whichever manager a component's unit lives in.
#[derive(Debug)]
pub struct RealSystemctl {
    pub runner: Arc<dyn CommandRunner>,
    /// Working directory for the invocations — irrelevant to systemctl, but the runner asks.
    pub dir: PathBuf,
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
        let mut all = vec![Self::scope_flag(scope), "--no-pager"];
        all.extend_from_slice(args);
        self.runner
            .run(&self.dir, "systemctl", &all)
            .context("running systemctl")
    }
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
}

impl BusProbe for RealBusProbe {
    fn answers(&self, bus_name: &str) -> bool {
        let Ok(output) = self.runner.run(
            &self.dir,
            "busctl",
            &[
                "--user",
                "--no-pager",
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                "s",
                bus_name,
            ],
        ) else {
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
pub struct RealStamps;

impl Stamps for RealStamps {
    fn read(&self) -> BTreeMap<String, RunningStamp> {
        lege_build::read_stamps()
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
            },
            runner,
        )
    }

    #[test]
    fn a_user_unit_is_restarted_in_the_user_manager() {
        let (systemctl, runner) = systemctl();
        systemctl
            .restart(Scope::User, "lege-shell.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec!["systemctl --user --no-pager restart lege-shell.service"]
        );
    }

    #[test]
    fn a_system_unit_is_restarted_in_the_system_manager() {
        let (systemctl, runner) = systemctl();
        systemctl
            .restart(Scope::System, "lege-hostd.service")
            .unwrap();
        assert_eq!(
            runner.lines(),
            vec!["systemctl --system --no-pager restart lege-hostd.service"]
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
                .is_active(Scope::User, "lege-shell.service")
                .unwrap()
        );
        runner.say("systemctl", "failed\n");
        assert!(
            !systemctl
                .is_active(Scope::User, "lege-shell.service")
                .unwrap()
        );
    }

    #[test]
    fn a_missing_restart_count_reads_as_zero_rather_than_failing() {
        let (systemctl, runner) = systemctl();
        runner.say("systemctl", "\n");
        assert_eq!(
            systemctl
                .restart_count(Scope::User, "lege-shell.service")
                .unwrap(),
            0
        );
        runner.say("systemctl", "4\n");
        assert_eq!(
            systemctl
                .restart_count(Scope::User, "lege-shell.service")
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
        };
        runner.say("busctl", "b true\n");
        assert!(probe.answers("org.legeos.Resources"));
        runner.say("busctl", "b false\n");
        assert!(!probe.answers("org.legeos.Resources"));
        assert!(runner.lines()[0].contains("NameHasOwner"));
    }
}
