use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, ensure};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::destination::{Destination, fd_path, safe_leaf_name};
use crate::paths::{absolute_leaf, link_target, resolved_links, zip_name};
use crate::{BUFFER_SIZE, Progress, check_cancel};

pub fn compress(
    paths: &[PathBuf],
    output: Option<&Path>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<PathBuf> {
    ensure!(
        !paths.is_empty(),
        "Select at least one file or directory to compress"
    );
    check_cancel(cancel)?;
    let roots: Vec<PathBuf> = paths
        .iter()
        .map(|path| absolute_leaf(path))
        .collect::<anyhow::Result<_>>()?;
    let mut names = BTreeSet::new();
    for (index, path) in roots.iter().enumerate() {
        let name = path.file_name().context("Selected path has no filename")?;
        zip_name(Path::new(name))?;
        ensure!(
            names.insert(name.to_os_string()),
            "Selected roots produce duplicate ZIP member names: {}",
            name.to_string_lossy()
        );
        for previous in &roots[..index] {
            ensure!(
                !path.starts_with(previous) && !previous.starts_with(path),
                "Selected roots overlap: {} and {}",
                previous.display(),
                path.display()
            );
        }
    }
    let desired = if let Some(output) = output {
        absolute_leaf(output)?
    } else {
        let mut name = if roots.len() == 1 {
            roots[0].file_name().expect("validated root").to_os_string()
        } else {
            "Archive".into()
        };
        name.push(".zip");
        roots[0].parent().expect("absolute root parent").join(name)
    };
    let name = desired
        .file_name()
        .context("Archive output has no filename")?;
    safe_leaf_name(name)?;
    let destination = Destination::open(desired.parent().context("Archive output has no parent")?)?;
    // Temporary ZIP output also lives in this parent. Reject a destination under
    // any selected directory rather than accidentally including our own stream.
    for root in &roots {
        ensure!(
            !destination.path.starts_with(root),
            "Archive output would include itself inside selected source: {}",
            root.display()
        );
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".templar-zip-")
        .tempfile_in(destination.anchored())?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut links = BTreeMap::new();
    {
        let mut writer = ZipWriter::new(BufWriter::with_capacity(
            BUFFER_SIZE,
            temporary.as_file_mut(),
        ));
        for root in &roots {
            let member = PathBuf::from(root.file_name().expect("validated root"));
            let parent = Destination::open(root.parent().expect("absolute root parent"))?;
            let source = parent
                .anchored()
                .join(root.file_name().expect("validated root"));
            write_member(
                &source,
                &member,
                &mut writer,
                &mut buffer,
                &mut links,
                cancel,
                progress,
            )
            .with_context(|| format!("Cannot compress {}", root.display()))?;
        }
        for (path, target) in resolved_links(&links)? {
            let top = target
                .iter()
                .next()
                .context("Source symbolic link points outside selected archive members")?;
            ensure!(
                names.contains(top),
                "Source symbolic link targets an unselected tree: {}",
                path.display()
            );
        }
        check_cancel(cancel)?;
        let buffered = writer
            .finish()
            .context("Cannot finish ZIP central directory")?;
        let file = buffered
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)
            .context("Cannot flush ZIP output")?;
        file.sync_all().context("Cannot sync ZIP output")?;
    }
    check_cancel(cancel)?;
    destination.publish_unique(temporary.path(), name, true)
}

fn write_member<W: Write + std::io::Seek>(
    source: &Path,
    member: &Path,
    writer: &mut ZipWriter<W>,
    buffer: &mut [u8],
    links: &mut BTreeMap<PathBuf, PathBuf>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<()> {
    check_cancel(cancel)?;
    let name = zip_name(member)?;
    let metadata = fs::symlink_metadata(source)?;
    progress(Progress::Entry(member.to_path_buf()));
    check_cancel(cancel)?;
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(metadata.mode() & 0o777);
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        ensure!(
            !target.as_os_str().as_bytes().contains(&b'\\'),
            "Source symbolic link contains a non-portable backslash"
        );
        let target = link_target(target.as_os_str().as_bytes())?;
        let text = target
            .to_str()
            .context("ZIP symbolic link targets must be UTF-8")?;
        links.insert(member.to_path_buf(), target.clone());
        writer.add_symlink(name, text, options)?;
    } else if metadata.is_dir() {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(source)?;
        ensure_same_file(&metadata, &directory.metadata()?)?;
        writer.add_directory(name, options)?;
        // Holding this descriptor pins the source directory. Child opens are
        // relative to it and O_NOFOLLOW, so a concurrent directory replacement
        // cannot redirect traversal through a source symlink outside the tree.
        let anchored = fd_path(&directory);
        for child in fs::read_dir(&anchored)? {
            check_cancel(cancel)?;
            let child = child?;
            let filename = child.file_name();
            write_member(
                &anchored.join(&filename),
                &member.join(filename),
                writer,
                buffer,
                links,
                cancel,
                progress,
            )?;
        }
    } else if metadata.is_file() {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)?;
        let opened = file.metadata()?;
        ensure!(
            opened.is_file(),
            "Source changed to a special file during compression"
        );
        ensure_same_file(&metadata, &opened)?;
        writer.start_file(
            name,
            options.large_file(opened.len() >= u64::from(u32::MAX)),
        )?;
        loop {
            check_cancel(cancel)?;
            let count = file.read(buffer)?;
            if count == 0 {
                break;
            }
            writer.write_all(&buffer[..count])?;
            progress(Progress::Bytes(count as u64));
        }
    } else {
        anyhow::bail!(
            "Special files cannot be added to a ZIP: {}",
            member.display()
        );
    }
    Ok(())
}

fn ensure_same_file(before: &fs::Metadata, after: &fs::Metadata) -> anyhow::Result<()> {
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino(),
        "Source changed while opening it; no archive was published"
    );
    Ok(())
}
