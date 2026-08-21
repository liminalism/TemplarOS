//! Compositor-drawn chrome, as flat solid-colour rectangles.
//!
//! TODO(pixelkit-track): this is deliberately not real chrome. The recovery band, the
//! client-side-decoration recovery tab, the pressure overlay and the "show every window" scatter
//! background are drawn here as plain colour blocks with no text and no icon, because another
//! track owns `pixelkit-raster`/`pixelkit-text` (ADR-0012) and this crate must not add a font or
//! drawing dependency ahead of it. Every function below is a placeholder for that follow-up, not a
//! finished design; the colours exist only to make the four kinds of chrome visually distinct
//! while testing.

use crate::state::{Backend, LegeCompState};
use smithay::{
    backend::renderer::element::{Id, Kind, solid::SolidColorRenderElement},
    output::Output,
    utils::{Physical, Point, Rectangle, Scale},
};

/// A rectangle in output-logical space plus the colour to fill it with — the one shape every
/// chrome element reduces to before it reaches the renderer.
#[derive(Clone, Copy, Debug)]
pub struct ChromeRect {
    pub logical: Rectangle<i32, smithay::utils::Logical>,
    pub color: [f32; 4],
}

/// RGBA for the compositor-owned recovery band drawn at the top of every normal window
/// (`lege_window::recovery_band`). A muted slate so it reads as chrome, not content.
pub const RECOVERY_BAND_COLOR: [f32; 4] = [0.18, 0.20, 0.24, 0.92];

/// RGBA for the temporary recovery tab painted over client-side-decorated/borderless windows
/// (`WindowService::recovery_affordance().requires_compositor_paint`) — brighter than the plain
/// band so it reads as "the compositor put this here on purpose", not part of the client's frame.
pub const RECOVERY_TAB_COLOR: [f32; 4] = [0.85, 0.55, 0.10, 0.95];

/// RGBA for the resource-pressure overlay background (`lege_recovery_v1.show_pressure_overlay`).
pub const PRESSURE_OVERLAY_COLOR: [f32; 4] = [0.45, 0.08, 0.08, 0.85];

/// RGBA for each cell's background in the "show every window" scatter view
/// (`WindowCommand::ShowAllWindows`, `lege_window::scatter_layout`).
pub const SCATTER_CELL_COLOR: [f32; 4] = [0.12, 0.12, 0.16, 0.80];

/// Turn one [`ChromeRect`] into a render element at `output_scale`, positioned relative to the
/// output's own physical origin (`physical_offset`, typically `(0, 0)` — chrome is drawn per
/// output, not once globally).
pub fn render_element(
    rect: ChromeRect,
    physical_offset: Point<i32, Physical>,
    output_scale: Scale<f64>,
) -> SolidColorRenderElement {
    let physical: Rectangle<i32, Physical> = rect.logical.to_physical_precise_round(output_scale);
    let geometry = Rectangle::new(physical.loc + physical_offset, physical.size);
    SolidColorRenderElement::new(
        Id::new(),
        geometry,
        smithay::backend::renderer::utils::CommitCounter::default(),
        rect.color,
        Kind::Unspecified,
    )
}

/// Every chrome rectangle to draw over `output` this frame: recovery bands/tabs for each mapped,
/// visible window, plus the pressure overlay or "show every window" scatter backdrop if either is
/// active. Passed straight to `render_output`'s `custom_elements`, which draws them *above* every
/// `Space` element (`src/backend/winit.rs`) — the recovery band must always win over client
/// content, matching `src/input.rs`'s hit-testing priority for the same rectangles.
pub fn collect_elements<B: Backend + 'static>(
    state: &LegeCompState<B>,
    output: &Output,
) -> Vec<SolidColorRenderElement> {
    let Some(output_geometry) = state.space.output_geometry(output) else {
        return Vec::new();
    };
    let scale = output.current_scale().fractional_scale().into();
    let offset = Point::from((0, 0));
    let mut elements = Vec::new();

    if let Some(overlay) = &state.recovery_protocol.pressure_overlay {
        let _ = overlay;
        elements.push(render_element(
            ChromeRect {
                logical: output_geometry,
                color: PRESSURE_OVERLAY_COLOR,
            },
            offset,
            scale,
        ));
        return elements;
    }

    if let Some(layout) = &state.show_all_windows {
        for (_, rect) in layout {
            elements.push(render_element(
                ChromeRect {
                    logical: Rectangle::new(
                        Point::from((rect.x, rect.y)),
                        smithay::utils::Size::from((rect.width, rect.height)),
                    ),
                    color: SCATTER_CELL_COLOR,
                },
                offset,
                scale,
            ));
        }
    }

    for window in state.window_service.windows() {
        if window.shelved {
            continue;
        }
        let Some(affordance) = state.window_service.recovery_affordance(window.id) else {
            continue;
        };
        let color = if affordance.requires_compositor_paint {
            RECOVERY_TAB_COLOR
        } else {
            RECOVERY_BAND_COLOR
        };
        elements.push(render_element(
            ChromeRect {
                logical: Rectangle::new(
                    Point::from((affordance.band.x, affordance.band.y)),
                    smithay::utils::Size::from((affordance.band.width, affordance.band.height)),
                ),
                color,
            },
            offset,
            scale,
        ));
    }

    elements
}
