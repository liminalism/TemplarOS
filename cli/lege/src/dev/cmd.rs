//! External command execution, abstracted so the deployment tooling can be driven by a fake.
//!
//! `lege dev` shells out to `apt-ftparchive`, `gpg`, `apt-get` and `systemctl` — every one of
//! which either rewrites the system's package state or asks for a passphrase. None of that can
//! run in a unit test, and a deployment tool whose argv is untested is a deployment tool nobody
//! should run. So the same seam `lege-hostd` uses for apt and the boot managers is used here:
//! nothing outside [`RealCommandRunner`] may call `std::process::Command`.
//!
//! This is a knowing duplication of `services/lege-hostd/src/cmd.rs`. Two consumers is not enough
//! to justify a shared crate, and hostd's copy carries a transaction timeout policy this one has
//! no business inheriting; if a third appears, extract then.

use anyhow::{Context, Result};
use std::io::Read as _;
use std::{
    path::Path,
    process::{Command, Output},
};

/// Runs an external command in a given directory and returns its captured output.
///
/// The working directory is part of the signature rather than an option, because
/// `apt-ftparchive` writes *relative* `Filename:` fields into the index it generates: run it from
/// the wrong directory and the repository it produces points at paths apt cannot fetch. Making
/// callers say where they are is cheaper than debugging that once.
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    /// # Errors
    /// If the command cannot be spawned, or does not finish within the runner's timeout.
    fn run(&self, dir: &Path, program: &str, args: &[&str]) -> Result<Output>;
}

/// Spawns real processes with a wall-clock timeout, so a `gpg` waiting on a pinentry that will
/// never appear cannot wedge the command forever.
#[derive(Debug, Clone, Copy)]
pub struct RealCommandRunner {
    pub timeout: std::time::Duration,
}

impl Default for RealCommandRunner {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(600),
        }
    }
}

/// Reads one of a child's pipes to the end on its own thread, so the child never blocks on a
/// full pipe while the runner waits for it to exit.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, dir: &Path, program: &str, args: &[&str]) -> Result<Output> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {program}"))?;

        // Drain both pipes while the child runs. Reading them only after it exits deadlocks as
        // soon as the output exceeds the pipe buffer: apt-ftparchive indexing three sets blocked
        // on a full stdout, and the runner sat out its whole timeout waiting for it to finish.
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let deadline = std::time::Instant::now() + self.timeout;
        loop {
            if let Some(status) = child.try_wait().context("polling child status")? {
                return Ok(Output {
                    status,
                    stdout: stdout.join().unwrap_or_default(),
                    stderr: stderr.join().unwrap_or_default(),
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
    pub dir: std::path::PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

#[cfg(test)]
impl Invocation {
    /// The whole invocation as one string, which is what a test wants to assert about.
    pub fn line(&self) -> String {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Records every call, returns scripted stdout, and can be told to fail a given program.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeCommandRunner {
    pub calls: std::sync::Mutex<Vec<Invocation>>,
    pub failures: std::sync::Mutex<std::collections::HashSet<String>>,
    pub stdout: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

#[cfg(test)]
impl FakeCommandRunner {
    pub fn fail(&self, program: &str) {
        self.failures
            .lock()
            .expect("poisoned")
            .insert(program.to_owned());
    }

    pub fn say(&self, program: &str, stdout: &str) {
        self.stdout
            .lock()
            .expect("poisoned")
            .insert(program.to_owned(), stdout.as_bytes().to_vec());
    }

    pub fn calls(&self) -> Vec<Invocation> {
        self.calls.lock().expect("poisoned").clone()
    }

    /// Every invocation as one line, in order.
    pub fn lines(&self) -> Vec<String> {
        self.calls().iter().map(Invocation::line).collect()
    }
}

#[cfg(test)]
impl CommandRunner for FakeCommandRunner {
    fn run(&self, dir: &Path, program: &str, args: &[&str]) -> Result<Output> {
        // LegeOS targets Linux only; a real exit status is unavailable without spawning a
        // process, so synthesize a successful one directly.
        use std::os::unix::process::ExitStatusExt as _;

        self.calls.lock().expect("poisoned").push(Invocation {
            dir: dir.to_path_buf(),
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        });
        if self.failures.lock().expect("poisoned").contains(program) {
            anyhow::bail!("{program} failed (fake)");
        }
        Ok(Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: self
                .stdout
                .lock()
                .expect("poisoned")
                .get(program)
                .cloned()
                .unwrap_or_default(),
            stderr: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_runner_captures_output_and_status() {
        let runner = RealCommandRunner {
            timeout: std::time::Duration::from_secs(5),
        };
        let output = runner.run(Path::new("/"), "printf", &["hello"]).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
    }

    #[test]
    fn the_real_runner_does_not_deadlock_on_output_larger_than_a_pipe() {
        let runner = RealCommandRunner {
            timeout: std::time::Duration::from_secs(10),
        };
        let output = runner
            .run(Path::new("/"), "head", &["-c", "300000", "/dev/zero"])
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 300_000);
    }

    #[test]
    fn the_real_runner_kills_a_command_that_will_never_finish() {
        let runner = RealCommandRunner {
            timeout: std::time::Duration::from_millis(100),
        };
        let error = runner.run(Path::new("/"), "sleep", &["5"]).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[test]
    fn the_fake_records_argv_and_can_be_told_to_fail() {
        let fake = FakeCommandRunner::default();
        fake.run(Path::new("/tmp"), "apt-ftparchive", &["packages", "pool"])
            .unwrap();
        fake.fail("gpg");
        assert!(fake.run(Path::new("/tmp"), "gpg", &["--sign"]).is_err());
        assert_eq!(fake.lines()[0], "apt-ftparchive packages pool");
    }
}
