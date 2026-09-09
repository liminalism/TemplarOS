//! Generated server bindings for our two private protocols
//! (`interfaces/wayland/templar-shell-v1.xml`, `interfaces/wayland/templar-recovery-v1.xml`).
//!
//! `wayland-scanner` 0.31 (the version compatible with the pinned Smithay revision) dropped the
//! old build.rs-callable `generate_code` function in favour of `generate_interfaces!` /
//! `generate_server_code!` proc-macros invoked directly from source — there is no build.rs entry
//! point left to call at this version, so the bindings live here instead of in a build script.
//! Each protocol gets its own module because `generate_server_code!` expects a fresh
//! `__interfaces` submodule to already be populated by `generate_interfaces!` before it runs.
//!
//! `interfaces/wayland/README.md` (planning doc) requires `templar_shell_manager_v1.layer`'s wire
//! values to match `templar_window::Layer`'s `repr(u8)` discriminants exactly; the conversion lives
//! in `templar_comp_integration::protocol::{layer_to_wire, layer_from_wire}`, not duplicated here.

#[allow(
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    unused_imports
)]
pub mod templar_shell_v1 {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../interfaces/wayland/templar-shell-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("../../interfaces/wayland/templar-shell-v1.xml");
}

#[allow(
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    unused_imports
)]
pub mod templar_recovery_v1 {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../interfaces/wayland/templar-recovery-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("../../interfaces/wayland/templar-recovery-v1.xml");
}
