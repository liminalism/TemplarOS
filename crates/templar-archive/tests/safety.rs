use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;
use templar_archive::{Cancelled, ExtractFailure, ExtractMode, Progress, compress, extract};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

fn zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut writer = ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in entries {
        writer
            .start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap();
}

struct TarEntry<'a> {
    name: &'a [u8],
    kind: u8,
    target: &'a [u8],
    data: &'a [u8],
    mode: u32,
}

fn regular<'a>(name: &'a [u8], data: &'a [u8]) -> TarEntry<'a> {
    TarEntry {
        name,
        kind: b'0',
        target: b"",
        data,
        mode: 0o644,
    }
}

fn link<'a>(name: &'a [u8], target: &'a [u8], kind: u8) -> TarEntry<'a> {
    TarEntry {
        name,
        kind,
        target,
        data: b"",
        mode: 0o777,
    }
}

fn directory(name: &[u8], mode: u32) -> TarEntry<'_> {
    TarEntry {
        name,
        kind: b'5',
        target: b"",
        data: b"",
        mode,
    }
}

fn tar(path: &Path, entries: &[TarEntry<'_>]) {
    let mut file = fs::File::create(path).unwrap();
    for entry in entries {
        let mut header = [0_u8; 512];
        header[..entry.name.len()].copy_from_slice(entry.name);
        header[100..108].copy_from_slice(format!("{:07o}\0", entry.mode).as_bytes());
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", entry.data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = entry.kind;
        header[157..157 + entry.target.len()].copy_from_slice(entry.target);
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        file.write_all(&header).unwrap();
        file.write_all(entry.data).unwrap();
        let padding = (512 - entry.data.len() % 512) % 512;
        file.write_all(&vec![0; padding]).unwrap();
    }
    file.write_all(&[0; 1024]).unwrap();
}

fn cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(<dyn std::error::Error>::is::<Cancelled>)
}

fn no_staging(directory: &Path) {
    for entry in fs::read_dir(directory).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(
            !name.to_string_lossy().starts_with(".templar-"),
            "Leaked private staging output: {}",
            name.display()
        );
    }
}

#[test]
fn multifile_deflate_zip_preserves_empty_directories_and_safe_links() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("source");
    fs::create_dir_all(directory.join("empty")).unwrap();
    fs::write(directory.join("file.txt"), b"file contents").unwrap();
    fs::write(directory.join("日本語.txt"), b"UTF-8 ZIP filename").unwrap();
    symlink("file.txt", directory.join("link")).unwrap();
    let other = temporary.path().join("other.txt");
    fs::write(&other, b"other contents").unwrap();
    let cancel = AtomicBool::new(false);
    let archive = compress(&[directory, other], None, &cancel, &mut |_| {}).unwrap();
    let mut reader = ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
    assert_eq!(
        reader.by_name("source/file.txt").unwrap().compression(),
        CompressionMethod::Deflated
    );
    assert!(reader.by_name("source/empty/").unwrap().is_dir());
    assert!(reader.by_name("other.txt").is_ok());
    drop(reader);
    let outputs = extract(&[archive], &ExtractMode::Smart, None, &cancel, &mut |_| {}).unwrap();
    assert_eq!(outputs, vec![temporary.path().join("Archive")]);
    assert_eq!(
        fs::read(outputs[0].join("source/link")).unwrap(),
        b"file contents"
    );
    assert_eq!(
        fs::read(outputs[0].join("source/日本語.txt")).unwrap(),
        b"UTF-8 ZIP filename"
    );
    assert!(outputs[0].join("source/empty").is_dir());
    assert_eq!(
        fs::read(outputs[0].join("other.txt")).unwrap(),
        b"other contents"
    );
    no_staging(temporary.path());
}

#[test]
fn smart_uses_archive_stem_flattens_directory_and_rewrites_root_relative_links() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("chosen.tar");
    tar(
        &archive,
        &[
            directory(b"unrelated/", 0o755),
            regular(b"unrelated/file", b"contents"),
            link(b"unrelated/link", b"../unrelated/file", b'2'),
            directory(b"unrelated/child/", 0o755),
            link(b"unrelated/child/relative", b"../file", b'2'),
        ],
    );
    let outputs = extract(
        std::slice::from_ref(&archive),
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outputs, vec![temporary.path().join("chosen")]);
    assert!(!outputs[0].join("unrelated").exists());
    assert_eq!(fs::read(outputs[0].join("link")).unwrap(), b"contents");
    assert_eq!(
        fs::read(outputs[0].join("child/relative")).unwrap(),
        b"contents"
    );
    let second = extract(
        &[archive],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(second, vec![temporary.path().join("chosen (1)")]);
}

