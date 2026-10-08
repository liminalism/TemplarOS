//! Typed zbus proxies for every `org.templaros.*` interface, one module per owning daemon.
//!
//! `interfaces/dbus/*.xml` is the contract; these proxies are its Rust side and
//! `cargo xtask check-interfaces` fails the build when the two drift apart. Everything is
//! re-exported flat, so `templar_client::proxies::AppIdentityProxy` keeps working.

pub mod bluetooth;
pub mod checkpoint;
pub mod displays;
pub mod files;
pub mod host;
pub mod input;
pub mod network;
pub mod resources;
pub mod session;
pub mod workloads;
pub mod wallpaper;

pub use bluetooth::BluetoothProxy;
pub use displays::DisplaysProxy;
pub use network::NetworkProxy;
pub use input::InputProxy;
pub use checkpoint::*;
pub use files::*;
pub use host::*;
pub use resources::*;
pub use session::*;
pub use workloads::WorkloadsProxy;
pub use wallpaper::WallpaperProxy;
