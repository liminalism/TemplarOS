//! One-shot native argv adapter for the application broker's systemd units.
//!
//! No policy or service runs here: the broker has already selected the executable.
//! Successful exec replaces this adapter, retaining PID, cgroup and environment.

#![forbid(unsafe_code)]

use anyhow::{Context, Result, anyhow, ensure};
use std::{
    ffi::OsString,
    os::unix::process::CommandExt,
    process::{Command, ExitCode},
};
use templar_appd::native_exec;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("templar-app-exec: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let payload = payload_argument(std::env::args_os().skip(1))?;
    exec_payload(&payload)
}

fn exec_payload(payload: &str) -> Result<()> {
    let launch = native_exec::decode(payload)?;
    let mut command = Command::new(&launch.executable);
    command.args(&launch.arguments);
    if let Some(directory) = &launch.directory {
        command.current_dir(directory);
    }
    // No child or shell: Command::exec establishes native cwd before replacing
    // this process, so the actual application retains systemd's main PID.
    Err(command.exec()).context("executing brokered application")
}

fn payload_argument(mut arguments: impl Iterator<Item = OsString>) -> Result<String> {
    let payload = arguments
        .next()
        .context("expected exactly one native launch payload argument")?;
    ensure!(
        arguments.next().is_none(),
        "expected exactly one native launch payload argument"
    );
    payload
        .into_string()
        .map_err(|_| anyhow!("native launch payload argument must be UTF-8 JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::ffi::OsStringExt, path::Path};

    #[test]
    fn accepts_only_one_payload_argument_and_preserves_it_literally() {
        let payload = native_exec::encode(
            Path::new("/usr/bin/viewer"),
            &[
                OsString::from("two words %f $HOME --secret=literal-token"),
                OsString::from_vec(vec![0xfe, 0xff]),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            payload_argument([OsString::from(&payload)].into_iter()).unwrap(),
            payload
        );
        assert!(payload_argument(std::iter::empty()).is_err());
        assert!(
            payload_argument([OsString::from(&payload), OsString::from("extra")].into_iter())
                .is_err()
        );
    }

    #[test]
    fn rejects_non_utf8_transport_without_echoing_bytes() {
        let error =
            payload_argument([OsString::from_vec(b"secret-token-\xff".to_vec())].into_iter())
                .unwrap_err();
        assert!(!format!("{error:#}").contains("secret-token"));
    }

    #[test]
    #[ignore = "subprocess entry point for the exec consumer test"]
    fn exec_adapter_child() {
        let payload = std::env::var("TEMPLAR_EXEC_TEST_PAYLOAD").unwrap();
        exec_payload(&payload).unwrap();
    }

    #[test]
    fn actual_exec_preserves_pid_native_argv_and_native_working_directory() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let temp = tempfile::tempdir().unwrap();
        let directory = temp
            .path()
            .join(OsString::from_vec(b"cwd-\xff-${HOME}-$VAR-$$".to_vec()));
        std::fs::create_dir(&directory).unwrap();
        let name = OsString::from_vec(b"link-\xfe-${HOME}-$VAR-$$".to_vec());
        let target = OsString::from_vec(b"target-\xff-${HOME}-$VAR-$$".to_vec());
        symlink(&target, directory.join(&name)).unwrap();
        let payload = native_exec::encode(
            Path::new("/usr/bin/readlink"),
            &[
                name,
                OsString::from("/proc/self"),
                OsString::from("/proc/self/cwd"),
            ],
            Some(&directory),
        )
        .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "tests::exec_adapter_child",
                "--quiet",
                "--nocapture",
            ])
            .env("TEMPLAR_EXEC_TEST_PAYLOAD", payload)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let mut expected = target.as_bytes().to_vec();
        expected.push(b'\n');
        expected.extend_from_slice(pid.to_string().as_bytes());
        expected.push(b'\n');
        expected.extend_from_slice(directory.canonicalize().unwrap().as_os_str().as_bytes());
        expected.push(b'\n');
        assert!(output.stdout.ends_with(&expected));
    }
}
