//! The Home Surface and dock as real windows.
//!
//! `--print`/`--list-drawers` stay the tested, headless path (`crate::data`, `crate::dock`,
//! `crate::drawers`); everything in this module is a rendering front end over exactly that same
//! model, drawn with `pixelkit`. See [`window`]'s module doc for why this crate drives its own
//! `winit::application::ApplicationHandler` rather than `pixelkit_shell::run_app`, and
//! `crate::wayland_bridge` for how it still reaches the compositor's private protocol from there.

pub mod app;
pub mod dock_screen;
pub mod fonts;
pub mod home_screen;
pub mod window;

pub use app::run;
