//! Hardware page (`org.templaros.Host.Hardware1`) — the one page that talks to `templar-hostd` on the
//! **system bus**, not to `templar-sessiond` on the session bus.
//!
//! `GraphicsProfile`/`GpuMode` are unauthenticated reads (`interfaces/dbus/org.templaros.Host.Hardware1.xml`
//! doc comment: only `SetGpuMode` is polkit-gated), so this page never needs `SetGpuMode`, never
//! prompts for a password, and simply shows the detected profile and the declared mode.
//! [`Gpu`]/[`GpuVendor`]/[`NvidiaFlavour`]/[`GraphicsProfile`] mirror `crates/templar-hwdetect`'s
//! wire shapes exactly.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GpuVendor {
    Intel,
    Amd,
    Nvidia,
    Other,
}

impl std::fmt::Display for GpuVendor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Intel => "Intel",
            Self::Amd => "AMD",
            Self::Nvidia => "NVIDIA",
            Self::Other => "other",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Gpu {
    pub(crate) pci_slot: String,
    pub(crate) vendor: GpuVendor,
    pub(crate) boot_vga: bool,
    pub(crate) driver: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum NvidiaFlavour {
    Open,
    Proprietary,
}

impl std::fmt::Display for NvidiaFlavour {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::Proprietary => "proprietary",
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GraphicsProfile {
    pub(crate) gpus: Vec<Gpu>,
    pub(crate) hybrid: bool,
    pub(crate) nvidia_flavour: Option<NvidiaFlavour>,
}

/// The bus operations this page needs.
pub(crate) trait HardwareSource {
    async fn graphics_profile(&self) -> Result<String>;
    /// The declared GPU mode (`hybrid`/`integrated`/`discrete`), or `""` if unset.
    async fn gpu_mode(&self) -> Result<String>;
}

impl HardwareSource for templar_client::proxies::HostHardwareProxy<'_> {
    async fn graphics_profile(&self) -> Result<String> {
        self.graphics_profile()
            .await
            .context("Host.Hardware1.GraphicsProfile")
    }

    async fn gpu_mode(&self) -> Result<String> {
        self.gpu_mode().await.context("Host.Hardware1.GpuMode")
    }
}

fn render_profile(profile: &GraphicsProfile, mode: &str) -> String {
    let mut lines = vec![format!(
        "declared GPU mode: {}",
        if mode.is_empty() {
            "unset (distribution default applies)"
        } else {
            mode
        }
    )];
    if profile.gpus.is_empty() {
        lines.push("No GPUs detected.".to_owned());
        return lines.join("\n");
    }
    lines.push(format!(
        "hybrid graphics: {}{}",
        profile.hybrid,
        profile
            .nvidia_flavour
            .map(|flavour| format!(" (NVIDIA flavour: {flavour})"))
            .unwrap_or_default()
    ));
    for gpu in &profile.gpus {
        lines.push(format!(
            "  {} — {}{}{}",
            gpu.pci_slot,
            gpu.vendor,
            if gpu.boot_vga { ", boot VGA" } else { "" },
            gpu.driver.as_deref().map_or_else(
                || ", no driver bound".to_owned(),
                |driver| format!(", driver {driver}")
            ),
        ));
    }
    lines.join("\n")
}

pub(crate) async fn render_with<S: HardwareSource>(source: &S) -> String {
    let mode = match source.gpu_mode().await {
        Ok(mode) => mode,
        Err(error) => return format!("service unavailable: {error:#}"),
    };
    let profile_json = match source.graphics_profile().await {
        Ok(json) => json,
        Err(error) => return format!("service unavailable: {error:#}"),
    };
    match serde_json::from_str::<GraphicsProfile>(&profile_json) {
        Ok(profile) => render_profile(&profile, &mode),
        Err(error) => format!("could not parse Host.Hardware1.GraphicsProfile: {error}"),
    }
}

/// Connects over the **system bus** — `templar-hostd` runs there, not on the session bus.
pub(crate) async fn render() -> String {
    let connection = match zbus::Connection::system().await {
        Ok(connection) => connection,
        Err(error) => {
            return format!("service unavailable: could not reach the system bus ({error})");
        }
    };
    match templar_client::proxies::HostHardwareProxy::new(&connection).await {
        Ok(proxy) => render_with(&proxy).await,
        Err(error) => format!("service unavailable: could not reach Host.Hardware1 ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeHardware {
        profile: String,
        mode: String,
    }

    impl HardwareSource for FakeHardware {
        async fn graphics_profile(&self) -> Result<String> {
            Ok(self.profile.clone())
        }
        async fn gpu_mode(&self) -> Result<String> {
            Ok(self.mode.clone())
        }
    }

    #[tokio::test]
    async fn render_with_shows_the_declared_mode_and_every_gpu() {
        let fake = FakeHardware {
            profile: r#"{"gpus":[
                {"pci_slot":"0000:00:02.0","vendor":"intel","vendor_id":32902,"device_id":1,"boot_vga":true,"driver":"i915"},
                {"pci_slot":"0000:01:00.0","vendor":"nvidia","vendor_id":4318,"device_id":2,"boot_vga":false,"driver":null}
            ],"hybrid":true,"nvidia_flavour":"open","supported_modes":["hybrid","integrated","discrete"]}"#.to_owned(),
            mode: "hybrid".to_owned(),
        };
        let rendered = render_with(&fake).await;
        assert!(rendered.contains("declared GPU mode: hybrid"));
        assert!(rendered.contains("hybrid graphics: true (NVIDIA flavour: open)"));
        assert!(rendered.contains("Intel"));
        assert!(rendered.contains("boot VGA"));
        assert!(rendered.contains("NVIDIA"));
        assert!(rendered.contains("no driver bound"));
    }

    #[tokio::test]
    async fn render_with_says_when_the_mode_is_unset() {
        let fake = FakeHardware {
            profile: r#"{"gpus":[],"hybrid":false,"nvidia_flavour":null,"supported_modes":[]}"#
                .to_owned(),
            mode: String::new(),
        };
        let rendered = render_with(&fake).await;
        assert!(rendered.contains("unset (distribution default applies)"));
        assert!(rendered.contains("No GPUs detected."));
    }

    #[tokio::test]
    async fn render_with_degrades_on_a_hostd_failure() {
        struct Failing;
        impl HardwareSource for Failing {
            async fn graphics_profile(&self) -> Result<String> {
                anyhow::bail!("no reply from Host.Hardware1")
            }
            async fn gpu_mode(&self) -> Result<String> {
                anyhow::bail!("no reply from Host.Hardware1")
            }
        }
        let rendered = render_with(&Failing).await;
        assert!(rendered.starts_with("service unavailable:"));
    }
}
