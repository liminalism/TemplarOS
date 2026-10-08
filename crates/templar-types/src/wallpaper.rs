//! Wallpaper policy and descriptor vocabulary. Codec/GPU ownership lives elsewhere.
use crate::{display::DisplayId, settings::Revision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizingMode { #[default] Fill, Fit, Stretch, Center, Actual, Tile }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOrder { #[default] Random, Sequential, Shuffle }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextureLimit { #[default] Native, MaxEdge { pixels: u32 }, MaxPixels { pixels: u64 } }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputOverride {
    pub sizing: Option<SizingMode>,
    pub ground: Option<[u8; 4]>,
    pub texture: Option<TextureLimit>,
    /// Lossless Unix filename bytes; absent uses the shared selected source.
    pub source: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WallpaperConfig {
    /// Files or directories, represented as lossless Unix filename bytes.
    pub sources: Vec<Vec<u8>>,
    pub recursive: bool,
    pub order: SourceOrder,
    /// None disables automatic rotation. Otherwise 300..=3600 seconds.
    pub interval_seconds: Option<u64>,
    pub sizing: SizingMode,
    pub ground: [u8; 4],
    pub texture: TextureLimit,
    pub worker_budget_bytes: u64,
    /// Includes candidate backing plus old/candidate graphics allocations.
    pub transition_budget_bytes: u64,
    /// WuDP palette, 1..=256 colors; 256 by default, None disables quantization.
    pub quantize_colors: Option<u16>,
    pub outputs: BTreeMap<String, OutputOverride>,
}
impl Default for WallpaperConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(), recursive: false, order: SourceOrder::Random,
            interval_seconds: None, sizing: SizingMode::Fill, ground: [24, 26, 30, 255],
            texture: TextureLimit::Native, worker_budget_bytes: 192 * 1024 * 1024,
            transition_budget_bytes: 256 * 1024 * 1024, quantize_colors: Some(256),
            outputs: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat { Rgba8, Indexed8 }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RasterDescriptor {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    /// Oriented source dimensions, before an explicit texture reduction.
    pub source_width: u32,
    pub source_height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    /// Empty for RGBA; 1..=256 opaque colors for indexed output.
    pub palette: Vec<[u8; 4]>,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallpaperImage {
    pub revision: Revision,
    pub display_revision: Revision,
    pub output: DisplayId,
    pub raster: RasterDescriptor,
    pub sizing: SizingMode,
    pub ground: [u8; 4],
    pub transition_budget_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallpaperOutput {
    pub output: DisplayId,
    pub accepted: Option<Revision>,
    /// Updated only after this generation participated in a successful frame.
    pub presented: Option<Revision>,
    pub source_bytes: u64,
    pub texture_bytes: u64,
    pub context_replicas: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallpaperObservation {
    pub revision: Revision,
    pub outputs: Vec<WallpaperOutput>,
    pub indexed_supported: bool,
}
impl WallpaperObservation {
    pub fn active(&self) -> bool {
        self.outputs.iter().any(|output| output.presented.is_some())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallpaperStatus {
    pub revision: Revision,
    pub enabled: bool,
    pub config: WallpaperConfig,
    pub paused: bool,
    pub source: Option<Vec<u8>>,
    pub pending: bool,
    pub applied: Option<WallpaperObservation>,
    pub error: Option<String>,
}
