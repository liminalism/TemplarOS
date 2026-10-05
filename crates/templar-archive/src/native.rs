use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, ensure};
use flate2::read::MultiGzDecoder;
use libarchive3_sys::ffi;

use crate::{BUFFER_SIZE, PasswordRequired, check_cancel};

const FORMAT_MASK: i32 = 0x00ff_0000;
const FORMAT_ZIP: i32 = 0x50000;
const FORMAT_RAW: i32 = 0x90000;

// libarchive3-sys predates these public APIs. These declarations match the
// libarchive 3.x headers (archive.h/archive_entry.h); 3.4+ is checked at runtime.
unsafe extern "C" {
    fn archive_read_support_format_rar5(archive: *mut ffi::Struct_archive) -> libc::c_int;
    fn archive_read_support_format_warc(archive: *mut ffi::Struct_archive) -> libc::c_int;
    fn archive_read_add_passphrase(
        archive: *mut ffi::Struct_archive,
        password: *const libc::c_char,
    ) -> libc::c_int;
    fn archive_entry_is_encrypted(entry: *mut ffi::Struct_archive_entry) -> libc::c_int;
    fn archive_read_has_encrypted_entries(archive: *mut ffi::Struct_archive) -> libc::c_int;
}

#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) path: Vec<u8>,
    pub(crate) kind: u32,
    pub(crate) mode: u32,
    pub(crate) size: Option<u64>,
    pub(crate) symlink: Option<Vec<u8>>,
    pub(crate) hardlink: Option<Vec<u8>>,
    pub(crate) raw: bool,
}

/// Owns exactly one archive_read_new allocation and the fd libarchive borrows.
/// No entry pointer or libarchive-owned byte slice escapes a native call.
#[derive(Debug)]
pub(crate) struct Reader {
    archive: NonNull<ffi::Struct_archive>,
    file: Option<File>,
    gzip: Option<Box<CheckedGzip>>,
    zip_links: Option<zip::ZipArchive<File>>,
    _locale: Utf8Locale,
    password: Option<CString>,
    closed: bool,
}

