//! A display hint only: saved after greetd confirms a session started, never after authentication
//! alone. No password, token, or session command is persisted.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) const PATH: &str = "/var/lib/templar-greeter/last-user";

fn valid(username: &str) -> bool {
    !username.is_empty() && username.len() <= 256 && !username.chars().any(char::is_control)
}

pub(crate) fn load(path: &Path) -> Option<String> {
    let mut username = String::new();
    fs::File::open(path)
        .ok()?
        .take(258)
        .read_to_string(&mut username)
        .ok()?;
    let username = username.strip_suffix('\n').unwrap_or(&username);
    valid(username).then(|| username.to_owned())
}

pub(crate) fn save(path: &Path, username: &str) -> io::Result<()> {
    if !valid(username) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid username hint",
        ));
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(username.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    // We own this temporary file: create_new succeeded before entering the write transaction.
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn hint_round_trips_replaces_and_rejects_multiline_or_oversized_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-user");
        assert_eq!(load(&path), None);
        save(&path, "alice").unwrap();
        assert_eq!(load(&path).as_deref(), Some("alice"));
        save(&path, "bob@example.org").unwrap();
        assert_eq!(load(&path).as_deref(), Some("bob@example.org"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for value in [String::new(), "alice\nbob".into(), "x".repeat(257)] {
            assert!(save(&path, &value).is_err());
            fs::write(&path, &value).unwrap();
            assert_eq!(load(&path), None);
        }
    }

    #[test]
    fn failed_replacement_preserves_the_hint_and_unowned_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-user");
        save(&path, "alice").unwrap();
        assert!(save(&path, "bob\nmallory").is_err());
        assert_eq!(load(&path).as_deref(), Some("alice"));
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temporary, "not owned by this transaction").unwrap();
        assert!(save(&path, "bob").is_err());
        assert_eq!(load(&path).as_deref(), Some("alice"));
        assert_eq!(
            fs::read_to_string(&temporary).unwrap(),
            "not owned by this transaction"
        );
    }

    #[test]
    fn failed_rename_cleans_up_the_owned_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-user");
        fs::create_dir(&path).unwrap();
        assert!(save(&path, "alice").is_err());
        assert!(path.is_dir());
        assert!(
            !path
                .with_extension(format!("tmp-{}", std::process::id()))
                .exists()
        );
    }
}
