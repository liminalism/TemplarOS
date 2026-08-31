//! `text/uri-list` (RFC 2483 §5): the interchange format every desktop already speaks.
//!
//! This is what makes copy, paste and drag work *between* LegeOS and everything else. GTK, Qt,
//! Chromium and every file manager on Linux put `text/uri-list` on the clipboard and offer it as
//! a drag-and-drop MIME type, so encoding and parsing it correctly is the whole of the
//! interoperation problem — the transport underneath (`wl_data_device`) carries bytes and does
//! not care what they mean.
//!
//! Pure functions over `&str` and `&[PathBuf]`, deliberately: this is the part that can be got
//! exactly right and tested exhaustively without a compositor, a clipboard or a second
//! application, and every awkward case (a space, a `#`, a newline, a non-UTF-8 byte, CRLF versus
//! LF, a trailing blank line, a `file:` URI with a host) is a case a real desktop will hand us.
//!
//! Two rules from the specification that are easy to get wrong and matter here:
//!
//! * Lines are separated by CRLF, and a line beginning with `#` is a comment, not a file.
//! * The URI is percent-encoded. A file named `my report #2.txt` is not a comment and does not
//!   have a fragment; it is one path whose `#` and spaces are escaped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The MIME type this module reads and writes.
pub const MIME_TYPE: &str = "text/uri-list";

/// Encode absolute paths as a `text/uri-list` payload.
///
/// CRLF-terminated including the last line, which is what the specification asks for and what
/// the strictest readers expect. Relative paths are skipped rather than guessed at: a `file:`
/// URI is absolute by definition, and resolving one against a working directory the receiving
/// application does not share would produce a URI that points somewhere real and wrong.
pub fn encode(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        if !path.is_absolute() {
            continue;
        }
        out.push_str(&encode_path(path));
        out.push_str("\r\n");
    }
    out
}

/// One absolute path as a `file://` URI.
pub fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        // The unreserved set of RFC 3986 plus the sub-delims that are safe in a path, plus `/`
        // itself so the URI stays readable. Everything else — space, `#`, `?`, `%`, every
        // non-ASCII byte — is escaped.
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(*byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Parse a `text/uri-list` payload into the local paths it names.
///
/// Tolerant on the way in and strict on the way out, which is the right way round for a format
/// arriving from another application: lines may be separated by CRLF or bare LF, blank lines and
/// `#` comments are ignored, and a URI this machine cannot turn into a local path — an `http:`
/// URL, a `file://otherhost/...` on someone else's machine — is skipped rather than mangled into
/// a path that would name the wrong thing locally.
pub fn decode(payload: &str) -> Vec<PathBuf> {
    payload
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(decode_uri)
        .collect()
}

/// One `file:` URI as a local path, or `None` when it does not name one.
pub fn decode_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://").or_else(|| {
        // `file:/path` and `file:///path` are both seen in the wild; the first is legal and the
        // second is what everything actually emits.
        uri.strip_prefix("file:")
    })?;
    // `file://host/path`: an empty host (or `localhost`) is this machine and the rest is the
    // path. Any other host names a file that is not here.
    let path = if rest.starts_with('/') {
        rest
    } else {
        let (host, path) = rest.split_once('/')?;
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        // `split_once` ate the leading slash.
        return percent_decode(&format!("/{path}"));
    };
    percent_decode(path)
}

/// Percent-decode into a path, keeping bytes rather than characters: a filename is a byte string
/// on Linux and need not be valid UTF-8, and a lossy conversion here would silently rename it.
fn percent_decode(value: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(out));
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_uri_list_round_trips_ordinary_paths() {
        let paths = vec![
            PathBuf::from("/home/u/report.pdf"),
            PathBuf::from("/home/u/Documents/notes.md"),
        ];
        let payload = encode(&paths);
        assert_eq!(
            payload,
            "file:///home/u/report.pdf\r\nfile:///home/u/Documents/notes.md\r\n"
        );
        assert_eq!(decode(&payload), paths);
    }

    #[test]
    fn clipboard_uri_list_escapes_the_characters_that_would_change_its_meaning() {
        // A `#` in a filename is not a comment and a space is not a separator. Getting either
        // wrong loses the file silently, which is the worst possible failure for a paste.
        let paths = vec![
            PathBuf::from("/home/u/my report #2.txt"),
            PathBuf::from("/home/u/100% done.txt"),
            PathBuf::from("/home/u/a\nb.txt"),
        ];
        let payload = encode(&paths);
        assert!(payload.contains("my%20report%20%232.txt"));
        assert!(payload.contains("100%25%20done.txt"));
        assert!(
            !payload.contains("\n\n"),
            "a newline in a name must be escaped"
        );
        assert_eq!(payload.lines().count(), 3);
        assert_eq!(decode(&payload), paths);
    }

    #[test]
    fn clipboard_uri_list_survives_a_non_utf8_filename() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        // A perfectly legal Linux filename that is not valid UTF-8. A lossy decode would rename
        // it to something with a replacement character, and the paste would miss.
        let path = PathBuf::from(OsString::from_vec(b"/home/u/caf\xe9.txt".to_vec()));
        let payload = encode(std::slice::from_ref(&path));
        assert_eq!(payload, "file:///home/u/caf%E9.txt\r\n");
        assert_eq!(decode(&payload), vec![path]);
    }

    #[test]
    fn clipboard_uri_list_reads_what_other_applications_actually_send() {
        // Bare LF (some senders), a comment line, a blank line, a trailing space, and the
        // `localhost` spelling GTK still emits in places.
        let payload = "# some comment\n\
                       file:///home/u/one.txt\n\
                       \n\
                       file://localhost/home/u/two.txt \r\n\
                       file:/home/u/three.txt\r\n";
        assert_eq!(
            decode(payload),
            vec![
                PathBuf::from("/home/u/one.txt"),
                PathBuf::from("/home/u/two.txt"),
                PathBuf::from("/home/u/three.txt"),
            ]
        );
    }

    #[test]
    fn clipboard_uri_list_skips_what_is_not_a_local_file_rather_than_inventing_a_path() {
        let payload = "https://example.com/page.html\r\n\
                       file://otherhost/home/u/theirs.txt\r\n\
                       mailto:someone@example.com\r\n\
                       file:///home/u/mine.txt\r\n\
                       file:relative.txt\r\n";
        assert_eq!(decode(payload), vec![PathBuf::from("/home/u/mine.txt")]);
    }

    #[test]
    fn clipboard_uri_list_refuses_to_emit_a_relative_path() {
        // A `file:` URI is absolute by definition; resolving one against a working directory the
        // receiving application does not share would name something real and wrong.
        let payload = encode(&[
            PathBuf::from("relative/notes.md"),
            PathBuf::from("/home/u/absolute.md"),
        ]);
        assert_eq!(payload, "file:///home/u/absolute.md\r\n");
    }

    #[test]
    fn clipboard_uri_list_of_nothing_is_empty_not_a_blank_line() {
        assert_eq!(encode(&[]), "");
        assert!(decode("").is_empty());
        assert!(decode("\r\n\r\n").is_empty());
        assert!(decode("# only a comment\r\n").is_empty());
    }
}