impl Reader {
    pub(crate) fn open(path: &Path, password: Option<&str>) -> anyhow::Result<Self> {
        // SAFETY: a stateless libarchive version query takes no arguments.
        ensure!(
            unsafe { ffi::archive_version_number() } >= 3_004_000,
            "Extraction requires system libarchive 3.4 or newer"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("Cannot open archive {}", path.display()))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "Archive input is not a regular file: {}",
            path.display()
        );
        ensure!(
            metadata.len() > 0,
            "Empty input is not an archive or compressed stream"
        );
        let password = password
            .map(CString::new)
            .transpose()
            .context("Archive password contains NUL")?;
        let mut signature = [0_u8; 2];
        let is_gzip = file.read_at(&mut signature, 0)? == 2 && signature == [0x1f, 0x8b];
        let (file, gzip) = if is_gzip {
            (None, Some(Box::new(CheckedGzip::new(file))))
        } else {
            (Some(file), None)
        };
        let locale = Utf8Locale::new()?;
        // SAFETY: archive_read_new returns a fresh allocation or null; Reader
        // exclusively owns it and frees it even if subsequent setup fails.
        let archive = NonNull::new(unsafe { ffi::archive_read_new() })
            .context("Cannot allocate native archive reader")?;
        let mut reader = Self {
            archive,
            file,
            gzip,
            zip_links: None,
            _locale: locale,
            password,
            closed: false,
        };
        let pointer = reader.archive.as_ptr();
        // SAFETY: pointer is our live exclusive reader. Registration functions
        // retain no Rust pointers. Do not register mtree: it can read external
        // files named by a manifest rather than only the input archive stream.
        unsafe {
            reader.status(
                ffi::archive_read_support_filter_all(pointer),
                "Registering archive codecs",
            )?;
            let formats: [unsafe extern "C" fn(*mut ffi::Struct_archive) -> libc::c_int; 14] = [
                ffi::archive_read_support_format_7zip,
                ffi::archive_read_support_format_ar,
                ffi::archive_read_support_format_cab,
                ffi::archive_read_support_format_cpio,
                ffi::archive_read_support_format_empty,
                ffi::archive_read_support_format_iso9660,
                ffi::archive_read_support_format_lha,
                ffi::archive_read_support_format_rar,
                archive_read_support_format_rar5,
                ffi::archive_read_support_format_tar,
                archive_read_support_format_warc,
                ffi::archive_read_support_format_xar,
                ffi::archive_read_support_format_zip,
                ffi::archive_read_support_format_raw,
            ];
            for register in formats {
                reader.status(register(pointer), "Registering archive format")?;
            }
            if let Some(password) = &reader.password {
                reader.status(
                    archive_read_add_passphrase(pointer, password.as_ptr()),
                    "Setting ZIP password",
                )?;
            }
            let status = if let Some(gzip) = reader.gzip.as_mut() {
                // Box pins callback state; archive_read_free runs before its
                // destructor. The buffer remains live until the next callback.
                ffi::archive_read_open(
                    pointer,
                    std::ptr::from_mut(gzip.as_mut()).cast(),
                    None,
                    Some(gzip_read),
                    None,
                )
            } else {
                ffi::archive_read_open_fd(
                    pointer,
                    reader.file.as_ref().expect("non-gzip fd").as_raw_fd(),
                    BUFFER_SIZE,
                )
            };
            reader.status(status, "Opening archive")?;
        }
        Ok(reader)
    }

    pub(crate) fn next(&mut self, cancel: &AtomicBool) -> anyhow::Result<Option<Entry>> {
        let pointer = self.archive.as_ptr();
        let mut entry = std::ptr::null_mut();
        // SAFETY: pointer is live; entry is a writable out-pointer. The returned
        // entry is borrowed until the next header; all fields are copied below.
        let status = unsafe { ffi::archive_read_next_header(pointer, &raw mut entry) };
        if status == ffi::ARCHIVE_EOF {
            return Ok(None);
        }
        self.status(status, "Reading archive header")?;
        ensure!(
            !entry.is_null(),
            "Native archive reader returned a missing header"
        );
        // SAFETY: successful read_next_header provided a valid entry; getters
        // return nullable C strings valid for this entry's lifetime.
        unsafe {
            let format = ffi::archive_format(pointer) & FORMAT_MASK;
            if archive_entry_is_encrypted(entry) > 0 {
                self.encrypted(format)?;
            }
            let path = copy_string(ffi::archive_entry_pathname(entry))
                .context("Archive entry has no pathname")?;
            let size = if ffi::archive_entry_size_is_set(entry) != 0 {
                Some(
                    u64::try_from(ffi::archive_entry_size(entry))
                        .context("Archive member has a negative size")?,
                )
            } else {
                None
            };
            let metadata = Entry {
                path,
                kind: ffi::archive_entry_filetype(entry),
                mode: ffi::archive_entry_perm(entry) & 0o777,
                size,
                symlink: copy_string(ffi::archive_entry_symlink(entry)),
                hardlink: copy_string(ffi::archive_entry_hardlink(entry)),
                raw: format == FORMAT_RAW,
            };
            if format == FORMAT_ZIP && metadata.kind == ffi::AE_IFDIR {
                ensure!(
                    metadata.size.unwrap_or(0) == 0,
                    "ZIP directory has unexpected file data"
                );
            }
            if format == FORMAT_ZIP && metadata.symlink.is_some() {
                self.verify_zip_link(&metadata.path, cancel)?;
            }
            Ok(Some(metadata))
        }
    }

    pub(crate) fn read(&mut self, buffer: &mut [u8]) -> anyhow::Result<usize> {
        // SAFETY: archive is live; buffer is uniquely borrowed and writable for
        // exactly buffer.len() bytes. libarchive retains no buffer pointer.
        let count = unsafe {
            ffi::archive_read_data(
                self.archive.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if count < 0 {
            return Err(self.error("Reading archive data"));
        }
        let count = usize::try_from(count).context("Invalid native archive read count")?;
        ensure!(
            count <= buffer.len(),
            "Native archive reader exceeded the provided buffer"
        );
        Ok(count)
    }

    pub(crate) fn is_compressed(&self) -> bool {
        // SAFETY: archive is a live reader. Count includes the final none filter.
        self.gzip.is_some() || unsafe { ffi::archive_filter_count(self.archive.as_ptr()) > 1 }
    }

    pub(crate) fn empty_compressed_stream(&self) -> bool {
        // SAFETY: a live reader's format can be queried after header EOF. EMPTY
        // means zero decompressed bytes, not an empty TAR/ZIP with real headers.
        self.is_compressed()
            && unsafe { ffi::archive_format(self.archive.as_ptr()) & FORMAT_MASK == 0x60000 }
    }

    pub(crate) fn finish(&mut self, cancel: &AtomicBool) -> anyhow::Result<()> {
        // Close native consumers before reusing their last borrowed buffer.
        // SAFETY: archive remains allocated after close and will be freed in Drop.
        let status = unsafe { ffi::archive_read_close(self.archive.as_ptr()) };
        self.closed = true;
        self.status(status, "Closing archive")?;
        if let Some(gzip) = &mut self.gzip {
            // An archive parser can stop before compressor EOF (e.g. TAR zero
            // blocks). Drain the same decoder so CRC/length/truncation are still
            // checked, without a second pass over the input.
            loop {
                check_cancel(cancel)?;
                if gzip
                    .decoder
                    .read(&mut gzip.buffer)
                    .context("Validating gzip checksum and trailer")?
                    == 0
                {
                    break;
                }
            }
        }
        Ok(())
    }

    fn verify_zip_link(&mut self, path: &[u8], cancel: &AtomicBool) -> anyhow::Result<()> {
        if self.zip_links.is_none() {
            let file = self.file.as_ref().context(
                "ZIP symlink integrity verification requires an unwrapped seekable ZIP input",
            )?;
            // Reopen the pinned fd, not try_clone: a cloned fd shares its seek
            // offset with the native reader and would corrupt its stream state.
            let input = File::open(crate::destination::fd_path(file))?;
            self.zip_links = Some(
                zip::ZipArchive::new(input)
                    .context("Cannot read ZIP symlink integrity metadata")?,
            );
        }
        let name = std::str::from_utf8(path)
            .context("ZIP symlink pathname encoding cannot be verified")?;
        let zip = self.zip_links.as_mut().expect("initialized ZIP verifier");
        let mut entry = if let Some(password) = &self.password {
            zip.by_name_decrypt(name, password.as_bytes())
        } else {
            zip.by_name(name)
        }
        .context(
            "Cannot verify ZIP symlink: unsupported compression/encryption or invalid metadata",
        )?;
        ensure!(
            entry.is_symlink(),
            "ZIP symlink metadata disagrees with the native reader"
        );
        let mut buffer = [0_u8; 4096];
        loop {
            check_cancel(cancel)?;
            let count = entry
                .read(&mut buffer)
                .context("ZIP symlink checksum or stream integrity failed")?;
            if count == 0 {
                break;
            }
            ensure!(
                !buffer[..count].contains(&0),
                "ZIP symbolic link target contains NUL"
            );
        }
        Ok(())
    }

    fn encrypted(&self, format: i32) -> anyhow::Result<()> {
        ensure!(
            format == FORMAT_ZIP,
            "Encrypted archive format is unsupported by the native backend (encrypted RAR/7z cannot be extracted)"
        );
        if self.password.is_none() {
            return Err(PasswordRequired.into());
        }
        Ok(())
    }

    fn status(&self, status: i32, operation: &str) -> anyhow::Result<()> {
        if status != ffi::ARCHIVE_OK {
            // Warnings are not success: they can indicate a skipped unsupported
            // method, corrupt metadata, invalid filename encoding or truncation.
            return Err(self.error(operation));
        }
        Ok(())
    }

    fn error(&self, operation: &str) -> anyhow::Error {
        if let Some(error) = self.gzip.as_ref().and_then(|gzip| gzip.error.as_ref()) {
            return anyhow::anyhow!("{operation}: gzip integrity check failed: {error}");
        }
        // SAFETY: native getters borrow live reader state; text is copied before
        // another native call and null error strings are handled explicitly.
        let (format, encrypted, message) = unsafe {
            let pointer = self.archive.as_ptr();
            let text = ffi::archive_error_string(pointer);
            let message = if text.is_null() {
                "Native archive error (corrupt input or unsupported format)".to_owned()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            };
            (
                ffi::archive_format(pointer) & FORMAT_MASK,
                archive_read_has_encrypted_entries(pointer),
                message,
            )
        };
        let lower = message.to_ascii_lowercase();
        let needs_password = encrypted > 0
            || lower.contains("encrypt")
            || lower.contains("passphrase")
            || lower.contains("password");
        if needs_password && format == FORMAT_ZIP && self.password.is_none() {
            return anyhow::Error::new(PasswordRequired).context(format!("{operation}: {message}"));
        }
        if needs_password && format != FORMAT_ZIP {
            return anyhow::anyhow!(
                "{operation}: unsupported encrypted archive: {message} (encrypted RAR/7z are not supported)"
            );
        }
        anyhow::anyhow!("{operation}: {message}")
    }
}

// SAFETY: caller passes a nullable libarchive-owned NUL-terminated metadata
// string valid during this call. Copying ensures no borrow outlives the entry.
unsafe fn copy_string(pointer: *const libc::c_char) -> Option<Vec<u8>> {
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec())
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: Reader exclusively owns the allocation, which has not been
        // freed elsewhere. Closing is idempotently avoided after explicit finish.
        unsafe {
            if !self.closed {
                ffi::archive_read_close(self.archive.as_ptr());
            }
            ffi::archive_read_free(self.archive.as_ptr());
        }
    }
}

