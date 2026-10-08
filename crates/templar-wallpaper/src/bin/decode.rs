use anyhow::{Context, Result, bail, ensure};
use std::{fs::{File, OpenOptions}, os::fd::{FromRawFd, RawFd}};
use templar_wallpaper::decode::{DecodeOptions, decode_to_fd};

fn main() {
    if let Err(error) = run() {
        eprintln!("templar-wallpaper-decode: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut source_fd = None;
    let mut output_fd = None;
    let mut input = None;
    let mut output_path = None;
    let mut options_json = None;
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--help" {
            println!("templar-wallpaper-decode --source-fd N --output-fd N --options-json JSON\nSmoke: --input IMAGE --output RAW [--options-json JSON]\nOutput is one RasterDescriptor JSON line; raw pixels are written to the output FD/file.");
            return Ok(());
        }
        let value = arguments.next().context("argument requires a value")?;
        match argument.to_str() {
            Some("--source-fd") => source_fd = Some(parse_fd(&value)?),
            Some("--output-fd") => output_fd = Some(parse_fd(&value)?),
            Some("--input") => input = Some(value),
            Some("--output") => output_path = Some(value),
            Some("--options-json") => { ensure!(value.len() <= 4096, "options JSON exceeds 4096 bytes"); options_json = Some(value.into_string().map_err(|_| anyhow::anyhow!("options JSON is not UTF-8"))?); }
            _ => bail!("unknown argument {argument:?}"),
        }
    }
    let options: DecodeOptions = options_json.map_or_else(|| Ok(DecodeOptions::default()), |json| serde_json::from_str(&json)).context("decode options")?;
    let fd_mode = source_fd.is_some() || output_fd.is_some();
    let (source, output) = if fd_mode {
        ensure!(input.is_none() && output_path.is_none(), "cannot mix FD mode and file smoke mode");
        let source_fd = source_fd.context("--source-fd required")?;
        let output_fd = output_fd.context("--output-fd required")?;
        ensure!(source_fd != output_fd, "source and output descriptors must differ");
        let source = take_fd(source_fd)?;
        let output = take_fd(output_fd)?;
        ensure_access(&source, false)?; ensure_access(&output, true)?;
        (source, output)
    } else {
        let source = File::open(input.context("--input or --source-fd required")?).context("open smoke input")?;
        // create_new prevents accidentally truncating the source or an existing user file.
        let output = OpenOptions::new().read(true).write(true).create_new(true).open(output_path.context("--output or --output-fd required")?).context("create smoke raw output")?;
        (source, output)
    };
    use std::os::unix::fs::MetadataExt;
    let source_metadata = source.metadata()?;
    let output_metadata = output.metadata()?;
    ensure!((source_metadata.dev(), source_metadata.ino()) != (output_metadata.dev(), output_metadata.ino()), "source and output refer to the same file");
    // The estimate covers allocations plus mapped/file-backed output. A process address-space
    // hard ceiling independently contains malformed codec inputs; never inherited by hosts.
    set_address_space_limit(options.worker_budget_bytes)?;
    let descriptor = decode_to_fd(source, output, options)?;
    let json = serde_json::to_string(&descriptor)?;
    ensure!(json.len() <= 8192, "worker descriptor exceeds 8192 bytes");
    println!("{json}");
    Ok(())
}
fn parse_fd(value: &std::ffi::OsStr) -> Result<RawFd> {
    let fd: RawFd = value.to_str().context("descriptor must be ASCII")?.parse().context("invalid descriptor")?;
    ensure!(fd >= 3, "worker descriptor must be at least 3");
    Ok(fd)
}
#[allow(unsafe_code)]
fn take_fd(fd: RawFd) -> Result<File> {
    // SAFETY: query validates that the CLI-provided descriptor exists before taking ownership.
    ensure!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0, "worker descriptor is not open");
    // SAFETY: caller supplies distinct inherited descriptors, transferred to this process.
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[allow(unsafe_code)]
fn ensure_access(file: &File, writable: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: scalar query on a valid borrowed descriptor.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(flags >= 0, "cannot inspect descriptor access");
    let access = flags & libc::O_ACCMODE;
    ensure!(if writable { access == libc::O_RDWR } else { access == libc::O_RDONLY }, "source FD must be read-only and output FD read-write");
    Ok(())
}
#[allow(unsafe_code)]
fn set_address_space_limit(bytes: u64) -> Result<()> {
    let mut current = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: writable rlimit pointer and known resource constant.
    ensure!(unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut current) } == 0, "get worker address-space limit: {}", std::io::Error::last_os_error());
    let ceiling = bytes.min(current.rlim_max);
    let limit = libc::rlimit { rlim_cur: ceiling, rlim_max: ceiling };
    // SAFETY: immutable limit pointer; only this worker is affected.
    ensure!(unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } == 0, "set worker address-space limit: {}", std::io::Error::last_os_error());
    Ok(())
}
