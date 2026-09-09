//! First-class file actions, so they stop being per-file-manager scripts.
//!
//! "Copy path", "copy URI", "copy shell-escaped path" and "open a terminal here" exist in every
//! desktop as a service menu, a downloaded script or a patch — and behave differently in each.
//! They are small enough to be exact, so the service owns them and every client gets the same
//! answer: the same quoting rules, the same URI escaping, the same idea of what a canonical
//! path is.

use anyhow::{Context, Result};
use std::{
    fmt::Write as _,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
};

/// A form of a path a user can put on the clipboard.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PathForm {
    /// The path with symlinks resolved and `..` removed.
    Canonical,
    /// The path exactly as the user is looking at it.
    AsShown,
    /// `file://` URI with the necessary octets percent-encoded.
    Uri,
    /// Safe to paste into a POSIX shell, however odd the name is.
    ShellQuoted,
    /// The containing directory, canonical.
    ParentDirectory,
    /// Just the file name.
    Name,
}

impl PathForm {
    pub const ALL: [PathForm; 6] = [
        Self::Canonical,
        Self::AsShown,
        Self::Uri,
        Self::ShellQuoted,
        Self::ParentDirectory,
        Self::Name,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::AsShown => "as-shown",
            Self::Uri => "uri",
            Self::ShellQuoted => "shell-quoted",
            Self::ParentDirectory => "parent-directory",
            Self::Name => "name",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|form| form.as_str() == value)
    }
}

/// Render `path` in `form`. Only [`PathForm::Canonical`] and [`PathForm::ParentDirectory`] touch
/// the filesystem, and both fall back to the literal path when the file has since gone.
pub fn render(path: &Path, form: PathForm) -> Result<String> {
    let canonical = || path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(match form {
        PathForm::Canonical => canonical().to_string_lossy().into_owned(),
        PathForm::AsShown => path.to_string_lossy().into_owned(),
        PathForm::Uri => file_uri(&canonical()),
        PathForm::ShellQuoted => shell_quote(&path.to_string_lossy()),
        PathForm::ParentDirectory => canonical()
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .with_context(|| format!("{} has no parent", path.display()))?,
        PathForm::Name => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .with_context(|| format!("{} has no file name", path.display()))?,
    })
}

/// `file:///home/u/a b.txt` — the URI form the portal and drag-and-drop both need.
///
/// The unreserved set of RFC 3986 plus `/`, which keeps the URI readable; everything else,
/// including the space that breaks naive consumers, is percent-encoded.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

/// Decode a `file://` URI back to a path. Returns `None` for any other scheme.
pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An empty or `localhost` authority is the only one a file URI may have.
    let rest = rest
        .strip_prefix("localhost")
        .filter(|remainder| remainder.starts_with('/'))
        .unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Single-quote for a POSIX shell. A name containing a quote is closed, escaped and reopened —
/// `it's` becomes `'it'\''s'` — which is the only form that is safe for every possible name.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'/' | b'@' | b'+' | b':' | b',')
        })
    {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// The working directory a "open a terminal here" action should use: the path itself when it is
/// a directory, otherwise the directory that contains it.
pub fn terminal_directory(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(Path::new("/")).to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_survives_hostile_names() {
        assert_eq!(shell_quote("plain.txt"), "plain.txt");
        assert_eq!(shell_quote("/a/b/c"), "/a/b/c");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("back`tick`"), "'back`tick`'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("new\nline"), "'new\nline'");
    }

    #[test]
    fn shell_quoting_is_actually_safe_in_a_shell() {
        for name in [
            "it's",
            "two words",
            "$(echo pwned)",
            "a;b",
            "a|b",
            "a\"b",
            "-dash",
        ] {
            let quoted = shell_quote(name);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                name,
                "quoting {name:?} as {quoted} did not round-trip"
            );
        }
    }

    #[test]
    fn file_uris_round_trip() {
        for path in [
            "/home/u/report.pdf",
            "/home/u/a b.txt",
            "/home/u/100%.txt",
            "/home/u/#anchor",
            "/home/u/ü.txt",
        ] {
            let uri = file_uri(Path::new(path));
            assert!(!uri.contains(' '), "{uri}");
            assert_eq!(path_from_uri(&uri), Some(PathBuf::from(path)));
        }
        assert_eq!(
            path_from_uri("file://localhost/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(path_from_uri("https://example.org/x"), None);
        assert_eq!(path_from_uri("file://remote/share"), None);
    }

    #[test]
    fn every_form_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/report.pdf");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();

        assert_eq!(render(&path, PathForm::Name).unwrap(), "report.pdf");
        assert!(
            render(&path, PathForm::Uri)
                .unwrap()
                .starts_with("file:///")
        );
        assert!(
            render(&path, PathForm::ParentDirectory)
                .unwrap()
                .ends_with("/sub")
        );
        assert_eq!(
            render(&path, PathForm::AsShown).unwrap(),
            path.to_string_lossy()
        );
        assert_eq!(terminal_directory(&path), path.parent().unwrap());
        assert_eq!(
            terminal_directory(path.parent().unwrap()),
            path.parent().unwrap()
        );
    }

    #[test]
    fn a_form_name_round_trips() {
        for form in PathForm::ALL {
            assert_eq!(PathForm::parse(form.as_str()), Some(form));
        }
        assert_eq!(PathForm::parse("nonsense"), None);
    }

    #[test]
    fn rendering_a_missing_file_falls_back_to_the_literal_path() {
        let path = Path::new("/definitely/not/here.txt");
        assert_eq!(
            render(path, PathForm::Canonical).unwrap(),
            "/definitely/not/here.txt"
        );
    }
}
