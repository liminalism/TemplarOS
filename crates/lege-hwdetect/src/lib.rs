//! Hardware detection: GPUs from sysfs, hybrid-graphics classification, and the single supported
//! NVIDIA driver flavour for the detected hardware (planning doc 01 §4: **one driver authority**).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub mod probes;

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

impl GraphicsProfile {
    /// True when the machine has an NVIDIA GPU and nothing to hybrid-switch to (planning doc 01
    /// §4 / hardware matrix `nvidia-desktop` case).
    pub fn is_discrete_only(&self) -> bool {
        !self.hybrid && self.nvidia_flavour.is_some()
    }
}

/// One DRM connector (an output path) and which GPU it is wired to. Used to select the hardware
/// matrix's "external monitor on each output path" case: on a hybrid laptop, HDMI is commonly
/// wired to the dGPU and USB-C/DP to the iGPU, and both paths must be exercised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// e.g. `card1-HDMI-A-1`.
    pub connector: String,
    pub connected: bool,
    /// PCI slot of the GPU driving this connector's card, when it could be resolved.
    pub gpu_pci_slot: Option<String>,
}

/// Enumerate DRM connectors from a sysfs root. Bare `cardN` directories (not connectors) are
/// skipped; a connector directory's card prefix is resolved back to its GPU via `cardN/device`.
pub fn scan_outputs(sys: &Path) -> anyhow::Result<Vec<Output>> {
    let drm = sys.join("class/drm");
    let mut outputs = Vec::new();
    let Ok(entries) = fs::read_dir(&drm) else {
        return Ok(outputs);
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(dash) = name.find('-') else { continue };
        let card = &name[..dash];
        if !card.starts_with("card") {
            continue;
        }
        let connected =
            fs::read_to_string(entry.path().join("status")).is_ok_and(|s| s.trim() == "connected");
        let gpu_pci_slot = fs::read_link(drm.join(card).join("device"))
            .ok()
            .and_then(|link| link.file_name().map(|f| f.to_string_lossy().into_owned()));
        outputs.push(Output {
            connector: name,
            connected,
            gpu_pci_slot,
        });
    }
    outputs.sort_by(|a, b| a.connector.cmp(&b.connector));
    Ok(outputs)
}

/// Secure Boot posture (ADR-0004: the Secure-Boot decision is a backend choice, not a
/// hardcoding — the boot manager must know what it is dealing with).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecureBootState {
    Enabled,
    Disabled,
    /// Not an EFI system at all (BIOS/CSM); Secure Boot does not apply.
    NotEfi,
    /// EFI system, but the `SecureBoot` efivar could not be read (commonly: no permission).
    Unknown,
}

/// Read the `SecureBoot` UEFI variable from a sysfs root. The efivarfs payload is a 4-byte
/// attributes header followed by the variable's value; `SecureBoot` is a single byte, 0 or 1.
pub fn secure_boot_state(sys: &Path) -> SecureBootState {
    if !sys.join("firmware/efi").is_dir() {
        return SecureBootState::NotEfi;
    }
    let efivars = sys.join("firmware/efi/efivars");
    let Ok(entries) = fs::read_dir(&efivars) else {
        return SecureBootState::Unknown;
    };
    let Some(var) = entries
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("SecureBoot-"))
    else {
        return SecureBootState::Unknown;
    };
    match fs::read(var.path()) {
        Ok(bytes) if bytes.len() >= 5 => {
            if bytes[4] == 1 {
                SecureBootState::Enabled
            } else {
                SecureBootState::Disabled
            }
        }
        _ => SecureBootState::Unknown,
    }
}

/// Package facts read from a dpkg `status` file — the one primitive shared by LRM-presence
/// detection and `lege-hostd`'s support-violation checks (standalone installer, third-party
/// repos, DKMS, mixed flavours; planning doc 01 §4).
pub mod dpkg {
    use std::{fs, path::Path};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct InstalledPackage {
        pub name: String,
        pub version: String,
        /// `Status:` contained "installed" (as opposed to `deinstall`, `config-files`, ...).
        pub installed: bool,
    }

    /// Parse a dpkg `status` file (deb822 stanzas separated by blank lines). A missing file
    /// (common in test fixtures and non-Debian dev machines) is treated as "no packages known",
    /// not an error.
    pub fn scan_status(path: &Path) -> Vec<InstalledPackage> {
        let Ok(text) = fs::read_to_string(path) else {
            return Vec::new();
        };
        let mut packages = Vec::new();
        let mut name: Option<String> = None;
        let mut version: Option<String> = None;
        let mut status: Option<String> = None;
        for line in text.lines().chain(std::iter::once("")) {
            if line.is_empty() {
                if let (Some(n), Some(v)) = (name.take(), version.take()) {
                    let installed = status.take().is_some_and(|s| s.contains("installed"));
                    packages.push(InstalledPackage {
                        name: n,
                        version: v,
                        installed,
                    });
                }
                status = None;
                continue;
            }
            if let Some(v) = line.strip_prefix("Package: ") {
                name = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("Version: ") {
                version = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("Status: ") {
                status = Some(v.trim().to_owned());
            }
        }
        packages
    }

    /// Installed packages whose name starts with `prefix`.
    pub fn find_installed<'a>(
        packages: &'a [InstalledPackage],
        prefix: &str,
    ) -> Vec<&'a InstalledPackage> {
        packages
            .iter()
            .filter(|p| p.installed && p.name.starts_with(prefix))
            .collect()
    }
}

