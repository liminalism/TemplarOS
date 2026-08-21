//! The `Backend` trait lives on `LegeCompState` (`crate::state::Backend`); this module holds what
//! both backends share: rebuilding `lege_window`'s `Vec<Output>` (COMPOSITOR.md's "the only place
//! `Output`/topology changes enter `lege-window`") and closing a window from either a Wayland or
//! an XWayland toplevel.

pub mod udev;
pub mod winit;

use crate::state::{Backend, LegeCompState};
use lege_window::{WindowId, geometry::Exclusive};
use smithay::wayland::seat::WaylandFocus;

/// Rebuild the whole output topology from `Space` (kept in sync with whatever the active backend
/// currently has mapped — winit's one synthetic output, or udev's real connectors) plus every live
/// exclusive-zone claim (`lege_shell_v1.set_exclusive_zone`), and feed it through
/// `WindowService::on_outputs_changed` — the single entry point COMPOSITOR.md requires for every
/// kind of topology change, so a dock claiming space and a monitor unplugging can never diverge
/// into different code paths.
pub fn recompute_outputs<B: Backend + 'static>(state: &mut LegeCompState<B>) {
    let outputs: Vec<lege_window::Output> = state
        .space
        .outputs()
        .filter_map(|output| {
            let logical = state.space.output_geometry(output)?;
            let scale_milli = scale_to_milli(output.current_scale());
            let zones: Vec<Exclusive> = state
                .shell_protocol
                .exclusive_zones
                .values()
                .filter(|(name, _)| name == output.name().as_str())
                .map(|(_, zone)| *zone)
                .collect();
            let base = lege_window::Rect::new(
                logical.loc.x,
                logical.loc.y,
                logical.size.w,
                logical.size.h,
            );
            let work_area = crate::window_bridge::work_area_from_zones(base, &zones);
            Some(
                lege_window::Output::new(output.name(), base, scale_milli)
                    .with_work_area(work_area),
            )
        })
        .collect();

    let repaired = state.window_service.on_outputs_changed(outputs);
    for (window_id, rect) in &repaired {
        configure_x11_geometry(state, *window_id, *rect);
    }
    crate::space_sync::sync_after_change(state);
}

/// `WindowService::on_outputs_changed`/`repair_window` never change a window's *size*, only its
/// position (`lege_window::geometry`'s doc) — so a native Wayland toplevel needs no protocol
/// message at all; an X11 window does, since its position is compositor-pushed rather than
/// compositor-decided-and-silent the way `xdg_toplevel` position is.
fn configure_x11_geometry<B: Backend + 'static>(
    state: &LegeCompState<B>,
    window_id: WindowId,
    rect: lege_window::Rect,
) {
    let Some(surface) = state.windows.surface_for(window_id) else {
        return;
    };
    let Some(window) = state
        .space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))
    else {
        return;
    };
    if let Some(x11) = window.x11_surface() {
        let _ = x11.configure(Some(smithay::utils::Rectangle::new(
            smithay::utils::Point::from((rect.x, rect.y)),
            smithay::utils::Size::from((rect.width, rect.height)),
        )));
    }
}

/// `WindowCommand::Close`'s effect: `xdg_toplevel.close` or the X11 delete protocol, whichever the
/// window actually is. `WindowService` itself only reports intent — the window leaves it through
/// the resulting `on_toplevel_destroyed` once the client actually closes (`commands.rs`'s doc).
pub fn close_window<B: Backend + 'static>(state: &LegeCompState<B>, window_id: WindowId) {
    let Some(surface) = state.windows.surface_for(window_id) else {
        return;
    };
    let Some(window) = state
        .space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))
    else {
        return;
    };
    if let Some(toplevel) = window.toplevel() {
        toplevel.send_close();
    } else if let Some(x11) = window.x11_surface() {
        let _ = x11.close();
    }
}

/// Convert a `Scale` (physical/logical output scale, possibly fractional) to `lege_window`'s
/// `scale_milli` convention (`Output::new`'s doc) — kept as a named function rather than inlined
/// so the rounding rule has one definition.
pub fn scale_to_milli(scale: smithay::output::Scale) -> u32 {
    (scale.fractional_scale() * 1000.0).round() as u32
}
