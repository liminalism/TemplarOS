//! MIME detection.
//!
//! `docs/architecture/associations.md`: *detected type beats extension*. [`SharedMimeInfoDetector`]
//! is the real detector: bounded content sniffing against a small built-in magic table, then
//! [`GlobsDb`] (a parser for shared-mime-info's `globs2` format) by filename, with `xdg-mime`
//! kept only as a last-resort fallback ([`FallbackDetector`]) for a system where `/usr/share/mime`
//! is not usable for some reason. The MIME directory is a constructor parameter so this is
//! testable against `tests/fixtures/mime/` — no real `/usr/share/mime` required.
//!
//! ## Why a built-in magic table instead of parsing `/usr/share/mime/magic`
//!
//! `magic` is a binary format with nested priority/mask sections; parsing it fully is a
//! significant chunk of work for a payoff this crate does not need. The acceptance bar this
//! project cares about — "a `.pdf` that is actually an executable is not treated as a PDF" — is
//! about detecting the handful of formats a desktop actually launches applications for (PDF,
//! the common image formats, archives) versus an ELF/shebang executable. A small ordered table
//! of magic byte sequences covers exactly that, is trivial to test, and never needs an external
//! parser to stay correct. If a broader set of formats ever matters, the natural next step is a
//! `magic`-file parser behind the same [`MimeDetector`] trait, not a rewrite of this module.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::{os::unix::fs::FileTypeExt, path::Path};
use tokio::{fs::File, io::AsyncReadExt, process::Command};

/// Something that can tell what a file actually is.
#[async_trait]
pub trait MimeDetector: Send + Sync {
    async fn detect(&self, path: &Path) -> Result<String>;
}

/// `xdg-mime query filetype`.
#[derive(Debug, Default, Clone, Copy)]
pub struct XdgMimeDetector;

#[async_trait]
impl MimeDetector for XdgMimeDetector {
    async fn detect(&self, path: &Path) -> Result<String> {
        let output = Command::new("xdg-mime")
            .arg("query")
            .arg("filetype")
            .arg(path)
            .output()
            .await
            .with_context(|| format!("detecting MIME type for {}", path.display()))?;

        if !output.status.success() {
            bail!("xdg-mime failed with {}", output.status);
        }

        let mime = String::from_utf8(output.stdout)?;
        let mime = mime.trim();
        if mime.is_empty() {
            return Ok("application/octet-stream".to_owned());
        }
        Ok(mime.to_owned())
    }
}

/// Fixed answers, for tests and dry runs.
#[derive(Debug, Clone)]
pub struct StaticDetector(pub String);

#[async_trait]
impl MimeDetector for StaticDetector {
    async fn detect(&self, _path: &Path) -> Result<String> {
        Ok(self.0.clone())
    }
}

/// How many bytes of a file content-sniffing reads. Generous enough for every magic prefix
/// below and for the text/binary heuristic, small enough to be a non-issue against a file of
/// any real size (a multi-gigabyte ISO costs one 4 KiB read, not a full scan).
const SNIFF_LIMIT: usize = 4096;

/// `(prefix, mime)`, checked in order — first match wins. Ordered so more specific/longer
/// prefixes are not shadowed by a shorter one that happens to come first.
const MAGIC_TABLE: &[(&[u8], &str)] = &[
    (b"%PDF-", "application/pdf"),
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"\x1f\x8b", "application/gzip"),
    (b"PK\x03\x04", "application/zip"),
    (b"PK\x05\x06", "application/zip"), // empty zip archive
    // ELF: the case that matters most — a `.pdf` that is actually a binary must not be treated
    // as a PDF regardless of what its extension claims.
    (b"\x7fELF", "application/x-executable"),
];

fn sniff_magic(bytes: &[u8]) -> Option<&'static str> {
    MAGIC_TABLE
        .iter()
        .find(|(prefix, _)| bytes.starts_with(prefix))
        .map(|(_, mime)| *mime)
}

