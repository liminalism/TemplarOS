//! One driver authority: the package repository owns all NVIDIA files, exactly one supported
//! flavour is selected from detected hardware, kernel + modules + userspace are version-locked,
//! and hybrid/integrated/discrete is one declarative setting (planning doc 01 §4).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write as _, path::Path};
use templar_hwdetect::{
    GpuMode, GraphicsProfile, NvidiaFlavour,
    dpkg::{InstalledPackage, find_installed, scan_status},
};

/// The metapackage that owns the NVIDIA stack for a flavour.
pub fn metapackage_for(flavour: NvidiaFlavour) -> &'static str {
    match flavour {
        NvidiaFlavour::Open => "templaros-nvidia-open",
        NvidiaFlavour::Proprietary => "templaros-nvidia-proprietary",
    }
}

/// Files/repos/packages that move the installation into an unsupported state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SupportViolation {
    /// `nvidia-installer` artefacts (or anything else NVIDIA-related) present under `/usr/local`
    /// — every file must be owned by a package (ARCHITECTURE.md invariant).
    StandaloneInstaller { path: String },
    /// A third-party driver repository is enabled (graphics-drivers PPA, NVIDIA's own CUDA repo).
    ThirdPartyRepository { name: String },
    /// A DKMS-built nvidia package is installed; the repository ships LRM only.
    Dkms { package: String },
    /// Both module flavours present on the filesystem (must never coexist).
    MixedFlavours,
    /// An installed component's version does not match the locked set.
    VersionLockMismatch {
        component: String,
        expected: String,
        installed: String,
    },
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

/// Directories that must own every file on disk (ARCHITECTURE.md: "Nothing under `/usr/local`").
const STANDALONE_INSTALLER_PATHS: &[&str] = &[
    "usr/local/bin/nvidia-installer",
    "usr/local/bin/nvidia-uninstall",
    "usr/local/lib/nvidia",
];

/// Repository markers that move a machine out of the one-driver-authority archive.
const THIRD_PARTY_MARKERS: &[&str] = &[
    "ppa.launchpad.net/graphics-drivers",
    "developer.download.nvidia.com",
];

/// Files under `/usr/local` left by NVIDIA's standalone `.run` installer.
pub fn detect_standalone_installer(root: &Path) -> Vec<SupportViolation> {
    STANDALONE_INSTALLER_PATHS
        .iter()
        .filter(|p| root.join(p).exists())
        .map(|p| SupportViolation::StandaloneInstaller {
            path: (*p).to_owned(),
        })
        .collect()
}

/// Third-party apt sources (`sources.list.d/*.list` and deb822 `*.sources`) that must be pinned
/// out (`distro/apt/preferences.d/templaros-nvidia`) or are simply present and enabled.
pub fn detect_third_party_repos(root: &Path) -> Vec<SupportViolation> {
    let dir = root.join("etc/apt/sources.list.d");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut violations = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for marker in THIRD_PARTY_MARKERS {
            if text.contains(marker) {
                violations.push(SupportViolation::ThirdPartyRepository {
                    name: entry.file_name().to_string_lossy().into_owned(),
                });
                break;
            }
        }
    }
    violations
}

/// DKMS-built nvidia packages installed — the repository ships LRM only, never DKMS.
pub fn detect_dkms(packages: &[InstalledPackage]) -> Vec<SupportViolation> {
    find_installed(packages, "nvidia-dkms-")
        .into_iter()
        .map(|p| SupportViolation::Dkms {
            package: p.name.clone(),
        })
        .collect()
}

/// Both the open and proprietary metapackages installed at once.
pub fn detect_mixed_flavours(packages: &[InstalledPackage]) -> Vec<SupportViolation> {
    let open = packages
        .iter()
        .any(|p| p.installed && p.name == "templaros-nvidia-open");
    let proprietary = packages
        .iter()
        .any(|p| p.installed && p.name == "templaros-nvidia-proprietary");
    if open && proprietary {
        vec![SupportViolation::MixedFlavours]
    } else {
        Vec::new()
    }
}

