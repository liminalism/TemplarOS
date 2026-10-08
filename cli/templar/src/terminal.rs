//! `templar terminal` — start the Terminal provider role through `Launcher1`, in a directory.
//!
//! This is what `templar-terminal` (and so PCManFM-Qt's "Open in Terminal" and "Execute in
//! Terminal") runs. A terminal a program spawns itself lands in that program's cgroup and is
//! identified as that program; one started through `Launcher1` gets its own
//! `app-templaros-<id>@<instance>.service` unit, and with it its own identity, window grouping
//! and history.
//!
//! `Launcher1.Launch` takes an argument vector and nothing else: no working directory, no
//! environment. Every launched unit starts in the home directory, so the directory travels as an
//! argument — `--directory DIR`, kitty's spelling, the same form the shell's own "Open terminal
//! here" passes the role (`templar-shell/src/surface/app.rs`, `spawn_open_terminal`). A command
//! follows as `-e CMD ARGS…` (kitty accepts `-e` as a no-op, and it is every other terminal's
//! spelling), and `--hold` keeps the window open after it exits. A terminal provider that spells
//! these differently needs its own form; none is declared today.
//!
//! Only when `org.templaros.Session` cannot be reached at all — no session bus, or nobody owning
//! the name — is there no role to resolve; then the Debian `x-terminal-emulator` alternative is
//! executed directly in the directory, with `-e` (the one option Debian policy requires of it).

use crate::providers::{Resolution, parse_resolution};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use templar_types::ProviderRole;

/// What Debian policy guarantees exists and accepts `-e`; used only without a session.
pub const FALLBACK_TERMINAL: &str = "x-terminal-emulator";

/// One request: where, and optionally what to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Absolute, existing directory.
    pub directory: PathBuf,
    pub hold: bool,
    /// Program and arguments; empty for an interactive shell.
    pub command: Vec<String>,
}

impl Request {
    /// `directory` is resolved against `cwd` (libfm-qt starts the terminal command *in* the
    /// folder, so no flag at all means "here") and must be an existing directory.
    pub fn new(directory: Option<&Path>, cwd: &Path, hold: bool, command: Vec<String>) -> Result<Self> {
        let directory = match directory {
            Some(directory) if directory.is_absolute() => directory.to_owned(),
            Some(directory) => cwd.join(directory),
            None => cwd.to_owned(),
        };
        anyhow::ensure!(
            directory.is_dir(),
            "{} is not a directory",
            directory.display()
        );
        Ok(Self { directory, hold, command })
    }

    fn directory_str(&self) -> Result<&str> {
        self.directory.to_str().with_context(|| {
            format!(
                "{} is not valid UTF-8, and Launcher1 arguments are D-Bus strings",
                self.directory.display()
            )
        })
    }

    /// The argument vector handed to `Launcher1.Launch` for the role's provider.
    pub fn provider_arguments(&self) -> Result<Vec<String>> {
        let mut arguments = vec!["--directory".to_owned(), self.directory_str()?.to_owned()];
        if self.hold {
            arguments.push("--hold".to_owned());
        }
        if !self.command.is_empty() {
            arguments.push("-e".to_owned());
            arguments.extend(self.command.iter().cloned());
        }
        Ok(arguments)
    }

    /// The arguments for [`FALLBACK_TERMINAL`], which is started *in* the directory instead.
    /// `--hold` has no portable spelling and is not passed.
    pub fn fallback_arguments(&self) -> Vec<String> {
        if self.command.is_empty() {
            return Vec::new();
        }
        let mut arguments = vec!["-e".to_owned()];
        arguments.extend(self.command.iter().cloned());
        arguments
    }
}