/// `#!` scripts are executable text, not opaque binary — the shared-mime-info convention this
/// mirrors.
fn sniff_shebang(bytes: &[u8]) -> Option<&'static str> {
    bytes.starts_with(b"#!").then_some("text/x-shellscript")
}

/// No magic and no shebang matched: is this plausibly text, or should it fall back to the
/// generic binary type?
fn sniff_text_or_binary(bytes: &[u8]) -> &'static str {
    let sample = &bytes[..bytes.len().min(512)];
    let looks_binary = sample.contains(&0)
        || sample
            .iter()
            .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20))
            .count()
            * 20
            > sample.len(); // >5% control bytes outside \t\n\r
    if looks_binary {
        "application/octet-stream"
    } else {
        "text/plain"
    }
}

/// One weighted glob rule from a `globs2` file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GlobRule {
    weight: i64,
    mime: String,
    pattern: String,
    case_sensitive: bool,
}

/// A parsed shared-mime-info `globs2` database: `weight:mimetype:pattern[:cs]` per line.
#[derive(Clone, Debug, Default)]
pub struct GlobsDb {
    rules: Vec<GlobRule>,
}

impl GlobsDb {
    /// Parse `globs2` text (blank lines and `#` comments ignored, per the format).
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(4, ':');
            let (Some(weight), Some(mime), Some(pattern)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(weight) = weight.parse::<i64>() else {
                continue;
            };
            let case_sensitive = parts
                .next()
                .is_some_and(|flags| flags.split(',').any(|f| f == "cs"));
            rules.push(GlobRule {
                weight,
                mime: mime.to_owned(),
                pattern: pattern.to_owned(),
                case_sensitive,
            });
        }
        Self { rules }
    }

    /// Read `<mime_dir>/globs2`. A missing file yields an empty (never-matching) database —
    /// callers fall through to the text/binary heuristic, which is the right degraded behaviour
    /// on a system without shared-mime-info installed.
    pub fn load(mime_dir: &Path) -> Result<Self> {
        match std::fs::read_to_string(mime_dir.join("globs2")) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading {}/globs2", mime_dir.display()))
            }
        }
    }

    /// The highest-weight rule matching `file_name` (ties broken by the longer/more specific
    /// pattern, mirroring shared-mime-info's own tie-break).
    pub fn best_match(&self, file_name: &str) -> Option<&str> {
        self.rules
            .iter()
            .filter(|rule| glob_match(&rule.pattern, file_name, rule.case_sensitive))
            .max_by_key(|rule| (rule.weight, rule.pattern.len()))
            .map(|rule| rule.mime.as_str())
    }
}

/// A small `fnmatch`-style matcher: `*`, `?` and `[...]`/`[!...]` character classes — the glob
/// syntax shared-mime-info's `globs2` format actually uses.
fn glob_match(pattern: &str, text: &str, case_sensitive: bool) -> bool {
    let (pattern, text) = if case_sensitive {
        (pattern.to_owned(), text.to_owned())
    } else {
        (pattern.to_ascii_lowercase(), text.to_ascii_lowercase())
    };
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    matches(&pattern, &text)
}

fn matches(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|split| matches(&pattern[1..], &text[split..])),
        Some('?') => !text.is_empty() && matches(&pattern[1..], &text[1..]),
        Some('[') => {
            let Some(close) = pattern.iter().position(|&c| c == ']') else {
                return false; // malformed class: never matches, rather than panic.
            };
            let Some((&candidate, rest)) = text.split_first() else {
                return false;
            };
            let mut class = &pattern[1..close];
            let negate = class.first() == Some(&'!');
            if negate {
                class = &class[1..];
            }
            if class_contains(class, candidate) == negate {
                false
            } else {
                matches(&pattern[close + 1..], rest)
            }
        }
        Some(&expected) => text.first() == Some(&expected) && matches(&pattern[1..], &text[1..]),
    }
}

