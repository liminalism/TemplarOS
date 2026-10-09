use anyhow::{Context, Result, ensure};
use std::{
    ffi::CString,
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd},
    ptr::NonNull,
};
use templar_types::wallpaper::{PixelFormat, RasterDescriptor};

#[allow(unsafe_code)]
pub fn create_memfd() -> io::Result<File> {
    let name = CString::new("templar-wallpaper").expect("literal");
    // SAFETY: CString is terminated, flags are valid, successful descriptor is owned.
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: newly created unique descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[allow(unsafe_code)]
pub fn seal(file: &File) -> io::Result<()> {
    // SAFETY: fcntl operates on a borrowed valid descriptor and scalar flags.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, required_seals()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
const fn required_seals() -> i32 {
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL
}
/// Metadata validation is separate so the producer can validate before sealing.
pub fn validate_descriptor(descriptor: &RasterDescriptor) -> Result<()> {
    ensure!(descriptor.version == 1, "unsupported raster wire version");
    ensure!(
        descriptor.width > 0
            && descriptor.height > 0
            && descriptor.source_width >= descriptor.width
            && descriptor.source_height >= descriptor.height,
        "invalid raster/source dimensions"
    );
    let channels = match descriptor.format {
        PixelFormat::Rgba8 => 4,
        PixelFormat::Indexed8 => 1,
    };
    ensure!(
        descriptor.width.checked_mul(channels) == Some(descriptor.stride),
        "raster must be tightly packed"
    );
    ensure!(
        u64::from(descriptor.stride).checked_mul(u64::from(descriptor.height))
            == Some(descriptor.bytes),
        "raster byte length mismatch"
    );
    ensure!(
        descriptor.bytes <= 2 * 1024 * 1024 * 1024 && isize::try_from(descriptor.bytes).is_ok(),
        "raster exceeds transport envelope"
    );
    match descriptor.format {
        PixelFormat::Rgba8 => ensure!(descriptor.palette.is_empty(), "RGBA raster has a palette"),
        PixelFormat::Indexed8 => ensure!(
            !descriptor.palette.is_empty()
                && descriptor.palette.len() <= 256
                && descriptor.palette.iter().all(|color| color[3] == 255),
            "invalid opaque palette"
        ),
    }
    Ok(())
}
#[allow(unsafe_code)]
pub fn validate(file: &File, descriptor: &RasterDescriptor) -> Result<()> {
    validate_descriptor(descriptor)?;
    let metadata = file.metadata().context("stat raster")?;
    ensure!(
        metadata.is_file() && metadata.len() == descriptor.bytes,
        "raster descriptor/file length mismatch"
    );
    // SAFETY: query has no pointer arguments and does not change file state.
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(
        seals >= 0 && seals & required_seals() == required_seals(),
        "raster lacks immutable memfd seals"
    );
    Ok(())
}
#[derive(Debug)]
pub struct RasterMapping {
    pointer: NonNull<u8>,
    length: usize,
}
// SAFETY: mappings are read-only, sealed against all mutation and uniquely unmapped on Drop.
#[allow(unsafe_code)]
unsafe impl Send for RasterMapping {}
#[allow(unsafe_code)]
unsafe impl Sync for RasterMapping {}
#[allow(unsafe_code)]
impl RasterMapping {
    pub fn new(file: &File, descriptor: &RasterDescriptor) -> Result<Self> {
        validate(file, descriptor)?;
        let length = usize::try_from(descriptor.bytes)?;
        // SAFETY: file is immutable and exact length validated; mmap retains its own file reference.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        ensure!(
            pointer != libc::MAP_FAILED,
            "mapping raster: {}",
            io::Error::last_os_error()
        );
        let result = Self {
            pointer: NonNull::new(pointer.cast()).context("null raster mapping")?,
            length,
        };
        match descriptor.format {
            PixelFormat::Rgba8 => ensure!(
                result
                    .as_bytes()
                    .chunks_exact(4)
                    .all(|pixel| pixel[3] == 255),
                "raster alpha is not opaque"
            ),
            PixelFormat::Indexed8 => ensure!(
                result
                    .as_bytes()
                    .iter()
                    .all(|&index| usize::from(index) < descriptor.palette.len()),
                "raster palette index out of bounds"
            ),
        }
        Ok(result)
    }
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: immutable mapping lives for self's lifetime and has validated length.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }
}
impl std::ops::Deref for RasterMapping {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}
#[allow(unsafe_code)]
impl Drop for RasterMapping {
    fn drop(&mut self) {
        // SAFETY: uniquely owned mapping with original address and length.
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), self.length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn seals_and_pixels_are_enforced() {
        let mut file = create_memfd().unwrap();
        file.write_all(&[1, 2, 3, 255]).unwrap();
        let descriptor = RasterDescriptor {
            version: 1,
            width: 1,
            height: 1,
            source_width: 1,
            source_height: 1,
            stride: 4,
            format: PixelFormat::Rgba8,
            palette: vec![],
            bytes: 4,
        };
        assert!(RasterMapping::new(&file, &descriptor).is_err());
        seal(&file).unwrap();
        assert_eq!(
            RasterMapping::new(&file, &descriptor).unwrap().as_bytes(),
            &[1, 2, 3, 255]
        );
        assert!(file.write_all(&[0]).is_err());
    }
}