/// Every support-violation check, against a root that is `/` on a live system or a fixture tree
/// in tests.
pub fn detect_violations(root: &Path) -> Vec<SupportViolation> {
    let packages = scan_status(&root.join("var/lib/dpkg/status"));
    let mut violations = detect_standalone_installer(root);
    violations.extend(detect_third_party_repos(root));
    violations.extend(detect_dkms(&packages));
    violations.extend(detect_mixed_flavours(&packages));
    violations
}

/// The kernel + NVIDIA modules + userspace versions this installation is pinned to — written by
/// `Repair`/the image build, read by `templar doctor` and by [`detect_violations`]'s caller to catch
/// drift. One declarative file (`/etc/templaros/hardware/version-lock.toml`), never hand-edited.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionLock {
    pub kernel: Option<String>,
    pub nvidia_lrm: Option<String>,
    pub nvidia_userspace: Option<String>,
}

/// Derive metadata from the same boot pair selected for the candidate's ESP publication.
/// Retained kernel packages are not the active ABI's LRM baseline.
pub fn derive_lock(root: &Path) -> Result<VersionLock> {
    let (kernel, _) = crate::deploy::matching_kernel_pair(&root.join("boot"))
        .context("driver lock requires a matching candidate kernel/initramfs pair")?;
    let abi = kernel.file_name().and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("vmlinuz-")).context("invalid selected kernel ABI")?;
    let packages = templar_hwdetect::dpkg::read_status(&root.join("var/lib/dpkg/status"))
        .context("reading deployment-paired package metadata")?;
    let mut userspace = packages.iter().filter(|package| package.installed && package.name.starts_with("nvidia-driver-"));
    let userspace_version = userspace.next().map(|package| package.version.as_str());
    anyhow::ensure!(userspace.all(|package| Some(package.version.as_str()) == userspace_version),
        "installed NVIDIA userspace branches have different versions");
    let mut modules = packages.iter().filter(|package| package.installed
        && package.name.starts_with("linux-modules-nvidia-")
        && package.name.strip_suffix(abi).is_some_and(|prefix| prefix.ends_with('-')));
    let modules_version = modules.next().map(|package| package.version.as_str());
    anyhow::ensure!(modules.all(|package| Some(package.version.as_str()) == modules_version),
        "selected kernel ABI has conflicting NVIDIA module versions");
    anyhow::ensure!(modules_version.is_some() == userspace_version.is_some(),
        "selected kernel ABI and NVIDIA userspace are not paired");
    Ok(VersionLock {
        kernel: Some(abi.to_owned()),
        nvidia_lrm: modules_version.map(str::to_owned),
        nvidia_userspace: userspace_version.map(str::to_owned),
    })
}

pub fn derive_candidate_lock(candidate_root: &Path) -> Result<()> {
    let lock = derive_lock(candidate_root)?;
    let path = candidate_root.join("etc/templaros/hardware/version-lock.toml");
    if read_version_lock(&path)? == lock { return Ok(()); }
    write_version_lock(&path, &lock)
}

pub fn read_version_lock(path: &Path) -> Result<VersionLock> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VersionLock::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn write_version_lock(path: &Path, lock: &VersionLock) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    let text = toml::to_string_pretty(lock)?;
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

/// Mismatches between the version lock and what is actually installed — kernel and NVIDIA must
/// move as a pair; a mismatched module vs. userspace version is exactly the failure mode ADR-0003
/// exists to prevent.
pub fn version_lock_violations(
    lock: &VersionLock,
    packages: &[InstalledPackage],
) -> Vec<SupportViolation> {
    let mut violations = Vec::new();
    if let Some(expected) = &lock.nvidia_lrm {
        let mut found = false;
        for package in packages.iter().filter(|package| package.installed
            && package.name.starts_with("linux-modules-nvidia-")
            && lock.kernel.as_ref().is_none_or(|abi|
                package.name.strip_suffix(abi).is_some_and(|prefix| prefix.ends_with('-')))) {
            found = true;
            if &package.version != expected {
                violations.push(SupportViolation::VersionLockMismatch {
                    component: "nvidia-lrm".to_owned(), expected: expected.clone(), installed: package.version.clone(),
                });
            }
        }
        if !found {
            violations.push(SupportViolation::VersionLockMismatch {
                component: "nvidia-lrm".to_owned(), expected: expected.clone(), installed: "(absent for selected ABI)".to_owned(),
            });
        }
    }
    check_locked(
        lock.nvidia_userspace.as_ref(),
        "nvidia-driver-",
        "nvidia-userspace",
        packages,
        &mut violations,
    );
    violations
}