fn class_contains(class: &[char], candidate: char) -> bool {
    let mut i = 0;
    while i < class.len() {
        if i + 2 < class.len() && class[i + 1] == '-' {
            if (class[i]..=class[i + 2]).contains(&candidate) {
                return true;
            }
            i += 3;
        } else {
            if class[i] == candidate {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// The real detector: bounded content sniffing, then filename globs, then a text/binary guess.
#[derive(Clone, Debug)]
pub struct SharedMimeInfoDetector {
    globs: GlobsDb,
}

impl SharedMimeInfoDetector {
    /// Load rules from `mime_dir` (typically `/usr/share/mime`, or a fixture tree in tests).
    pub fn load(mime_dir: &Path) -> Result<Self> {
        Ok(Self {
            globs: GlobsDb::load(mime_dir)?,
        })
    }
}

/// The `inode/*` type for a non-regular, non-symlink file — never opened, only `stat`-ed.
/// This is what stops [`SharedMimeInfoDetector::detect`] from hanging on a FIFO with no writer
/// (or a character device, or blocking on a directory read): every path this project's callers
/// (`Associations1.ResolveFile`, `templar open`, the file-chooser portal, `templar-filed`) can be asked
/// to resolve is reachable by any session-bus peer, so this is not a hypothetical.
fn special_type_for(file_type: std::fs::FileType) -> Option<&'static str> {
    if file_type.is_dir() {
        Some("inode/directory")
    } else if file_type.is_fifo() {
        Some("inode/fifo")
    } else if file_type.is_socket() {
        Some("inode/socket")
    } else if file_type.is_block_device() {
        Some("inode/blockdevice")
    } else if file_type.is_char_device() {
        Some("inode/chardevice")
    } else {
        None
    }
}

#[async_trait]
impl MimeDetector for SharedMimeInfoDetector {
    async fn detect(&self, path: &Path) -> Result<String> {
        // `lstat`, never `open`: a FIFO with no writer blocks forever on open(), and a directory
        // opens successfully but fails the first read with EISDIR. Neither is reachable this way.
        let link_metadata = tokio::fs::symlink_metadata(path)
            .await
            .with_context(|| format!("stat-ing {} for MIME sniffing", path.display()))?;

        if let Some(mime) = special_type_for(link_metadata.file_type()) {
            return Ok(mime.to_owned());
        }

        if link_metadata.file_type().is_symlink() {
            // Follow exactly one resolution via another stat (never an open) so a symlink to a
            // real document still gets sniffed by content, while a symlink to a FIFO still
            // cannot hang this call, and a broken symlink gets its own honest answer.
            match tokio::fs::metadata(path).await {
                Ok(target_metadata) => {
                    if let Some(mime) = special_type_for(target_metadata.file_type()) {
                        return Ok(mime.to_owned());
                    }
                    // Target is a regular file: fall through and sniff the original path, which
                    // `open()` will transparently resolve through the symlink to that file.
                }
                Err(_) => return Ok("inode/symlink".to_owned()), // broken symlink
            }
        }

        let bytes = read_prefix(path, SNIFF_LIMIT)
            .await
            .with_context(|| format!("reading {} for MIME sniffing", path.display()))?;

        // shared-mime-info's own answer for an empty regular file, rather than letting the
        // text/binary heuristic call it "text/plain" by default.
        if bytes.is_empty() {
            return Ok("application/x-zerosize".to_owned());
        }

        if let Some(mime) = sniff_magic(&bytes) {
            return Ok(mime.to_owned());
        }

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if let Some(mime) = self.globs.best_match(file_name) {
            return Ok(mime.to_owned());
        }

        if let Some(mime) = sniff_shebang(&bytes) {
            return Ok(mime.to_owned());
        }

        Ok(sniff_text_or_binary(&bytes).to_owned())
    }
}

async fn read_prefix(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = File::open(path).await?;
    let mut buffer = vec![0_u8; limit];
    let mut total = 0;
    loop {
        let read = file.read(&mut buffer[total..]).await?;
        if read == 0 {
            break;
        }
        total += read;
        if total == buffer.len() {
            break;
        }
    }
    buffer.truncate(total);
    Ok(buffer)
}

/// Try `primary`; on any error, fall back to `fallback` (the documented last resort: `xdg-mime`).
#[derive(Debug, Clone)]
pub struct FallbackDetector<P, F> {
    primary: P,
    fallback: F,
}

impl<P, F> FallbackDetector<P, F> {
    pub fn new(primary: P, fallback: F) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl<P: MimeDetector, F: MimeDetector> MimeDetector for FallbackDetector<P, F> {
    async fn detect(&self, path: &Path) -> Result<String> {
        match self.primary.detect(path).await {
            Ok(mime) => Ok(mime),
            Err(_) => self.fallback.detect(path).await,
        }
    }
}

/// The fixture MIME directory tests point `SharedMimeInfoDetector` at.
#[cfg(test)]
fn fixture_mime_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mime")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs2_parses_weight_mime_pattern_and_case_flag() {
        let db = GlobsDb::parse(
            "50:application/pdf:*.pdf\n# comment\n\n60:application/x-compressed-tar:*.tar.gz\n50:text/x-csrc:*.C:cs\n",
        );
        assert_eq!(db.best_match("report.pdf"), Some("application/pdf"));
        assert_eq!(
            db.best_match("archive.tar.gz"),
            Some("application/x-compressed-tar")
        );
        assert_eq!(
            db.best_match("thing.c"),
            None,
            "cs flag: lowercase .c must not match *.C"
        );
        assert_eq!(db.best_match("thing.C"), Some("text/x-csrc"));
        assert_eq!(db.best_match("no-extension"), None);
    }

    #[test]
    fn higher_weight_wins_over_more_generic_rule() {
        let db =
            GlobsDb::parse("50:application/gzip:*.gz\n60:application/x-compressed-tar:*.tar.gz\n");
        assert_eq!(
            db.best_match("archive.tar.gz"),
            Some("application/x-compressed-tar")
        );
    }

    #[test]
    fn glob_classes_and_wildcards() {
        assert!(glob_match("core.[0-9]*", "core.123", true));
        assert!(!glob_match("core.[0-9]*", "core.abc", true));
        assert!(glob_match("*.[Cc]", "x.C", true));
        assert!(glob_match("*.[Cc]", "x.c", true));
        assert!(glob_match("file?.txt", "file1.txt", true));
        assert!(!glob_match("file?.txt", "file12.txt", true));
    }

    #[test]
    fn magic_beats_a_misleading_extension() {
        let elf = b"\x7fELF\x02\x01\x01\x00rest-of-a-binary";
        assert_eq!(sniff_magic(elf), Some("application/x-executable"));
        let pdf = b"%PDF-1.7\n...";
        assert_eq!(sniff_magic(pdf), Some("application/pdf"));
        assert_eq!(sniff_magic(b"not any known magic"), None);
    }

    #[test]
    fn text_vs_binary_heuristic() {
        assert_eq!(sniff_text_or_binary(b"hello\nworld\n"), "text/plain");
        assert_eq!(
            sniff_text_or_binary(b"\x00\x01\x02\x03binary garbage"),
            "application/octet-stream"
        );
        assert_eq!(
            sniff_shebang(b"#!/bin/sh\necho hi\n"),
            Some("text/x-shellscript")
        );
    }

    #[tokio::test]
    async fn a_pdf_named_executable_is_not_treated_as_a_pdf_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();

        let disguised = dir.path().join("invoice.pdf");
        std::fs::write(&disguised, b"\x7fELF\x02\x01\x01\x00rest").unwrap();
        assert_eq!(
            detector.detect(&disguised).await.unwrap(),
            "application/x-executable"
        );

        let real_pdf_wrong_name = dir.path().join("data.bin");
        std::fs::write(&real_pdf_wrong_name, b"%PDF-1.4\n%...").unwrap();
        assert_eq!(
            detector.detect(&real_pdf_wrong_name).await.unwrap(),
            "application/pdf"
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_extension_glob_when_content_is_generic() {
        let dir = tempfile::tempdir().unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        let path = dir.path().join("script.py");
        std::fs::write(&path, b"print('hello')\n").unwrap();
        assert_eq!(detector.detect(&path).await.unwrap(), "text/x-python");
    }

    #[tokio::test]
    async fn missing_mime_dir_degrades_to_the_text_binary_heuristic() {
        let dir = tempfile::tempdir().unwrap();
        let detector = SharedMimeInfoDetector::load(&dir.path().join("does-not-exist")).unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"just some text").unwrap();
        assert_eq!(detector.detect(&path).await.unwrap(), "text/plain");
    }

    #[tokio::test]
    async fn a_directory_is_answered_without_being_opened() {
        let dir = tempfile::tempdir().unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        let subdir = dir.path().join("a-directory");
        std::fs::create_dir(&subdir).unwrap();
        assert_eq!(detector.detect(&subdir).await.unwrap(), "inode/directory");
    }

    #[tokio::test]
    async fn a_fifo_is_answered_without_ever_being_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("a-fifo");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        let Ok(status) = status else {
            eprintln!("skipping: mkfifo is not available in this environment");
            return;
        };
        if !status.success() {
            eprintln!("skipping: mkfifo failed");
            return;
        }

        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        // With no writer, opening this FIFO for reading would block forever; a bounded timeout
        // proves detect() never tries to open it at all.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), detector.detect(&fifo)).await;
        assert_eq!(result.expect("must not hang").unwrap(), "inode/fifo");
    }

    #[tokio::test]
    async fn a_unix_socket_is_answered_without_being_opened() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("a-socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        assert_eq!(detector.detect(&socket_path).await.unwrap(), "inode/socket");
    }

    #[tokio::test]
    async fn a_broken_symlink_is_its_own_answer() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("does-not-exist"), &link).unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        assert_eq!(detector.detect(&link).await.unwrap(), "inode/symlink");
    }

    #[tokio::test]
    async fn a_symlink_to_a_regular_file_is_sniffed_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("report.pdf");
        std::fs::write(&target, b"%PDF-1.4\n%...").unwrap();
        let link = dir.path().join("link-to-report");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        assert_eq!(detector.detect(&link).await.unwrap(), "application/pdf");
    }

    #[tokio::test]
    async fn a_symlink_to_a_fifo_still_cannot_hang() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("target-fifo");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        let Ok(status) = status else {
            eprintln!("skipping: mkfifo is not available in this environment");
            return;
        };
        if !status.success() {
            eprintln!("skipping: mkfifo failed");
            return;
        }
        let link = dir.path().join("link-to-fifo");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();

        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), detector.detect(&link)).await;
        assert_eq!(result.expect("must not hang").unwrap(), "inode/fifo");
    }

    #[tokio::test]
    async fn an_empty_regular_file_is_zerosize_not_text_plain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, b"").unwrap();
        let detector = SharedMimeInfoDetector::load(&fixture_mime_dir()).unwrap();
        assert_eq!(
            detector.detect(&path).await.unwrap(),
            "application/x-zerosize"
        );
    }

    #[tokio::test]
    async fn fallback_detector_uses_the_secondary_on_primary_failure() {
        struct AlwaysFails;
        #[async_trait]
        impl MimeDetector for AlwaysFails {
            async fn detect(&self, _path: &Path) -> Result<String> {
                bail!("nope")
            }
        }
        let detector =
            FallbackDetector::new(AlwaysFails, StaticDetector("application/x-fallback".into()));
        assert_eq!(
            detector.detect(Path::new("/nonexistent")).await.unwrap(),
            "application/x-fallback"
        );
    }
}
