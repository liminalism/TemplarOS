//! Hardware profile: detection (via `lege-hwdetect`) plus the declarative profile files under
//! `/usr/share/legeos/hardware/profiles/`.

use lege_hwdetect::{GraphicsProfile, classify, scan_sysfs};
use std::path::Path;

pub fn probe() -> anyhow::Result<GraphicsProfile> {
    Ok(classify(scan_sysfs(Path::new("/sys"))?))
}

/// `org.legeos.Host.Hardware1`
#[derive(Debug, Default)]
pub struct HardwareApi;

#[zbus::interface(name = "org.legeos.Host.Hardware1")]
impl HardwareApi {
    /// The graphics profile as JSON.
    async fn graphics_profile(&self) -> zbus::fdo::Result<String> {
        let profile = probe().map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        serde_json::to_string(&profile).map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
}
