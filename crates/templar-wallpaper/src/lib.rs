//! Wallpaper policy and sealed raster transport. Codec ownership is opt-in and temporary.
#[cfg(feature = "compatibility")]
pub mod compat;
pub mod config;
#[cfg(feature = "decode")]
pub mod decode;
pub mod geometry;
pub mod quantize;
pub mod raster;
pub mod selection;
