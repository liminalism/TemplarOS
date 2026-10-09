//! The Home Surface as a real window.
//!
//! `--print`/`--list-drawers` stay the tested, headless path (`crate::data`, `crate::drawers`);
//! everything in this module is a rendering front end over exactly that same model, drawn with
//! `pixelkit`. See [`window`]'s module doc for why this crate drives its own
//! `winit::application::ApplicationHandler` rather than `pixelkit_shell::run_app`, and
//! `crate::wayland_bridge` for how it still reaches the compositor's private protocol from there.

pub mod app;
pub mod bar_screen;
pub(crate) mod calendar;
pub mod cluster_screen;
pub mod context_menu;
pub mod design;
pub mod desktop_dock;
pub mod field_screen;
pub mod file_browser;
pub mod files_screen;
pub mod fonts;
pub mod glyphs;
pub mod lock_screen;
pub mod notification_feed;
pub mod notification_popup;
pub mod offscreen;
pub mod omnibox_screen;
pub mod place_styles;
pub mod rail;
pub mod rail_session;
pub mod search_pages;
pub mod taskfield_screen;
pub mod widgets;
pub mod window;
pub mod window_visual;

pub use app::run;
