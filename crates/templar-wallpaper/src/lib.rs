//! Wallpaper policy and sealed raster transport. Codec ownership is opt-in and temporary.
pub mod config;
pub mod geometry;
pub mod selection;
pub mod raster;
pub mod quantize;
#[cfg(feature = "decode")]
pub mod decode;
#[cfg(feature = "compatibility")]
pub mod compat;
