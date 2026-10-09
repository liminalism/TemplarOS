//! Shared host state survives root rollback: reject newer schemas and retain pre-write bytes.
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};
use std::{fs, io::Write, path::Path};

pub const SCHEMA: u32 = 1;
pub fn schema() -> u32 {
    SCHEMA
}
pub fn supported_schema<'de, D: Deserializer<'de>>(input: D) -> std::result::Result<u32, D::Error> {
    let version = u32::deserialize(input)?;
    if version != SCHEMA {
        return Err(serde::de::Error::custom(format!(
            "unsupported host state schema {version}; refusing downgrade"
        )));
    }
    Ok(version)
}
/// Preserve the last format before the first rewrite by this schema. Never
/// replace an earlier backup, and sync it before publishing the upgraded file.
pub fn backup_before_write(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("reading pre-migration state"),
    };
    let backup = path.with_extension("pre-schema-1");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
    {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("preserving pre-migration host state"),
    }
    Ok(())
}
pub fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