/// The Terminal role's provider from a `Providers1.Report`, or why there is none.
pub fn terminal_provider(report: &[(String, String)]) -> Result<String> {
    let role = ProviderRole::Terminal.as_str();
    let (_, resolution) = report
        .iter()
        .find(|(name, _)| name == role)
        .context("Providers1.Report does not list the terminal role")?;
    match parse_resolution(resolution)? {
        Resolution::Selected { app_id } | Resolution::DefaultedByPriority { app_id, .. } => {
            Ok(app_id)
        }
        Resolution::SelectionStale { app_id } => anyhow::bail!(
            "the selected terminal {app_id} is not installed — choose one in Settings ▸ Providers"
        ),
        Resolution::NoProvider => {
            anyhow::bail!("no terminal is installed — choose one in Settings ▸ Providers")
        }
    }
}

/// Whether a D-Bus error means `org.templaros.Session` is not there at all, as opposed to
/// answering with a refusal — only the former falls back to [`FALLBACK_TERMINAL`].
pub fn session_unreachable(error: &zbus::Error) -> bool {
    const ABSENT: [&str; 2] = [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
    ];
    match error {
        zbus::Error::MethodError(name, _, _) => ABSENT.contains(&name.as_str()),
        zbus::Error::FDO(error) => matches!(
            **error,
            zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
        ),
        zbus::Error::Connection(..)
        | zbus::Error::Handshake(_)
        | zbus::Error::InputOutput(_)
        | zbus::Error::Address(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(hold: bool, command: &[&str]) -> Request {
        let dir = std::env::temp_dir();
        Request::new(None, &dir, hold, command.iter().map(|s| (*s).to_owned()).collect()).unwrap()
    }

    #[test]
    fn a_plain_request_opens_the_terminal_in_the_directory() {
        let request = request(false, &[]);
        assert_eq!(
            request.provider_arguments().unwrap(),
            vec!["--directory".to_owned(), std::env::temp_dir().to_str().unwrap().to_owned()]
        );
        assert!(request.fallback_arguments().is_empty());
    }

    #[test]
    fn a_command_follows_dash_e_verbatim_and_hold_precedes_it() {
        let request = request(true, &["sh", "-c", "echo $HOME; ls -l"]);
        let arguments = request.provider_arguments().unwrap();
        assert_eq!(&arguments[2..], ["--hold", "-e", "sh", "-c", "echo $HOME; ls -l"]);
        assert_eq!(request.fallback_arguments(), ["-e", "sh", "-c", "echo $HOME; ls -l"]);
    }

    #[test]
    fn a_relative_directory_is_resolved_and_a_missing_one_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a b")).unwrap();
        let request = Request::new(Some(Path::new("a b")), root.path(), false, Vec::new()).unwrap();
        assert_eq!(request.directory, root.path().join("a b"));
        assert!(Request::new(Some(Path::new("missing")), root.path(), false, Vec::new()).is_err());
    }

    #[test]
    fn the_role_resolves_to_its_selected_or_defaulted_provider_only() {
        let report = |json: &str| vec![("terminal".to_owned(), json.to_owned())];
        assert_eq!(
            terminal_provider(&report(r#"{"kind":"selected","app_id":"desktop-import.kitty.desktop"}"#)).unwrap(),
            "desktop-import.kitty.desktop"
        );
        assert_eq!(
            terminal_provider(&report(r#"{"kind":"defaulted-by-priority","app_id":"org.example.Term"}"#)).unwrap(),
            "org.example.Term"
        );
        let stale = terminal_provider(&report(r#"{"kind":"selection-stale","app_id":"gone"}"#));
        assert!(stale.unwrap_err().to_string().contains("not installed"));
        assert!(terminal_provider(&report(r#"{"kind":"no-provider"}"#)).is_err());
        assert!(terminal_provider(&[]).is_err());
    }

    #[test]
    fn only_an_absent_session_counts_as_unreachable() {
        let absent = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown("gone".into())));
        assert!(session_unreachable(&absent));
        let refused = zbus::Error::FDO(Box::new(zbus::fdo::Error::Failed("no".into())));
        assert!(!session_unreachable(&refused));
        let no_bus = zbus::Error::Connection(
            std::sync::Arc::new(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "unix:path=/nonexistent".parse().unwrap(),
        );
        assert!(session_unreachable(&no_bus));
    }
}
