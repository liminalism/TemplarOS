//! Home Field design tokens and the few drawing primitives that give them one meaning.
//!
//! The shell deliberately owns these rather than changing Pixelkit: they are LegeOS product
//! language, not toolkit API.  Keeping the palette, geometry, elevation and state marks here
//! also prevents each tile renderer from inventing a nearly-identical grey or radius.

use pixelkit_raster::Rect;
use pixelkit_text::TextStyle;
use pixelkit_ui::{Theme, Ui};

// System colour roles. File-category colour is content and continues to come from
// `lege_treemap`; it never replaces one of these shell colours.
pub const GROUND: u32 = 0x00ec_eef0;
pub const SHELL: u32 = 0x00fb_fcfd;
pub const SHELL_LOW: u32 = 0x00f3_f5f7;
pub const SYSTEM_SHELL: u32 = 0x00f4_f6f8;
pub const RECESSED: u32 = 0x00e7_eaee;
pub const RECESSED_EDGE: u32 = 0x00b9_c0c8;
pub const EDGE: u32 = 0x00dd_e1e6;
pub const EDGE_STRONG: u32 = 0x00d5_dae0;
pub const PROJECT_EDGE: u32 = 0x00cf_d9db;
pub const TEXT: u32 = 0x0017_1b20;
pub const TEXT_DIM: u32 = 0x0069_737d;
pub const TEXT_FAINT: u32 = 0x008b_939c;
pub const ACCENT: u32 = 0x000e_7482;
pub const ACCENT_SOFT: u32 = 0x00e3_f1f3;
pub const ACTIVITY: u32 = 0x001f_8f60;
pub const ACTIVITY_SOFT: u32 = 0x00e2_f2ea;
pub const DANGER: u32 = 0x00b3_261e;
pub const DANGER_SOFT: u32 = 0x00fa_e8e6;

// Geometry. There are intentionally only three radii in the language.
pub const FIELD_INSET: i32 = 10;
pub const TOP_BAR_HEIGHT: i32 = 44;
pub const TOP_BAR_GAP: i32 = 8;
pub const HOME_WIDTH: i32 = 104;
pub const TILE_RADIUS: i32 = 8;
pub const WELL_RADIUS: i32 = 5;
pub const CHIP_RADIUS: i32 = 4;
pub const GRAB_BAND_HEIGHT: i32 = 34;
pub const ACTIVITY_EDGE_WIDTH: i32 = 5;
pub const FILE_SPINE_WIDTH: i32 = 8;
pub const COMPOSITION_BAND_HEIGHT: i32 = 5;

/// The light Home Field palette expressed through Pixelkit's existing semantic roles.
///
/// Pixelkit's default stays untouched because its dark operations-console palette is correct for
/// its own consumers.  LegeOS supplies this theme to both of its shell surfaces.
pub fn theme() -> Theme {
    let mut theme = Theme::default();
    theme.background = GROUND;
    theme.panel = SHELL;
    theme.panel_alt = SYSTEM_SHELL;
    theme.panel_edge = EDGE;
    theme.text = TEXT;
    theme.text_dim = TEXT_DIM;
    theme.alarm = DANGER;
    theme.alarm_soft = DANGER_SOFT;
    theme.good = ACTIVITY;
    theme.good_soft = ACTIVITY_SOFT;
    theme.accent = ACCENT;
    theme.accent_soft = ACCENT_SOFT;
    theme.hover = 0x00ff_ffff;
    theme.selection = ACCENT_SOFT;
    theme.row_height = GRAB_BAND_HEIGHT;
    theme.body = theme.body.with_size(13.0);
    theme.heading = theme.heading.with_size(32.0);
    theme.padding = 12;
    theme.corner_radius = TILE_RADIUS;
    theme.border_width = 1;
    theme
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    Rest,
    Hover,
    Lifted,
}

