//! Hardware profile: detection (via `templar-hwdetect`) plus the one declarative GPU-mode setting
//! (planning doc 01 §4: "hybrid, integrated-only and discrete-only modes are represented by one
//! declarative setting rather than hand-edited configuration files"). hostd is the only writer of
//! `<etc>/templaros/hardware/gpu-mode`; nothing else in the system should hand-edit it.

use anyhow::{Context, Result, bail};
use std::{fs, path::Path};
use templar_hwdetect::{GpuMode, GraphicsProfile, classify, scan_sysfs};

pub fn probe(sys_root: &Path) -> Result<GraphicsProfile> {
    Ok(classify(scan_sysfs(sys_root)?))
}

/// Apply a GPU mode: validated against what the detected hardware actually supports, then
/// written as the single declarative file every mode-aware component reads. This *is* "the right
/// mechanism" for a hybrid laptop in the absence of a live udev/prime-select action — the actual
/// switch (powering the dGPU, `__NV_PRIME_RENDER_OFFLOAD` defaults) is a session-side concern that
/// reads this file; hostd's job is only to make it one trustworthy, privileged write.
pub fn set_gpu_mode(mode: GpuMode, profile: &GraphicsProfile, etc_dir: &Path) -> Result<()> {
    if !profile.supported_modes.contains(&mode) {
        bail!(
            "mode {mode:?} is not supported by this hardware (supports {:?})",
            profile.supported_modes
        );
    }
    let path = etc_dir.join("gpu-mode");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let value = mode_to_str(mode);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{value}\n")).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

pub fn read_gpu_mode(etc_dir: &Path) -> Result<Option<GpuMode>> {
    match fs::read_to_string(etc_dir.join("gpu-mode")) {
        Ok(text) => mode_from_str(text.trim())
            .map(Some)
            .with_context(|| format!("unrecognised gpu-mode value {:?}", text.trim())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn mode_to_str(mode: GpuMode) -> &'static str {
    match mode {
        GpuMode::Hybrid => "hybrid",
        GpuMode::Integrated => "integrated",
        GpuMode::Discrete => "discrete",
    }
}

fn mode_from_str(text: &str) -> Result<GpuMode> {
    match text {
        "hybrid" => Ok(GpuMode::Hybrid),
        "integrated" => Ok(GpuMode::Integrated),
        "discrete" => Ok(GpuMode::Discrete),
        other => bail!("unknown gpu mode {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_hwdetect::{Gpu, GpuVendor};

    fn hybrid_profile() -> GraphicsProfile {
        classify(vec![
            Gpu {
                pci_slot: "0000:00:02.0".into(),
                vendor: GpuVendor::Intel,
                vendor_id: 0x8086,
                device_id: 0xa7a0,
                boot_vga: true,
                driver: Some("i915".into()),
            },
            Gpu {
                pci_slot: "0000:01:00.0".into(),
                vendor: GpuVendor::Nvidia,
                vendor_id: 0x10de,
                device_id: 0x28e0,
                boot_vga: false,
                driver: Some("nvidia".into()),
            },
        ])
    }

    #[test]
    fn set_and_read_gpu_mode_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_gpu_mode(dir.path()).unwrap().is_none());
        set_gpu_mode(GpuMode::Discrete, &hybrid_profile(), dir.path()).unwrap();
        assert_eq!(read_gpu_mode(dir.path()).unwrap(), Some(GpuMode::Discrete));
    }

    #[test]
    fn set_gpu_mode_rejects_unsupported_mode() {
        let dir = tempfile::tempdir().unwrap();
        let integrated_only = classify(vec![Gpu {
            pci_slot: "0000:00:02.0".into(),
            vendor: GpuVendor::Intel,
            vendor_id: 0x8086,
            device_id: 0xa7a0,
            boot_vga: true,
            driver: Some("i915".into()),
        }]);
        assert!(set_gpu_mode(GpuMode::Discrete, &integrated_only, dir.path()).is_err());
        assert!(read_gpu_mode(dir.path()).unwrap().is_none());
    }
}