#[test]
fn archive_member_traversal_is_rejected_before_any_publication() {
    for bad in [
        "../escaped",
        "/escaped",
        "C:\\escaped",
        "a\\..\\escaped",
        "C:escaped",
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::create_dir(&target).unwrap();
        let archive = temporary.path().join("unsafe.zip");
        zip(&archive, &[("otherwise-valid", b"data"), (bad, b"bad")]);
        assert!(
            extract(
                &[archive],
                &ExtractMode::Directory(target.clone()),
                None,
                &AtomicBool::new(false),
                &mut |_| {}
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert!(!temporary.path().join("escaped").exists());
    }
}

#[test]
fn links_cannot_escape_or_redirect_later_members() {
    for entries in [
        vec![
            regular(b"root/file", b"good"),
            link(b"root/link", b"../../outside", b'2'),
        ],
        vec![
            link(b"root/pivot", b".", b'2'),
            link(b"root/link", b"pivot/../../outside", b'2'),
        ],
        vec![
            link(b"root/pivot", b"child", b'2'),
            regular(b"root/pivot/file", b"bad"),
        ],
        vec![
            regular(b"root/file", b"good"),
            link(b"root/link", b"../outside", b'2'),
        ],
        vec![
            regular(b"root/file", b"good"),
            link(b"root/link", b"C:\\outside", b'2'),
        ],
        vec![
            link(b"root/one", b"two", b'2'),
            link(b"root/two", b"one", b'2'),
        ],
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("unsafe.tar");
        tar(&archive, &entries);
        assert!(
            extract(
                &[archive],
                &ExtractMode::Smart,
                None,
                &AtomicBool::new(false),
                &mut |_| {}
            )
            .is_err()
        );
        assert!(!temporary.path().join("unsafe").exists());
        no_staging(temporary.path());
    }
}

#[test]
fn existing_top_level_output_and_dangling_symlinks_are_never_overwritten() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("input.zip");
    zip(&archive, &[("existing", b"replacement"), ("new", b"new")]);
    fs::write(temporary.path().join("existing"), b"original").unwrap();
    assert!(
        extract(
            std::slice::from_ref(&archive),
            &ExtractMode::Here,
            None,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(
        fs::read(temporary.path().join("existing")).unwrap(),
        b"original"
    );
    assert!(!temporary.path().join("new").exists());
    fs::remove_file(temporary.path().join("existing")).unwrap();
    symlink("missing", temporary.path().join("existing")).unwrap();
    assert!(
        extract(
            &[archive],
            &ExtractMode::Here,
            None,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(
        fs::read_link(temporary.path().join("existing")).unwrap(),
        PathBuf::from("missing")
    );
    no_staging(temporary.path());
}

#[test]
fn zip_collision_numbers_and_self_inclusion_and_overlap_are_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("tree");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"contents").unwrap();
    let output = temporary.path().join("output.zip");
    fs::write(&output, b"leave alone").unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        compress(
            std::slice::from_ref(&source),
            Some(&output),
            &cancel,
            &mut |_| {}
        )
        .unwrap(),
        temporary.path().join("output (1).zip")
    );
    assert_eq!(fs::read(&output).unwrap(), b"leave alone");
    assert!(
        compress(
            std::slice::from_ref(&source),
            Some(&source.join("inside.zip")),
            &cancel,
            &mut |_| {}
        )
        .is_err()
    );
    assert!(
        compress(
            &[source.clone(), source.join("file")],
            None,
            &cancel,
            &mut |_| {}
        )
        .is_err()
    );
    let outside = temporary.path().join("outside");
    fs::write(&outside, b"secret").unwrap();
    symlink("../outside", source.join("escaping")).unwrap();
    assert!(compress(&[source], None, &cancel, &mut |_| {}).is_err());
    assert!(!temporary.path().join("tree.zip").exists());
    no_staging(temporary.path());
}

#[test]
fn selected_source_matching_default_output_is_preserved_and_numbered() {
    let temporary = tempfile::tempdir().unwrap();
    let selected = temporary.path().join("Archive.zip");
    let other = temporary.path().join("other.txt");
    fs::write(&selected, b"original archive-named source").unwrap();
    fs::write(&other, b"other source").unwrap();
    let output = compress(
        &[selected.clone(), other],
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output, temporary.path().join("Archive (1).zip"));
    let mut archive = ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
    assert_eq!(
        std::io::read_to_string(archive.by_name("Archive.zip").unwrap()).unwrap(),
        "original archive-named source"
    );
    assert_eq!(
        std::io::read_to_string(archive.by_name("other.txt").unwrap()).unwrap(),
        "other source"
    );
    assert_eq!(
        fs::read(selected).unwrap(),
        b"original archive-named source"
    );
    no_staging(temporary.path());
}

#[test]
fn cancellation_during_streaming_removes_zip_and_extract_staging() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("large");
    fs::write(&input, vec![b'x'; 512 * 1024]).unwrap();
    let cancel = AtomicBool::new(false);
    let error = compress(std::slice::from_ref(&input), None, &cancel, &mut |event| {
        if matches!(event, Progress::Bytes(_)) {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .unwrap_err();
    assert!(cancelled(&error));
    assert!(!temporary.path().join("large.zip").exists());
    no_staging(temporary.path());
    cancel.store(false, Ordering::Relaxed);
    let archive = temporary.path().join("large.zip");
    zip(&archive, &[("file", &vec![b'x'; 512 * 1024])]);
    let error = extract(
        &[archive],
        &ExtractMode::Smart,
        None,
        &cancel,
        &mut |event| {
            if matches!(event, Progress::Bytes(_)) {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    )
    .unwrap_err();
    assert!(cancelled(&error));
    assert_eq!(fs::read(&input).unwrap().len(), 512 * 1024);
    assert!(!temporary.path().join("large/file").exists());
    no_staging(temporary.path());
}

#[test]
fn failed_multi_extract_reports_earlier_outputs() {
    let temporary = tempfile::tempdir().unwrap();
    let first = temporary.path().join("first.zip");
    let second = temporary.path().join("second.zip");
    zip(&first, &[("one", b"first")]);
    zip(&second, &[("../escape", b"second")]);
    let error = extract(
        &[first, second.clone()],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap_err();
    let failure = error.downcast_ref::<ExtractFailure>().unwrap();
    assert_eq!(failure.archive, second);
    assert_eq!(failure.completed, vec![temporary.path().join("first")]);
    assert_eq!(
        fs::read(failure.completed[0].join("one")).unwrap(),
        b"first"
    );
    assert!(!temporary.path().join("second").exists());
}

#[test]
fn hardlinks_are_preserved_and_duplicates_and_special_entries_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("hardlinks.tar");
    tar(
        &archive,
        &[
            link(b"root/link2", b"root/link1", b'1'),
            link(b"root/link1", b"root/file", b'1'),
            regular(b"root/file", b"contents"),
        ],
    );
    let outputs = extract(
        &[archive],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        fs::metadata(outputs[0].join("file")).unwrap().ino(),
        fs::metadata(outputs[0].join("link2")).unwrap().ino()
    );
    for (name, entries) in [
        (
            "duplicate",
            vec![regular(b"file", b"first"), regular(b"file", b"second")],
        ),
        (
            "special",
            vec![TarEntry {
                name: b"fifo",
                kind: b'6',
                target: b"",
                data: b"",
                mode: 0o644,
            }],
        ),
        (
            "cycle",
            vec![link(b"one", b"two", b'1'), link(b"two", b"one", b'1')],
        ),
    ] {
        let archive = temporary.path().join(format!("{name}.tar"));
        tar(&archive, &entries);
        assert!(
            extract(
                &[archive],
                &ExtractMode::Smart,
                None,
                &AtomicBool::new(false),
                &mut |_| {}
            )
            .is_err()
        );
        assert!(!temporary.path().join(name).exists());
    }
    no_staging(temporary.path());
}

#[test]
fn raw_gzip_and_compound_suffix_and_utf8_names_round_trip() {
    let temporary = tempfile::tempdir().unwrap();
    let gzip = temporary.path().join("message.txt.gz");
    let mut encoder = GzEncoder::new(fs::File::create(&gzip).unwrap(), Compression::default());
    encoder.write_all(b"raw text").unwrap();
    encoder.finish().unwrap();
    let output = extract(
        &[gzip],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output, vec![temporary.path().join("message.txt")]);
    assert_eq!(
        fs::read(output[0].join("message.txt")).unwrap(),
        b"raw text"
    );
    let empty = temporary.path().join("empty.gz");
    GzEncoder::new(fs::File::create(&empty).unwrap(), Compression::default())
        .finish()
        .unwrap();
    let output = extract(
        &[empty],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(fs::metadata(output[0].join("empty")).unwrap().len(), 0);
    let source = temporary.path().join("unicode.tar");
    tar(&source, &[regular("root/日本語.txt".as_bytes(), b"UTF-8")]);
    let compound = temporary.path().join("chosen.tar.gz");
    let mut encoder = GzEncoder::new(fs::File::create(&compound).unwrap(), Compression::default());
    encoder.write_all(&fs::read(&source).unwrap()).unwrap();
    encoder.finish().unwrap();
    let output = extract(
        &[compound],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output, vec![temporary.path().join("chosen")]);
    assert_eq!(fs::read(output[0].join("日本語.txt")).unwrap(), b"UTF-8");
}

#[test]
fn corrupt_zip_gzip_and_truncated_tar_never_publish() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("corrupt.zip");
    zip(&archive, &[("file", b"unique checksum payload")]);
    let mut bytes = fs::read(&archive).unwrap();
    let offset = bytes
        .windows(b"unique checksum payload".len())
        .position(|window| window == b"unique checksum payload")
        .unwrap();
    bytes[offset] ^= 1;
    fs::write(&archive, bytes).unwrap();
    assert!(
        extract(
            &[archive],
            &ExtractMode::Smart,
            None,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert!(!temporary.path().join("corrupt").exists());
    let link_archive = temporary.path().join("symlink.zip");
    let mut writer = ZipWriter::new(fs::File::create(&link_archive).unwrap());
    writer
        .add_symlink(
            "root/link",
            "unique-safe-target",
            SimpleFileOptions::default(),
        )
        .unwrap();
    writer.finish().unwrap();
    let mut bytes = fs::read(&link_archive).unwrap();
    let offset = bytes
        .windows(b"unique-safe-target".len())
        .position(|window| window == b"unique-safe-target")
        .unwrap();
    bytes[offset] ^= 1;
    fs::write(&link_archive, bytes).unwrap();
    assert!(
        extract(
            &[link_archive],
            &ExtractMode::Smart,
            None,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert!(!temporary.path().join("symlink").exists());
    let gzip = temporary.path().join("checksum.gz");
    let mut encoder = GzEncoder::new(fs::File::create(&gzip).unwrap(), Compression::default());
    encoder.write_all(b"gzip payload").unwrap();
    encoder.finish().unwrap();
    let mut bytes = fs::read(&gzip).unwrap();
    let trailer = bytes.len() - 8;
    bytes[trailer] ^= 1;
    fs::write(&gzip, bytes).unwrap();
    assert!(
        extract(
            &[gzip],
            &ExtractMode::Smart,
            None,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert!(!temporary.path().join("checksum").exists());
    let archive = temporary.path().join("truncated.tar");
    tar(&archive, &[regular(b"file", &[b'x'; 1024])]);
    fs::OpenOptions::new()
        .write(true)
        .open(&archive)
        .unwrap()
        .set_len(515)
        .unwrap();
    assert!(
        extract(
            &[archive],
            &ExtractMode::Smart,
            None,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert!(!temporary.path().join("truncated").exists());
    no_staging(temporary.path());
}

#[test]
fn arbitrary_uncompressed_input_is_not_misrepresented_as_an_archive() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("plain.txt");
    for content in [b"not an archive".as_slice(), b"".as_slice()] {
        fs::write(&input, content).unwrap();
        assert!(
            extract(
                std::slice::from_ref(&input),
                &ExtractMode::Smart,
                None,
                &AtomicBool::new(false),
                &mut |_| {}
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
        no_staging(temporary.path());
    }
}

#[test]
fn non_utf8_tar_member_bytes_are_preserved() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("bytes.tar");
    tar(&archive, &[regular(b"root/\xff", b"original")]);
    let output = extract(
        &[archive],
        &ExtractMode::Smart,
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        fs::read(output[0].join(OsString::from_vec(vec![0xff]))).unwrap(),
        b"original"
    );
}

#[test]
fn readonly_staging_directories_are_cleaned_after_precommit_cancellation() {
    let temporary = tempfile::tempdir().unwrap();
    let archive = temporary.path().join("restricted.tar");
    tar(
        &archive,
        &[directory(b"root/", 0), regular(b"root/file", b"original")],
    );
    let cancel = AtomicBool::new(false);
    let error = extract(
        std::slice::from_ref(&archive),
        &ExtractMode::Smart,
        None,
        &cancel,
        &mut |event| {
            if matches!(event, Progress::Bytes(_)) {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    )
    .unwrap_err();
    assert!(cancelled(&error));
    assert!(!temporary.path().join("restricted").exists());
    no_staging(temporary.path());
    cancel.store(false, Ordering::Relaxed);
    let existing = temporary.path().join("root");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"untouched").unwrap();
    assert!(extract(&[archive], &ExtractMode::Here, None, &cancel, &mut |_| {}).is_err());
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"untouched");
    no_staging(temporary.path());
    assert_eq!(
        fs::metadata(temporary.path()).unwrap().permissions().mode() & 0o700,
        0o700
    );
}