/// Paint the shared rectangular shell. Depth means graspability: only hover and lifted states
/// cast outwards; the resting shell is a hairline, top-lit neutral surface.
pub fn shell(ui: &mut Ui<'_>, rect: Rect, fill: u32, edge: u32, elevation: Elevation) {
    match elevation {
        Elevation::Rest => {}
        Elevation::Hover => ui.painter.shadow_rect(rect, ui.px(2), ui.px(3), TEXT, 42),
        Elevation::Lifted => ui
            .painter
            .shadow_rect(rect, ui.px(10), ui.px(12), TEXT, 110),
    }
    let fill = if elevation == Elevation::Hover {
        0x00ff_ffff
    } else {
        fill
    };
    ui.painter
        .fill_rounded_rect_aa(ui.kernel, rect, ui.px(TILE_RADIUS) as f32, fill, 255);
    // Pixelkit has intentionally tiny primitives rather than a gradient API. These two subtle
    // rules produce the specified upper-left light without adding toolkit surface area.
    let inner = rect.inset(ui.scale.hairline());
    if inner.w > 0 && inner.h > 1 {
        ui.painter
            .blend_rect(Rect::new(inner.x, inner.y, inner.w, 1), 0x00ff_ffff, 180);
        ui.painter.blend_rect(
            Rect::new(inner.x, inner.bottom() - 1, inner.w, 1),
            SHELL_LOW,
            120,
        );
    }
    ui.painter.stroke_rect(rect, ui.scale.hairline(), edge);
}

/// A keyboard focus is a complete accent ring and soft halo, never the activity-green edge.
pub fn focus_ring(ui: &mut Ui<'_>, rect: Rect) {
    ui.painter.shadow_rect(rect, 0, ui.px(4), ACCENT, 70);
    ui.painter.stroke_rect(rect, ui.px(2), ACCENT);
}

/// The absent/recessed grammar used by the origin hole, shelf and landing ghost.
pub fn dashed_rect(ui: &mut Ui<'_>, rect: Rect, colour: u32) {
    let stroke = ui.scale.hairline().max(1);
    let dash = ui.px(7).max(2);
    let gap = ui.px(5).max(1);
    let mut x = rect.x;
    while x < rect.right() {
        let width = dash.min(rect.right() - x);
        ui.painter
            .fill_rect(Rect::new(x, rect.y, width, stroke), colour);
        ui.painter
            .fill_rect(Rect::new(x, rect.bottom() - stroke, width, stroke), colour);
        x += dash + gap;
    }
    let mut y = rect.y;
    while y < rect.bottom() {
        let height = dash.min(rect.bottom() - y);
        ui.painter
            .fill_rect(Rect::new(rect.x, y, stroke, height), colour);
        ui.painter
            .fill_rect(Rect::new(rect.right() - stroke, y, stroke, height), colour);
        y += dash + gap;
    }
}

pub fn title_style(ui: &Ui<'_>) -> TextStyle {
    TextStyle::new(ui.theme.body.face, 13.0)
}

pub fn state_style(ui: &Ui<'_>) -> TextStyle {
    TextStyle::new(ui.theme.body.face, 11.0)
}

pub fn measurement_style(ui: &Ui<'_>) -> TextStyle {
    // The embedded set currently has one DejaVu face; this still centralises the 12/16 role so
    // adding DejaVu Sans Mono to the set later does not require changing every tile renderer.
    TextStyle::new(ui.theme.body.face, 12.0)
}

pub fn system_label_style(ui: &Ui<'_>) -> TextStyle {
    TextStyle::tracked(ui.theme.body.face, 13.0, 0.14)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_field_theme_uses_the_design_roles() {
        let theme = theme();
        assert_eq!(theme.background, GROUND);
        assert_eq!(theme.panel, SHELL);
        assert_eq!(theme.panel_edge, EDGE);
        assert_eq!(theme.accent, ACCENT);
        assert_eq!(theme.good, ACTIVITY);
        assert_eq!(theme.alarm, DANGER);
        assert_eq!(theme.corner_radius, TILE_RADIUS);
    }
}
