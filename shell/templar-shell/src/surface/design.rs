//! Home Field drawing primitives. Declarative policy lives in `templar_types::appearance`;
//! `templar_ui` maps it to the toolkit without introducing windowing dependencies.

use pixelkit_raster::Rect;
use pixelkit_text::TextStyle;
use pixelkit_ui::{Theme, Ui};

use templar_types::appearance::{LIGHT, Palette, spacing, typography};

static PALETTE: std::sync::RwLock<Palette> = std::sync::RwLock::new(LIGHT);

/// Replace drawing colours live. The host must invalidate cached bitmaps when this returns true.
pub fn set_palette(palette: Palette) -> bool {
    let mut current = PALETTE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *current == palette {
        return false;
    }
    *current = palette;
    true
}

pub fn palette() -> Palette {
    *read_palette()
}

fn read_palette() -> std::sync::RwLockReadGuard<'static, Palette> {
    PALETTE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn ground() -> u32 {
    read_palette().ground
}
pub fn shell_fill() -> u32 {
    read_palette().shell_fill
}
pub fn shell_low() -> u32 {
    read_palette().shell_low
}
pub fn system_shell() -> u32 {
    read_palette().system_shell
}
pub fn folder_shell() -> u32 {
    read_palette().folder_shell
}
pub fn folder_edge() -> u32 {
    read_palette().folder_edge
}
pub fn tray_shell() -> u32 {
    read_palette().tray_shell
}
pub fn tray_edge() -> u32 {
    read_palette().tray_edge
}
pub fn recessed() -> u32 {
    read_palette().recessed
}
pub fn recessed_edge() -> u32 {
    read_palette().recessed_edge
}
pub fn edge() -> u32 {
    read_palette().edge
}
pub fn edge_strong() -> u32 {
    read_palette().edge_strong
}
pub fn project_edge() -> u32 {
    read_palette().project_edge
}
pub fn text() -> u32 {
    read_palette().text
}
pub fn text_dim() -> u32 {
    read_palette().text_dim
}
pub fn text_faint() -> u32 {
    read_palette().text_faint
}
pub fn accent() -> u32 {
    read_palette().accent
}
pub fn accent_soft() -> u32 {
    read_palette().accent_soft
}
pub fn activity() -> u32 {
    read_palette().activity
}
pub fn activity_soft() -> u32 {
    read_palette().activity_soft
}
pub fn danger() -> u32 {
    read_palette().danger
}
pub fn danger_soft() -> u32 {
    read_palette().danger_soft
}
pub fn well_fill() -> u32 {
    read_palette().well_fill
}
pub fn strong_ink() -> u32 {
    read_palette().strong_ink
}

/// The bottom bar's chips wear the compositor's own name-tag colours, copied rather than shared:
/// `chrome::paint::RECOVERY_BAND_COLOR` and the light half of `chrome::layout::text_ink_for`.
/// A taskbar chip and the tag on a window's own strip are the same object seen twice, so they
/// have to look the same — and the shell does not link the compositor, so the two constants are
/// kept in step by this comment and by K3 rather than by the type system.
pub const TAG_FILL: u32 = 0x002e_333d;
pub const TAG_INK: u32 = 0x00f2_f2f2;

// Geometry. There are intentionally only three radii in the language.
pub const FIELD_INSET: i32 = 10;
pub const TOP_BAR_HEIGHT: i32 = 44;
pub const TOP_BAR_GAP: i32 = 8;
pub const HOME_WIDTH: i32 = 104;
/// The search field's preferred width. It is centred in the top bar and shrinks when the
/// output cannot spare this much between Home and Help.
pub const SEARCH_WIDTH: i32 = 520;
/// The recent-apps strip under the search field: icon plus name, one row.
pub const RECENTS_HEIGHT: i32 = 76;
/// One recent application in that strip.
pub const RECENT_CHIP_WIDTH: i32 = 76;
pub const TILE_RADIUS: i32 = spacing::TILE_RADIUS;
pub const WELL_RADIUS: i32 = spacing::WELL_RADIUS;
pub const CHIP_RADIUS: i32 = spacing::CHIP_RADIUS;
/// The heading a tile large enough to have one carries: its name, and the controls that act on
/// the whole tile. Reserved from the body, because at this size there is room to spend on it.
pub const HEADING_HEIGHT: i32 = 30;
/// At the compact rungs there is no heading. A strip this tall along the top edge is grabbable
/// for a move, and it is *not* taken away from the body — a 1x1 application tile spending a
/// third of itself on a band with its own name in it was the single worst trade on the field.
pub const EDGE_GRAB_HEIGHT: i32 = 10;
/// What hovering or focusing a compact tile reveals: a strip over the top of its body carrying
/// the controls the heading would have held. Identity is never hidden — only manipulation.
pub const REVEAL_HEIGHT: i32 = 20;
/// One line of a compact tile's body, which is where its title goes.
pub const COMPACT_TITLE_HEIGHT: i32 = 18;
pub const ACTIVITY_EDGE_WIDTH: i32 = 5;
pub const FILE_SPINE_WIDTH: i32 = 8;
pub const COMPOSITION_BAND_HEIGHT: i32 = 5;

