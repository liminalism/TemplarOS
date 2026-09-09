//! Generated client bindings for the private `templar_shell_v1` protocol
//! (`interfaces/wayland/templar-shell-v1.xml`).
//!
//! Mirrors `compositor/templar-comp/src/protocols.rs`'s own module doc almost exactly, on the client
//! side: `wayland-scanner` 0.31 dropped the build.rs-callable `generate_code` function in favour
//! of `generate_interfaces!`/`generate_client_code!` proc-macros invoked directly from source, so
//! the bindings live here rather than in a build script. There is exactly one protocol file to
//! bind (this crate never speaks `templar_recovery_v1`, which stays compositor-core-owned), but the
//! `__interfaces` submodule dance is still required: `generate_client_code!` expects it to already
//! be populated by `generate_interfaces!` before it runs.

#[allow(
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    unused_imports
)]
pub mod templar_shell_v1 {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../interfaces/wayland/templar-shell-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../interfaces/wayland/templar-shell-v1.xml");
}
