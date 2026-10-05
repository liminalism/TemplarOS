use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};

fn windows_drive(component: &[u8]) -> bool {
    component.len() >= 2 && component[0].is_ascii_alphabetic() && component[1] == b':'
}

/// Native pathname bytes are retained, including non-UTF-8 Unix filenames.
/// Backslashes in archive metadata are interpreted as Windows separators.
pub(crate) fn member_path(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    ensure!(!bytes.is_empty(), "Archive entry has an empty pathname");
    ensure!(
        !matches!(bytes[0], b'/' | b'\\'),
        "Absolute archive pathname is unsafe"
    );
    ensure!(!bytes.contains(&0), "Archive pathname contains NUL");
    let mut path = PathBuf::new();
    for component in bytes.split(|byte| matches!(byte, b'/' | b'\\')) {
        if component.is_empty() || component == b"." {
            continue;
        }
        ensure!(
            component != b"..",
            "Archive pathname contains parent traversal"
        );
        ensure!(
            !windows_drive(component),
            "Windows drive pathname is unsafe"
        );
        path.push(OsStr::from_bytes(component));
    }
    Ok(path)
}

pub(crate) fn link_target(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    ensure!(!bytes.is_empty(), "Empty symbolic link target");
    ensure!(
        !matches!(bytes[0], b'/' | b'\\'),
        "Absolute symbolic link target is unsafe"
    );
    ensure!(!bytes.contains(&0), "Symbolic link target contains NUL");
    let mut normalized = Vec::with_capacity(bytes.len());
    for component in bytes.split(|byte| matches!(byte, b'/' | b'\\')) {
        ensure!(
            !windows_drive(component),
            "Windows drive link target is unsafe"
        );
        if !normalized.is_empty() {
            normalized.push(b'/');
        }
        normalized.extend_from_slice(component);
    }
    Ok(PathBuf::from(OsString::from_vec(normalized)))
}

/// Resolve links as a filesystem would, not merely by lexically normalizing the
/// target. In particular `safe_link/../../outside` must expand safe_link first.
/// Links are created only after all non-link members have been written.
pub(crate) fn resolved_links(
    links: &BTreeMap<PathBuf, PathBuf>,
) -> anyhow::Result<BTreeMap<PathBuf, PathBuf>> {
    let mut outputs = BTreeMap::new();
    for (path, target) in links {
        let mut resolved: Vec<OsString> = path
            .parent()
            .unwrap_or(Path::new(""))
            .iter()
            .map(OsStr::to_os_string)
            .collect();
        let mut pending = components(target);
        let mut expansions = 0;
        while let Some(component) = pending.pop_front() {
            match component.as_bytes() {
                b"" | b"." => {}
                b".." => {
                    ensure!(
                        resolved.pop().is_some(),
                        "Symbolic link escapes extraction root: {}",
                        path.display()
                    );
                }
                _ => {
                    resolved.push(component);
                    let current: PathBuf = resolved.iter().collect();
                    if let Some(next) = links.get(&current) {
                        resolved.pop();
                        expansions += 1;
                        ensure!(
                            expansions <= 1024,
                            "Cyclic or excessively deep symbolic link: {}",
                            path.display()
                        );
                        let mut replacement = components(next);
                        replacement.append(&mut pending);
                        pending = replacement;
                    }
                }
            }
        }
        outputs.insert(path.clone(), resolved.iter().collect());
    }
    Ok(outputs)
}

fn components(path: &Path) -> VecDeque<OsString> {
    // Path::components normalizes interior '.' but retains '..'; direct splitting
    // also preserves all link traversal steps for the resolver above.
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .map(|part| OsStr::from_bytes(part).to_os_string())
        .collect()
}