/// libarchive's gzip filter does not validate trailer CRC/length. Decoding gzip
/// once with flate2 supplies checked bytes to the same native format readers.
#[derive(Debug)]
struct CheckedGzip {
    decoder: MultiGzDecoder<File>,
    buffer: Box<[u8]>,
    error: Option<std::io::Error>,
}

impl CheckedGzip {
    fn new(file: File) -> Self {
        Self {
            decoder: MultiGzDecoder::new(file),
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
            error: None,
        }
    }
}

unsafe extern "C" fn gzip_read(
    archive: *mut ffi::Struct_archive,
    client: *mut libc::c_void,
    output: *mut *const libc::c_void,
) -> libc::ssize_t {
    // SAFETY: archive_read_open was given this boxed state and callback. Its
    // allocation outlives the archive; libarchive calls synchronously and never
    // retains an exclusive state reference. output is its writable out-pointer.
    let input = unsafe { &mut *client.cast::<CheckedGzip>() };
    match input.decoder.read(&mut input.buffer) {
        Ok(count) => {
            // SAFETY: the boxed buffer stays alive and unchanged until the next
            // read callback, as required by archive_read_callback.
            unsafe {
                *output = input.buffer.as_ptr().cast();
            }
            count as libc::ssize_t
        }
        Err(error) => {
            input.error = Some(error);
            // SAFETY: archive is the live invoking reader; this static format
            // string contains no conversion directives and is NUL-terminated.
            unsafe {
                ffi::archive_set_error(
                    archive,
                    libc::EIO,
                    c"Gzip stream integrity check failed".as_ptr(),
                );
            }
            -1
        }
    }
}

