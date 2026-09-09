//! External command execution, abstracted so every backend that shells out (apt, `bootctl`,
//! `systemd-bless-boot`, `grub-reboot`, `journalctl --vacuum-size`) can be driven by a fake in
//! tests. Rule 4: nothing in this crate may call `std::process::Command` directly outside the
//! real implementation here.

use anyhow::{Context, Result};
use std::process::{Command, Output};

/// Runs an external command and returns its captured output. Implementations must not block
/// indefinitely: the real one enforces a wall-clock timeout so a hung `apt-get` or `grub-reboot`
/// cannot wedge a transaction forever.
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output>;
}

/// Spawns real processes with a timeout. `unsafe_code` is not needed: `Command` and `try_wait`
/// are safe APIs.
#[derive(Debug, Clone, Copy)]
pub struct RealCommandRunner {
    pub timeout: std::time::Duration,
}

impl Default for RealCommandRunner {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(300),
        }
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {program}"))?;

        let deadline = std::time::Instant::now() + self.timeout;
        loop {
            if let Some(status) = child.try_wait().context("polling child status")? {
                use std::io::Read;
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{program} timed out after {:?}", self.timeout);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// One recorded invocation, for test assertions.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
}

/// Records every call and returns scripted results, keyed by program name (first match wins,
/// then falls back to a success with empty output). Used by every backend's unit tests so none
/// of them touch the real filesystem-altering tools.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeCommandRunner {
    pub calls: std::sync::Mutex<Vec<Invocation>>,
    pub failures: std::sync::Mutex<std::collections::HashSet<String>>,
    /// `(substring of "program arg arg…", stdout)`, first match wins. Keyed on a substring
    /// rather than the program because one tool has many subcommands: `btrfs subvolume show`
    /// and `btrfs filesystem du` are parsed by different callers and must answer differently.
    pub outputs: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
}

#[cfg(test)]
impl FakeCommandRunner {
    pub fn fail(&self, program: &str) {
        self.failures
            .lock()
            .expect("poisoned")
            .insert(program.to_owned());
    }

    /// Answer any invocation whose command line contains `matching` with `stdout`.
    pub fn respond(&self, matching: &str, stdout: &str) {
        self.outputs
            .lock()
            .expect("poisoned")
            .push((matching.to_owned(), stdout.as_bytes().to_vec()));
    }

    pub fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().expect("poisoned").clone()
    }
}

#[cfg(test)]
impl CommandRunner for FakeCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output> {
        // TemplarOS targets Linux only (ARCHITECTURE.md); a real exit status is unavailable
        // without actually spawning a process, so synthesize a successful one directly.
        use std::os::unix::process::ExitStatusExt;

        self.calls.lock().expect("poisoned").push(Invocation {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        });
        if self.failures.lock().expect("poisoned").contains(program) {
            anyhow::bail!("{program} failed (fake)");
        }
        let line = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        let stdout = self
            .outputs
            .lock()
            .expect("poisoned")
            .iter()
            .find(|(matching, _)| line.contains(matching.as_str()))
            .map(|(_, stdout)| stdout.clone())
            .unwrap_or_default();
        Ok(Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout,
            stderr: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_runner_captures_output_and_status() {
        let runner = RealCommandRunner {
            timeout: std::time::Duration::from_secs(5),
        };
        let output = runner.run("printf", &["hello"]).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
    }

    #[test]
    fn real_runner_times_out_a_hung_process() {
        let runner = RealCommandRunner {
            timeout: std::time::Duration::from_millis(100),
        };
        let err = runner.run("sleep", &["5"]).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn fake_runner_records_and_scripts_failure() {
        let fake = FakeCommandRunner::default();
        fake.run("bootctl", &["set-default", "x"]).unwrap();
        fake.fail("grub-reboot");
        assert!(fake.run("grub-reboot", &["1"]).is_err());
        assert_eq!(fake.calls().len(), 2);
        assert_eq!(fake.calls()[0].program, "bootctl");
    }
}