fn check_locked(
    expected: Option<&String>,
    prefix: &str,
    component: &str,
    packages: &[InstalledPackage],
    violations: &mut Vec<SupportViolation>,
) {
    let Some(expected) = expected else { return };
    let mut installed = packages.iter().filter(|package| package.installed && package.name.starts_with(prefix)).peekable();
    if installed.peek().is_none() {
        violations.push(SupportViolation::VersionLockMismatch {
            component: component.to_owned(), expected: expected.clone(), installed: "(absent)".to_owned(),
        });
    }
    for package in installed {
        if &package.version != expected {
            violations.push(SupportViolation::VersionLockMismatch {
                component: component.to_owned(),
                expected: expected.clone(),
                installed: package.version.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_hwdetect::{Gpu, GpuVendor, classify};

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
        assert_eq!(plan.metapackage, Some("templaros-nvidia-open"));
        assert_eq!(plan.default_mode, GpuMode::Hybrid);
    }

    #[test]
    fn detects_standalone_installer_under_usr_local() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_standalone_installer(dir.path()).is_empty());
        fs::create_dir_all(dir.path().join("usr/local/bin")).unwrap();
        fs::write(dir.path().join("usr/local/bin/nvidia-installer"), b"").unwrap();
        let violations = detect_standalone_installer(dir.path());
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0],
            SupportViolation::StandaloneInstaller { .. }
        ));
    }

    #[test]
    fn detects_third_party_repo_markers() {
        let dir = tempfile::tempdir().unwrap();
        let sources = dir.path().join("etc/apt/sources.list.d");
        fs::create_dir_all(&sources).unwrap();
        fs::write(
            sources.join("graphics-drivers-ubuntu-ppa-noble.list"),
            "deb http://ppa.launchpad.net/graphics-drivers/ppa/ubuntu noble main\n",
        )
        .unwrap();
        fs::write(
            sources.join("templaros.sources"),
            "URIs: https://archive.templaros.org/ubuntu\n",
        )
        .unwrap();
        let violations = detect_third_party_repos(dir.path());
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn detects_dkms_and_mixed_flavours_from_dpkg_status() {
        let dir = tempfile::tempdir().unwrap();
        let status = dir.path().join("var/lib/dpkg/status");
        fs::create_dir_all(status.parent().unwrap()).unwrap();
        fs::write(
            &status,
            "Package: nvidia-dkms-580\nStatus: install ok installed\nVersion: 580.1\n\n\
             Package: templaros-nvidia-open\nStatus: install ok installed\nVersion: 1.0\n\n\
             Package: templaros-nvidia-proprietary\nStatus: install ok installed\nVersion: 1.0\n\n",
        )
        .unwrap();
        let violations = detect_violations(dir.path());
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, SupportViolation::Dkms { .. }))
        );
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, SupportViolation::MixedFlavours))
        );
    }

    #[test]
    fn retained_kernel_lrm_does_not_mask_or_invent_active_abi_drift() {
        let lock = VersionLock { kernel: Some("7.0.0-31-generic".into()),
            nvidia_lrm: Some("31.31".into()), nvidia_userspace: Some("580.2".into()) };
        let package = |name: &str, version: &str| InstalledPackage {
            name: name.into(), version: version.into(), installed: true,
        };
        let mut packages = vec![
            package("linux-modules-nvidia-580-open-7.0.0-14-generic", "14.14"),
            package("linux-modules-nvidia-580-open-7.0.0-31-generic", "31.31"),
            package("nvidia-driver-580-open", "580.2"),
        ];
        assert!(version_lock_violations(&lock, &packages).is_empty());
        packages[1].version = "31.30".into();
        assert!(matches!(version_lock_violations(&lock, &packages).as_slice(),
            [SupportViolation::VersionLockMismatch { component, installed, .. }]
            if component == "nvidia-lrm" && installed == "31.30"));
        packages.remove(1);
        assert!(matches!(version_lock_violations(&lock, &packages).as_slice(),
            [SupportViolation::VersionLockMismatch { component, installed, .. }]
            if component == "nvidia-lrm" && installed == "(absent for selected ABI)"));
    }
}