/// Whether the Canonical-signed LRM (linux-restricted-modules) NVIDIA packages are installed —
/// one of the facts the hardware matrix needs to select cases, and a precondition `lege-hostd`
/// checks before trusting a flavour is actually on disk (never DKMS, planning doc 01 §4).
pub fn lrm_present(packages: &[dpkg::InstalledPackage]) -> bool {
    !dpkg::find_installed(packages, "linux-modules-nvidia-").is_empty()
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

    #[test]
    fn discrete_only_classification() {
        let profile = classify(vec![gpu(VENDOR_NVIDIA, 0x28e0)]);
        assert!(profile.is_discrete_only());
        let hybrid = classify(vec![gpu(VENDOR_INTEL, 0xa7a0), gpu(VENDOR_NVIDIA, 0x28e0)]);
        assert!(!hybrid.is_discrete_only());
    }

    #[test]
    fn scans_drm_outputs_and_resolves_gpu() {
        let root = std::env::temp_dir().join(format!("lege-hwdetect-outputs-{}", uid()));
        let drm = root.join("class/drm");
        fs::create_dir_all(drm.join("card1")).unwrap();
        let pci_target = root.join("bus/pci/devices/0000:01:00.0");
        fs::create_dir_all(&pci_target).unwrap();
        std::os::unix::fs::symlink(&pci_target, drm.join("card1/device")).unwrap();

        let hdmi = drm.join("card1-HDMI-A-1");
        fs::create_dir_all(&hdmi).unwrap();
        fs::write(hdmi.join("status"), "connected\n").unwrap();

        let dp = drm.join("card1-DP-1");
        fs::create_dir_all(&dp).unwrap();
        fs::write(dp.join("status"), "disconnected\n").unwrap();

        let outputs = scan_outputs(&root).unwrap();
        assert_eq!(outputs.len(), 2);
        let hdmi_out = outputs
            .iter()
            .find(|o| o.connector.contains("HDMI"))
            .unwrap();
        assert!(hdmi_out.connected);
        assert_eq!(hdmi_out.gpu_pci_slot.as_deref(), Some("0000:01:00.0"));
        let dp_out = outputs.iter().find(|o| o.connector.contains("DP")).unwrap();
        assert!(!dp_out.connected);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secure_boot_reads_efivar_payload() {
        let root = std::env::temp_dir().join(format!("lege-hwdetect-sb-{}", uid()));
        assert_eq!(secure_boot_state(&root), SecureBootState::NotEfi);

        let efivars = root.join("firmware/efi/efivars");
        fs::create_dir_all(&efivars).unwrap();
        // Enabled: 4-byte attributes header + value byte 1.
        fs::write(
            efivars.join("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"),
            [0u8, 0, 0, 0, 1],
        )
        .unwrap();
        assert_eq!(secure_boot_state(&root), SecureBootState::Enabled);

        fs::write(
            efivars.join("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"),
            [0u8, 0, 0, 0, 0],
        )
        .unwrap();
        assert_eq!(secure_boot_state(&root), SecureBootState::Disabled);

        // EFI present but no efivars readable at all (mounted without permission, or absent).
        fs::remove_dir_all(&efivars).unwrap();
        fs::create_dir_all(root.join("firmware/efi")).unwrap();
        assert_eq!(secure_boot_state(&root), SecureBootState::Unknown);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dpkg_status_parses_stanzas_and_finds_lrm() {
        let root = std::env::temp_dir().join(format!("lege-hwdetect-dpkg-{}", uid()));
        fs::create_dir_all(&root).unwrap();
        let status = root.join("status");
        fs::write(
            &status,
            "Package: linux-modules-nvidia-580-open-generic\n\
             Status: install ok installed\n\
             Version: 580.1-0ubuntu1\n\
             \n\
             Package: nvidia-dkms-580\n\
             Status: deinstall ok config-files\n\
             Version: 580.1-0ubuntu1\n\
             \n",
        )
        .unwrap();
        let packages = dpkg::scan_status(&status);
        assert_eq!(packages.len(), 2);
        assert!(lrm_present(&packages));
        assert!(dpkg::find_installed(&packages, "nvidia-dkms-").is_empty());

        assert!(dpkg::scan_status(&root.join("missing")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// Distinguishes fixture directories across tests in this same process (see the historical
    /// `scans_sysfs_fixture` test, which relies on `process::id()` alone and is safe only because
    /// nothing else uses its exact path).
    fn uid() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }
}
