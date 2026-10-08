//! Explicit compatibility backends. None substitutes an XWayland root for wallpaper.
//! X11 and portal are one-shot: schedule them from a session-scoped external owner.
//! Only layer-shell owns a rotation deadline, because its Wayland connection must live.
pub mod layer_shell;
pub mod portal;
pub mod x11;
mod pixels;
mod worker;

use serde::Serialize;
use std::path::{Path, PathBuf};
use templar_types::wallpaper::WallpaperConfig;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend { X11, LayerShell, Portal }

#[derive(Clone, Debug, Serialize)]
pub struct Capabilities {
    pub backend: Backend,
    pub available: bool,
    pub exact_sizing: bool,
    pub reduced_buffers: bool,
    pub resident_connection: bool,
    pub indexed_storage: bool,
    pub presentation_verified: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyStatus {
    pub backend: Backend,
    pub accepted: bool,
    /// Neither root publication nor a portal response proves a visible frame.
    pub presented: bool,
    pub source: Vec<u8>,
    pub buffer_bytes: u64,
    pub retained_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub config: WallpaperConfig,
    pub input: Option<PathBuf>,
    pub previous: Option<PathBuf>,
    pub direction: crate::selection::Direction,
    pub decoder: PathBuf,
    pub output: Option<String>,
}

impl Request {
    pub fn select(&self, selector: &mut crate::selection::SourceSelector) -> anyhow::Result<PathBuf> {
        if let Some(input) = &self.input { return Ok(input.clone()); }
        selector.select(&self.config, self.previous.as_deref(), self.direction)
    }
}

pub fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

pub fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value)?;
    writeln!(lock)?;
    Ok(())
}