/// A thread-local locale, never a process-global setlocale mutation. Raw native
/// filename bytes are still copied; UTF-8 archives do not fail merely because
/// Rust processes begin with the C library's ASCII locale.
#[derive(Debug)]
struct Utf8Locale {
    current: libc::locale_t,
    previous: libc::locale_t,
}

impl Utf8Locale {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: newlocale creates an owned locale from a static string. A null
        // base requests a new object rather than modifying another locale.
        let current = unsafe {
            libc::newlocale(
                libc::LC_CTYPE_MASK,
                c"C.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if current.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("Cannot create native C.UTF-8 archive locale");
        }
        // SAFETY: current is a live owned locale. uselocale only changes this
        // thread; the prior locale/sentinel is retained for restoration.
        let previous = unsafe { libc::uselocale(current) };
        if previous.is_null() {
            let error = std::io::Error::last_os_error();
            // SAFETY: installation failed, so no thread is using this object.
            unsafe {
                libc::freelocale(current);
            }
            return Err(error.into());
        }
        Ok(Self { current, previous })
    }
}

impl Drop for Utf8Locale {
    fn drop(&mut self) {
        // SAFETY: raw locale pointers make this guard !Send, so Drop runs on the
        // installing thread. Restore its previous locale before freeing ours.
        unsafe {
            libc::uselocale(self.previous);
            libc::freelocale(self.current);
        }
    }
}
