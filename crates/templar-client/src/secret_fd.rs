//! Bounded, fully sealed Linux secret transport shared by owned account/device clients.
//! No secret is carried by JSON, command arguments or diagnostic strings.
use anyhow::{Result, bail, ensure};
use rustix::fs::{MemfdFlags, SealFlags};
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::{fd::OwnedFd, unix::fs::FileExt},
};
use zeroize::{Zeroize, Zeroizing};

fn validate(bytes: &[u8], maximum: usize) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= maximum,
        "secret length is outside the permitted bound"
    );
    ensure!(
        !bytes.iter().any(|byte| matches!(byte, 0 | b'\r' | b'\n')),
        "secret contains a forbidden delimiter"
    );
    Ok(())
}

pub fn seal_utf8(secret: &str, maximum: usize) -> Result<OwnedFd> {
    validate(secret.as_bytes(), maximum)?;
    let descriptor = rustix::fs::memfd_create(
        "templar-secret",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    let mut file = File::from(descriptor);
    file.write_all(secret.as_bytes())?;
    file.seek(SeekFrom::Start(0))?;
    rustix::fs::fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )?;
    Ok(file.into())
}

pub fn read_utf8(descriptor: zbus::zvariant::OwnedFd, maximum: usize) -> Result<Zeroizing<String>> {
    let descriptor: OwnedFd = descriptor.into();
    let file = File::from(descriptor);
    let required = SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL;
    ensure!(
        rustix::fs::fcntl_get_seals(&file)?.contains(required),
        "secret descriptor must be fully sealed"
    );
    let length = usize::try_from(file.metadata()?.len())?;
    ensure!(
        length > 0 && length <= maximum,
        "secret length is outside the permitted bound"
    );
    let mut bytes = Zeroizing::new(vec![0; length]);
    file.read_exact_at(&mut bytes, 0)?;
    validate(&bytes, maximum)?;
    let value = match String::from_utf8(std::mem::take(&mut *bytes)) {
        Ok(value) => value,
        Err(error) => {
            error.into_bytes().zeroize();
            bail!("secret must be valid UTF-8")
        }
    };
    Ok(Zeroizing::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consumer_reads_independently_of_shared_offset_and_kernel_refuses_mutation() {
        let descriptor = seal_utf8("päss:word", 1024).unwrap();
        let mut file = File::from(descriptor);
        file.seek(SeekFrom::End(0)).unwrap();
        assert!(file.write_all(b"x").is_err());
        assert!(file.set_len(1).is_err());
        assert!(file.set_len(1024).is_err());
        let descriptor: OwnedFd = file.into();
        let received = read_utf8(descriptor.into(), 1024).unwrap();
        assert_eq!(received.as_str(), "päss:word");
    }
    #[test]
    fn consumer_refuses_fully_sealed_invalid_utf8() {
        let descriptor = rustix::fs::memfd_create(
            "invalid-utf8-test",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        let mut file = File::from(descriptor);
        file.write_all(&[0xff]).unwrap();
        rustix::fs::fcntl_add_seals(
            &file,
            SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
        )
        .unwrap();
        let descriptor: OwnedFd = file.into();
        assert!(read_utf8(descriptor.into(), 1024).is_err());
    }
    #[test]
    fn transport_refuses_mutable_and_out_of_bounds_payloads() {
        let descriptor = rustix::fs::memfd_create(
            "mutable-test",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        let mut file = File::from(descriptor);
        file.write_all(b"password").unwrap();
        let descriptor: OwnedFd = file.into();
        assert!(read_utf8(descriptor.into(), 1024).is_err());
        for value in ["", "a\n", "a\r", "a\0", "too long"] {
            assert!(seal_utf8(value, 3).is_err());
        }
    }
}