// The Files object. It is the most-looked-at rectangle on the desktop, so its spacing is named
// here beside the rest of the language rather than spelled out inside its draw code.
/// The margin between the Files object's shell and anything inside it.
pub const FILES_PAD: i32 = 12;
/// One line of the object: a fact, a condition, or a changed file.
pub const FILES_ROW_HEIGHT: i32 = 18;
/// A pinned location's chip.
pub const FILES_CHIP_HEIGHT: i32 = 22;
pub const FILES_CHIP_GAP: i32 = 6;
/// Between the object's parts — heading, locations, conditions, listing. Larger than the gap
/// inside a part, which is what makes them read as parts at all.
pub const FILES_SECTION_GAP: i32 = 10;
/// The bar under a running operation's line. Thin on purpose: it is a state mark, not a control.
pub const FILES_PROGRESS_HEIGHT: i32 = 3;

/// The chosen Home Field palette expressed through Pixelkit's existing semantic roles.
///
/// Pixelkit's default stays untouched because its dark operations-console palette is correct for
/// its own consumers.  TemplarOS supplies this theme to both of its shell surfaces.
pub fn theme() -> Theme {
    templar_ui::theme_for(&read_palette())
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
        Elevation::Hover => ui.painter.shadow_rect(rect, ui.px(2), ui.px(3), text(), 42),
        Elevation::Lifted => ui
            .painter
            .shadow_rect(rect, ui.px(10), ui.px(12), text(), 110),
    }
    let fill = if elevation == Elevation::Hover {
        read_palette().hover_fill
    } else {
        fill
    };
    ui.painter
        .fill_rounded_rect_aa(ui.kernel, rect, ui.px(TILE_RADIUS) as f32, fill, 255);
    // Pixelkit has intentionally tiny primitives rather than a gradient API. These two subtle
    // rules produce the specified upper-left light without adding toolkit surface area.
    let inner = rect.inset(ui.scale.hairline());
    if inner.w > 0 && inner.h > 1 {
        ui.painter.blend_rect(
            Rect::new(inner.x, inner.y, inner.w, 1),
            read_palette().top_light,
            180,
        );
        ui.painter.blend_rect(
            Rect::new(inner.x, inner.bottom() - 1, inner.w, 1),
            read_palette().shell_low,
            120,
        );
    }
    ui.painter.stroke_rect(rect, ui.scale.hairline(), edge);
}

/// A keyboard focus is a complete accent ring and soft halo, never the activity-green edge.
pub fn focus_ring(ui: &mut Ui<'_>, rect: Rect) {
    ui.painter.shadow_rect(rect, 0, ui.px(4), accent(), 70);
    ui.painter.stroke_rect(rect, ui.px(2), accent());
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
    ui.text_style(TextStyle::new(ui.theme.body.face, typography::TITLE))
}

pub fn state_style(ui: &Ui<'_>) -> TextStyle {
    ui.text_style(TextStyle::new(ui.theme.body.face, typography::STATE))
}

pub fn measurement_style(ui: &Ui<'_>) -> TextStyle {
    // The embedded set currently has one DejaVu face; this still centralises the 12/16 role so
    // adding DejaVu Sans Mono to the set later does not require changing every tile renderer.
    ui.text_style(TextStyle::new(ui.theme.body.face, 12.0))
}

/// The label over one part of a tile's body — smaller than the tile's own title and tracked, so
/// it reads as a divider rather than as another row of content.
pub fn section_label_style(ui: &Ui<'_>) -> TextStyle {
    ui.text_style(TextStyle::tracked(ui.theme.body.face, 10.0, 0.12))
}

pub fn system_label_style(ui: &Ui<'_>) -> TextStyle {
    ui.text_style(TextStyle::tracked(ui.theme.body.face, 13.0, 0.14))
}
