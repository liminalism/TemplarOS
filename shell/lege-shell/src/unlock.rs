//! Asking `lege-unlock` whether a password is the person's.
//!
//! The shell never links PAM. It spawns the helper, writes the password to its stdin and reads
//! its exit status; that is the whole protocol, and it is why a bug in a PAM module cannot take
//! the shell's Wayland connection down with it.
//!
//! The password is written to a pipe rather than passed as an argument, because an argument is
//! in `/proc/<pid>/cmdline` and every process on the machine can read that.

use std::process::Stdio;
use tokio::io::AsyncWriteExt as _;

/// Where the helper is installed. The shell's own package ships it, so this is a fixed path
/// rather than a `PATH` lookup: a lock screen must not authenticate against whichever
/// `lege-unlock` happens to be earliest on somebody's `PATH`.
const HELPER: &str = "/usr/libexec/lege-unlock";

/// The helper as run from a development build, tried only when the installed one is absent.
/// `lege dev` runs the shell out of `target/`, where nothing is installed under `/usr/libexec`.
fn development_helper() -> Option<std::path::PathBuf> {
    let beside = std::env::current_exe().ok()?.parent()?.join("lege-unlock");
    beside.is_file().then_some(beside)
}

/// What a person is told when the helper cannot be run at all. Deliberately the same shape as a
/// wrong password from their side of the screen — the detail is in the journal.
const UNAVAILABLE: &str = "Could not check the password";

/// Check `password` against PAM. `Ok(())` unlocks; `Err` carries the line to show.
pub async fn authenticate(password: String) -> Result<(), String> {
    let installed = std::path::Path::new(HELPER);
    let helper = if installed.is_file() {
        installed.to_path_buf()
    } else if let Some(path) = development_helper() {
        path
    } else {
        tracing::warn!(helper = HELPER, "the unlock helper is not installed");
        return Err(UNAVAILABLE.to_owned());
    };

    let mut child = match tokio::process::Command::new(&helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, helper = %helper.display(), "spawning the unlock helper");
            return Err(UNAVAILABLE.to_owned());
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        // A closed pipe is the helper having exited early; its status says what happened, so
        // this failing is not itself the answer.
        let _ = stdin.write_all(password.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.shutdown().await;
    }

    match child.wait().await {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err("Incorrect password".to_owned()),
        Err(error) => {
            tracing::warn!(%error, "waiting for the unlock helper");
            Err(UNAVAILABLE.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing helper must be a refusal, not an unlock. This is the failure mode that matters:
    /// "the thing that says no is not there" defaulting to yes would unlock every locked session
    /// on a machine where the package did not install cleanly.
    #[tokio::test]
    async fn a_missing_helper_refuses_rather_than_unlocking() {
        // Neither `/usr/libexec/lege-unlock` (this is not an installed system) nor a helper
        // beside the test binary exists, so this exercises the absent path.
        if std::path::Path::new(HELPER).is_file() || development_helper().is_some() {
            return;
        }
        assert_eq!(
            authenticate("anything".to_owned()).await,
            Err(UNAVAILABLE.to_owned())
        );
    }

    /// A helper that exits non-zero is a wrong password, and one that exits zero is not. Checked
    /// against `/bin/false` and `/bin/true` so the status handling is exercised without PAM.
    #[tokio::test]
    async fn the_helpers_exit_status_is_the_whole_answer() {
        for (program, expected) in [
            ("/bin/true", Ok(())),
            ("/bin/false", Err("Incorrect password".to_owned())),
        ] {
            if !std::path::Path::new(program).is_file() {
                continue;
            }
            let mut child = tokio::process::Command::new(program)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("spawning a shell builtin");
            drop(child.stdin.take());
            let status = child.wait().await.expect("waiting");
            let answer = if status.success() {
                Ok(())
            } else {
                Err("Incorrect password".to_owned())
            };
            assert_eq!(answer, expected, "{program}");
        }
    }
}
