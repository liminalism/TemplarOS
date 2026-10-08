use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use templar_types::wallpaper::{RasterDescriptor, WallpaperConfig};

#[derive(Debug)]
pub struct Decoded { pub file: File, pub raster: RasterDescriptor }

/// Codecs live in the temporary executable, never in the resident Wayland host.
pub fn decode(path: &Path, decoder: &Path, config: &WallpaperConfig) -> Result<Decoded> {
    let source = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let output = crate::raster::create_memfd()?;
    // Move both input handles away from 3/4 before dup2: inherited descriptor ordering
    // must not accidentally close the source while installing the destination.
    fn duplicate(file: &File) -> Result<OwnedFd> {
        // SAFETY: F_DUPFD_CLOEXEC creates a new owned descriptor or returns -1.
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        // SAFETY: the successful fcntl returned a fresh descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    let source_fd = duplicate(&source)?;
    let output_fd = duplicate(&output)?;
    let options = serde_json::json!({
        "texture": config.texture, "ground": config.ground,
        "worker_budget_bytes": config.worker_budget_bytes,
        "quantize_colors": config.quantize_colors,
    });
    let mut command = Command::new(decoder);
    command.args(["--source-fd", "3", "--output-fd", "4", "--options-json"])
        .arg(serde_json::to_string(&options)?)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    // SAFETY: the hook only invokes async-signal-safe descriptor operations.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source_fd.as_raw_fd(), 3) < 0 || libc::dup2(output_fd.as_raw_fd(), 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().with_context(|| format!("launching {}", decoder.display()))?;
    let mut stdout = child.stdout.take().context("worker stdout missing")?;
    // SAFETY: fcntl changes flags on our pipe only.
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: the owned pipe descriptor is valid, and flags came from F_GETFL.
    if flags < 0 || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let _ = child.kill(); let _ = child.wait();
        return Err(std::io::Error::last_os_error().into());
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let result = (|| -> Result<RasterDescriptor> {
        let mut body = Vec::new();
        let mut block = [0; 4096];
        let mut eof = false;
        loop {
            loop {
                match stdout.read(&mut block) {
                    Ok(0) => { eof = true; break; }
                    Ok(n) => {
                        if body.len() + n > 65536 { bail!("decoder descriptor exceeds 64 KiB"); }
                        body.extend_from_slice(&block[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() { bail!("decoder exited with {status}"); }
                if eof { return Ok(serde_json::from_slice(&body).context("invalid decoder descriptor")?); }
            }
            if Instant::now() >= deadline { bail!("decoder exceeded 60-second deadline"); }
            let mut fd = libc::pollfd { fd: stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: poll points to one initialized pollfd for this call.
            unsafe { libc::poll(&mut fd, 1, 20); }
        }
    })();
    if result.is_err() { let _ = child.kill(); }
    let _ = child.wait();
    let raster = result?;
    crate::raster::seal(&output)?;
    crate::raster::validate(&output, &raster)?;
    Ok(Decoded { file: output, raster })
}
