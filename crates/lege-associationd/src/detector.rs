//! MIME detection adapter.
//!
//! First prototype: the installed shared-MIME machinery through `xdg-mime`. The real service
//! replaces the subprocess with direct shared-mime-info database access plus bounded content
//! sniffing (never trust the extension alone).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::path::Path;
use tokio::process::Command;

/// Something that can tell what a file actually is.
#[async_trait]
pub trait MimeDetector: Send + Sync {
    async fn detect(&self, path: &Path) -> Result<String>;
}

/// `xdg-mime query filetype`.
#[derive(Debug, Default, Clone, Copy)]
pub struct XdgMimeDetector;

#[async_trait]
impl MimeDetector for XdgMimeDetector {
    async fn detect(&self, path: &Path) -> Result<String> {
        let output = Command::new("xdg-mime")
            .arg("query")
            .arg("filetype")
            .arg(path)
            .output()
            .await
            .with_context(|| format!("detecting MIME type for {}", path.display()))?;

        if !output.status.success() {
            bail!("xdg-mime failed with {}", output.status);
        }

        let mime = String::from_utf8(output.stdout)?;
        let mime = mime.trim();
        if mime.is_empty() {
            return Ok("application/octet-stream".to_owned());
        }
        Ok(mime.to_owned())
    }
}

/// Fixed answers, for tests and dry runs.
#[derive(Debug, Clone)]
pub struct StaticDetector(pub String);

#[async_trait]
impl MimeDetector for StaticDetector {
    async fn detect(&self, _path: &Path) -> Result<String> {
        Ok(self.0.clone())
    }
}