pub(crate) fn zip_name(path: &Path) -> anyhow::Result<&str> {
    let name = path
        .to_str()
        .context("ZIP creation requires UTF-8 filenames")?;
    ensure!(
        !name.contains('\\'),
        "ZIP member names containing backslashes are not portable: {name}"
    );
    let normalized = member_path(name.as_bytes())?;
    ensure!(
        normalized == path && !normalized.as_os_str().is_empty(),
        "Unsafe ZIP member name: {name}"
    );
    Ok(name)
}

/// Resolve only the parent, so a selected source symlink is never dereferenced.
pub(crate) fn absolute_leaf(path: &Path) -> anyhow::Result<PathBuf> {
    let mut absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let last = absolute
        .as_os_str()
        .as_bytes()
        .rsplit(|byte| *byte == b'/')
        .find(|part| !part.is_empty());
    if last.is_some_and(|last| last == b"." || last == b"..") {
        absolute = absolute.canonicalize()?;
    }
    if absolute.file_name().is_none() {
        bail!("A filesystem root cannot be selected or used as an archive output");
    }
    let parent = absolute
        .parent()
        .context("Path has no parent")?
        .canonicalize()
        .with_context(|| format!("Cannot access parent of {}", path.display()))?;
    Ok(parent.join(absolute.file_name().expect("checked above")))
}

pub(crate) fn archive_stem(path: &Path) -> OsString {
    strip_suffix(path.file_name().unwrap_or(OsStr::new("Archive")), true)
}

pub(crate) fn raw_name(path: &Path) -> OsString {
    let name = path.file_name().unwrap_or(OsStr::new("data"));
    let stripped = strip_suffix(name, false);
    if stripped == name {
        let mut result = name.to_os_string();
        result.push(".out");
        result
    } else {
        stripped
    }
}

fn strip_suffix(name: &OsStr, compound: bool) -> OsString {
    let bytes = name.as_bytes();
    let compounds: &[&[u8]] = &[
        b".tar.bz2",
        b".tar.zstd",
        b".tar.lzma",
        b".tar.gz",
        b".tar.xz",
        b".tar.zst",
        b".tar.lz4",
        b".tar.lzo",
        b".tar.lz",
        b".tar.bz",
        b".tar.z",
        b".tgz",
        b".tbz2",
        b".tbz",
        b".txz",
        b".tlz",
        b".tzst",
        b".taz",
    ];
    let archives: &[&[u8]] = &[
        b".zip", b".7z", b".rar", b".tar", b".cab", b".iso", b".deb", b".ar", b".cpio", b".lha",
        b".lzh", b".xar", b".warc", b".a", b".rpm",
    ];
    let compressors: &[&[u8]] = &[
        b".bz2", b".zstd", b".lzma", b".gz", b".xz", b".lz4", b".zst", b".lzo", b".lz", b".bz",
        b".z",
    ];
    let suffix = if compound {
        compounds
            .iter()
            .chain(archives)
            .chain(compressors)
            .find(|suffix| bytes.ends_with_ignore_ascii_case(suffix))
    } else {
        compressors
            .iter()
            .find(|suffix| bytes.ends_with_ignore_ascii_case(suffix))
    };
    let mut stem = suffix.map_or(bytes, |suffix| &bytes[..bytes.len() - suffix.len()]);
    if compound
        && suffix.is_some_and(|suffix| compressors.contains(suffix))
        && let Some(archive) = archives
            .iter()
            .find(|suffix| stem.ends_with_ignore_ascii_case(suffix))
    {
        stem = &stem[..stem.len() - archive.len()];
    }
    if stem.is_empty() || stem == b"." || stem == b".." {
        OsString::from("Archive")
    } else {
        OsStr::from_bytes(stem).to_os_string()
    }
}

trait ByteSuffix {
    fn ends_with_ignore_ascii_case(&self, suffix: &[u8]) -> bool;
}

impl ByteSuffix for [u8] {
    fn ends_with_ignore_ascii_case(&self, suffix: &[u8]) -> bool {
        self.len() >= suffix.len() && self[self.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    }
}
