//! Integration crate for the compositor fork (`forks/cosmic-comp`, see `../FORK.md`).
//!
//! * [`identity`] — `DbusIdentityCache`, the render-thread-safe, PID-reuse-safe, self-healing
//!   client of `org.templaros.AppIdentity1`.
//! * [`broadcaster`] — turns `templar_window::WindowService` state into sanitized snapshots for the
//!   shell, encoded and sent off the render thread.
//! * [`governor`] — coalesced focus/visibility enrichment to `org.templaros.Resources1`.
//! * [`protocol`] — names and wire/`Layer` mapping for the private Wayland protocols in
//!   `interfaces/wayland/`; the `wayland-scanner` glue itself lives in the fork.
//!
//! The render thread only ever touches synchronous, allocation-cheap methods on the types here
//! (`DbusIdentityCache::resolve_cached`, `ShellStateBroadcaster::publish`,
//! `GovernorNotifier::set_instance_state`/`forget_instance`) — every one of them hands work to a
//! background tokio task instead of blocking on D-Bus or a slow shell client.

#![forbid(unsafe_code)]

pub mod broadcaster;
pub mod governor;
pub mod identity;
mod pid;

pub use broadcaster::{ShellStateBroadcaster, ShellStateSink, ShellStateSnapshot, snapshot};
pub use governor::GovernorNotifier;
pub use identity::DbusIdentityCache;

/// Names and semantics of the private protocols (XML in `interfaces/wayland/`). The
/// `wayland-scanner`-generated bindings live in the compositor fork; this module is the shared
/// vocabulary between that generated code and `templar_window`'s types.
pub mod protocol {
    use templar_window::Layer;

    pub const SHELL_V1: &str = "templar_shell_v1";
    pub const RECOVERY_V1: &str = "templar_recovery_v1";

    /// `templar_shell_manager_v1.layer` / `templar_shell_toplevel_v1.layer`'s wire values are exactly
    /// `Layer`'s `repr(u8)` discriminants — this is the one conversion point both directions
    /// should use instead of hand-rolling the mapping at each call site in the fork.
    pub fn layer_to_wire(layer: Layer) -> u32 {
        layer as u32
    }

    /// Inverse of [`layer_to_wire`]. `None` for a value outside `0..=7`, which a conformant peer
    /// never sends.
    pub fn layer_from_wire(value: u32) -> Option<Layer> {
        Layer::ALL
            .into_iter()
            .find(|layer| layer_to_wire(*layer) == value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_layer_round_trips_through_the_wire_encoding() {
            for layer in Layer::ALL {
                assert_eq!(layer_from_wire(layer_to_wire(layer)), Some(layer));
            }
        }

        #[test]
        fn an_out_of_range_wire_value_is_rejected() {
            assert_eq!(layer_from_wire(8), None);
        }
    }
}
