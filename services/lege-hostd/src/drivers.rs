//! One driver authority: the package repository owns all NVIDIA files, exactly one supported
//! flavour is selected from detected hardware, kernel + modules + userspace are version-locked,
//! and hybrid/integrated/discrete is one declarative setting.

use lege_hwdetect::{GpuMode, GraphicsProfile, NvidiaFlavour};
use serde::{Deserialize, Serialize};

/// The metapackage that owns the NVIDIA stack for a flavour.
pub fn metapackage_for(flavour: NvidiaFlavour) -> &'static str {
    match flavour {
        NvidiaFlavour::Open => "legeos-nvidia-open",
        NvidiaFlavour::Proprietary => "legeos-nvidia-proprietary",
    }
}

/// Files/repos that move the installation into an unsupported state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SupportViolation {
    /// `nvidia-installer` artefacts present under /usr/lib or /usr/bin.
    StandaloneInstaller { path: String },
    /// A third-party driver repository is enabled.
    ThirdPartyRepository { name: String },
    /// Both module flavours present on the filesystem (must never coexist).
    MixedFlavours,
}

/// The driver plan for a machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverPlan {
    pub metapackage: Option<&'static str>,
    pub default_mode: GpuMode,
    pub violations: Vec<SupportViolation>,
}

pub fn plan_for(profile: &GraphicsProfile) -> DriverPlan {
    DriverPlan {
        metapackage: profile.nvidia_flavour.map(metapackage_for),
        default_mode: if profile.hybrid {
            GpuMode::Hybrid
        } else {
            profile.supported_modes[0]
        },
        violations: Vec::new(),
    }
}

/// `org.legeos.Host.Drivers1`
#[derive(Debug, Default)]
pub struct DriversApi;

#[zbus::interface(name = "org.legeos.Host.Drivers1")]
impl DriversApi {
    /// The driver plan for the detected hardware, as JSON.
    async fn plan(&self) -> zbus::fdo::Result<String> {
        let profile = crate::hardware::probe()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        serde_json::to_string(&plan_for(&profile))
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_hwdetect::{Gpu, GpuVendor, classify};

    #[test]
    fn hybrid_laptop_gets_open_metapackage_and_hybrid_mode() {
        let gpu = |vendor_id: u16, device_id: u16| Gpu {
            pci_slot: String::new(),
            vendor: GpuVendor::from_pci(vendor_id),
            vendor_id,
            device_id,
            boot_vga: false,
            driver: None,
        };
        let plan = plan_for(&classify(vec![gpu(0x8086, 0xa7a0), gpu(0x10de, 0x28e0)]));
        assert_eq!(plan.metapackage, Some("legeos-nvidia-open"));
        assert_eq!(plan.default_mode, GpuMode::Hybrid);
    }
}
