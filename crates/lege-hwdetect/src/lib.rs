//! Hardware detection: GPUs from sysfs, hybrid-graphics classification, and the single supported
//! NVIDIA driver flavour for the detected hardware (planning doc 01 §4: **one driver authority**).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const VENDOR_INTEL: u16 = 0x8086;
pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_NVIDIA: u16 = 0x10de;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuVendor {
    Intel,
    Amd,
    Nvidia,
    Other,
}

impl GpuVendor {
    pub fn from_pci(vendor: u16) -> Self {
        match vendor {
            VENDOR_INTEL => Self::Intel,
            VENDOR_AMD => Self::Amd,
            VENDOR_NVIDIA => Self::Nvidia,
            _ => Self::Other,
        }
    }
}

/// One display controller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub pci_slot: String,
    pub vendor: GpuVendor,
    pub vendor_id: u16,
    pub device_id: u16,
    pub boot_vga: bool,
    /// Kernel driver currently bound (`i915`, `xe`, `nvidia`, `nouveau`, `amdgpu`), if any.
    pub driver: Option<String>,
}

/// The one declarative GPU mode (planning doc 01 §4).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuMode {
    /// Integrated drives the desktop; NVIDIA available for render offload.
    Hybrid,
    /// Discrete GPU powered down.
    Integrated,
    /// NVIDIA drives the desktop.
    Discrete,
}

/// Which NVIDIA kernel-module flavour the distribution supports for a GPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NvidiaFlavour {
    /// Open kernel modules — Turing (TU1xx) and newer; NVIDIA's recommended default since 560.
    Open,
    /// Proprietary kernel modules — pre-Turing only.
    Proprietary,
}

/// What the machine looks like from a graphics point of view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsProfile {
    pub gpus: Vec<Gpu>,
    pub hybrid: bool,
    pub nvidia_flavour: Option<NvidiaFlavour>,
    pub supported_modes: Vec<GpuMode>,
}

/// Classify a list of GPUs.
pub fn classify(gpus: Vec<Gpu>) -> GraphicsProfile {
    let has_integrated = gpus
        .iter()
        .any(|gpu| matches!(gpu.vendor, GpuVendor::Intel | GpuVendor::Amd));
    let nvidia = gpus.iter().find(|gpu| gpu.vendor == GpuVendor::Nvidia);
    let hybrid = has_integrated && nvidia.is_some();
    let nvidia_flavour = nvidia.map(|gpu| nvidia_flavour_for(gpu.device_id));
    let supported_modes = if hybrid {
        vec![GpuMode::Hybrid, GpuMode::Integrated, GpuMode::Discrete]
    } else if nvidia.is_some() {
        vec![GpuMode::Discrete]
    } else {
        vec![GpuMode::Integrated]
    };
    GraphicsProfile {
        gpus,
        hybrid,
        nvidia_flavour,
        supported_modes,
    }
}

/// Turing (TU1xx, device IDs from 0x1e00) and newer use the open modules. Coarse but correct
/// for the supported matrix; refined by the `distro/hardware/profiles` tables.
pub fn nvidia_flavour_for(device_id: u16) -> NvidiaFlavour {
    if device_id >= 0x1e00 {
        NvidiaFlavour::Open
    } else {
        NvidiaFlavour::Proprietary
    }
}

/// Enumerate GPUs from a sysfs root (`/sys` on a live system; a fixture directory in tests).
pub fn scan_sysfs(sys: &Path) -> anyhow::Result<Vec<Gpu>> {
    let devices = sys.join("bus/pci/devices");
    let mut gpus = Vec::new();
    let Ok(entries) = fs::read_dir(&devices) else {
        return Ok(gpus);
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let class = read_hex(&path.join("class")).unwrap_or(0);
        // 0x03xxxx = display controller
        if class >> 16 != 0x03 {
            continue;
        }
        let vendor_id = read_hex(&path.join("vendor")).unwrap_or(0) as u16;
        let device_id = read_hex(&path.join("device")).unwrap_or(0) as u16;
        let boot_vga = fs::read_to_string(path.join("boot_vga")).is_ok_and(|s| s.trim() == "1");
        let driver = fs::read_link(path.join("driver")).ok().and_then(|link| {
            link.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        gpus.push(Gpu {
            pci_slot: entry.file_name().to_string_lossy().into_owned(),
            vendor: GpuVendor::from_pci(vendor_id),
            vendor_id,
            device_id,
            boot_vga,
            driver,
        });
    }
    gpus.sort_by(|a, b| a.pci_slot.cmp(&b.pci_slot));
    Ok(gpus)
}

fn read_hex(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(vendor: u16, device: u16) -> Gpu {
        Gpu {
            pci_slot: format!("0000:00:{device:02x}.0"),
            vendor: GpuVendor::from_pci(vendor),
            vendor_id: vendor,
            device_id: device,
            boot_vga: false,
            driver: None,
        }
    }

    #[test]
    fn hybrid_intel_nvidia_laptop_gets_open_flavour_and_three_modes() {
        // Iris Xe + RTX 4060 Laptop (AD107M, 0x28e0)
        let profile = classify(vec![gpu(VENDOR_INTEL, 0xa7a0), gpu(VENDOR_NVIDIA, 0x28e0)]);
        assert!(profile.hybrid);
        assert_eq!(profile.nvidia_flavour, Some(NvidiaFlavour::Open));
        assert_eq!(
            profile.supported_modes,
            vec![GpuMode::Hybrid, GpuMode::Integrated, GpuMode::Discrete]
        );
    }

    #[test]
    fn pascal_gets_proprietary_and_integrated_only_gets_one_mode() {
        assert_eq!(nvidia_flavour_for(0x1c03), NvidiaFlavour::Proprietary); // GTX 1060
        let profile = classify(vec![gpu(VENDOR_AMD, 0x1681)]);
        assert!(!profile.hybrid);
        assert_eq!(profile.nvidia_flavour, None);
        assert_eq!(profile.supported_modes, vec![GpuMode::Integrated]);
    }

    #[test]
    fn scans_sysfs_fixture() {
        let root = std::env::temp_dir().join(format!("lege-hwdetect-{}", std::process::id()));
        let dev = root.join("bus/pci/devices/0000:01:00.0");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("class"), "0x030000\n").unwrap();
        fs::write(dev.join("vendor"), "0x10de\n").unwrap();
        fs::write(dev.join("device"), "0x28e0\n").unwrap();
        fs::write(dev.join("boot_vga"), "0\n").unwrap();
        let other = root.join("bus/pci/devices/0000:00:1f.3");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("class"), "0x040300\n").unwrap();

        let gpus = scan_sysfs(&root).unwrap();
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor, GpuVendor::Nvidia);
        assert_eq!(gpus[0].device_id, 0x28e0);
        let _ = fs::remove_dir_all(&root);
    }
}
