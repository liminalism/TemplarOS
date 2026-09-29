//! Home Field design tokens and the few drawing primitives that give them one meaning.
//!
//! The shell deliberately owns these rather than changing Pixelkit: they are TemplarOS product
//! language, not toolkit API.  Keeping the palette, geometry, elevation and state marks here
//! also prevents each tile renderer from inventing a nearly-identical grey or radius.

use pixelkit_raster::Rect;
use pixelkit_text::TextStyle;
use pixelkit_ui::{Theme, Ui};

/// Every colour role in the language, in one value.
///
/// Colours are reached through the accessors below rather than as constants because the palette
/// is chosen once, at startup, from `shell.appearance.theme` — a `const` cannot read a choice
/// made at runtime, so a light-only `const` would have meant every painter staying light however
/// the setting was set. Geometry stays `const`: it does not vary by theme.
///
/// File-category colour is content and continues to come from `templar_treemap`; it never replaces
/// one of these shell colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub ground: u32,
    pub shell_fill: u32,
    pub shell_low: u32,
    pub system_shell: u32,
    /// A real directory's shell: warm and paper-like, so a folder reads as material you file
    /// things into, distinct from the cool glass of a shortcut pane.
    pub folder_shell: u32,
    pub folder_edge: u32,
    /// A shortcut pane's shell: cool and glass-like — references, not the things themselves.
    pub tray_shell: u32,
    pub tray_edge: u32,
    pub recessed: u32,
    pub recessed_edge: u32,
    pub edge: u32,
    pub edge_strong: u32,
    pub project_edge: u32,
    pub text: u32,
    pub text_dim: u32,
    pub text_faint: u32,
    pub accent: u32,
    pub accent_soft: u32,
    pub activity: u32,
    pub activity_soft: u32,
    pub danger: u32,
    pub danger_soft: u32,
    /// What a hovered shell fills with, and what the toolkit's `hover` role is set to. Was a
    /// hardcoded white: on a dark palette that is a flash rather than a lift.
    pub hover_fill: u32,
    /// The upper-left light [`shell`] draws as a one-pixel rule inside the top edge.
    pub top_light: u32,
    /// A well: the recess a window preview or a project's child chip is set into. Lighter than
    /// the shell around it on a light palette, darker on a dark one — it is a hole, not a card.
    pub well_fill: u32,
    /// Ink drawn *on* a saturated fill — the accent, the danger colour, a group's own accent.
    /// A light palette's accent is dark and takes light ink; a dark palette's is light and takes
    /// dark ink, which is the one place the two palettes invert rather than shift.
    pub strong_ink: u32,
}

/// The light palette — the one this shell has always drawn. Every value here is the value the
/// matching constant had before the palette existed, so nothing about the default look moved.
pub const LIGHT: Palette = Palette {
    // A warm mineral off-white, not a cool grey: the substrate a garden is grown on stays quiet
    // and slightly warm, and the material a Place carries reads against it rather than against a
    // screen-blue plate (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`).
    ground: 0x00ee_ebe4,
    shell_fill: 0x00fb_fcfd,
    shell_low: 0x00f3_f5f7,
    system_shell: 0x00f4_f6f8,
    folder_shell: 0x00fb_f8f1,
    folder_edge: 0x00e2_dbc9,
    tray_shell: 0x00f3_f8fb,
    tray_edge: 0x00cf_dde7,
    recessed: 0x00e7_eaee,
    recessed_edge: 0x00b9_c0c8,
    edge: 0x00dd_e1e6,
    edge_strong: 0x00d5_dae0,
    project_edge: 0x00cf_d9db,
    text: 0x0017_1b20,
    text_dim: 0x0069_737d,
    text_faint: 0x008b_939c,
    accent: 0x000e_7482,
    accent_soft: 0x00e3_f1f3,
    activity: 0x001f_8f60,
    activity_soft: 0x00e2_f2ea,
    danger: 0x00b3_261e,
    danger_soft: 0x00fa_e8e6,
    hover_fill: 0x00ff_ffff,
    top_light: 0x00ff_ffff,
    well_fill: 0x00ff_ffff,
    strong_ink: 0x00ff_ffff,
};

/// The dark palette. Same roles, same relationships: the ground is the darkest thing, a shell
/// sits above it, and the two rules [`shell`] draws still run light at the top and dark at the
/// bottom — which is what keeps a tile reading as a raised surface rather than an inverted one.
pub const DARK: Palette = Palette {
    ground: 0x0016_181c,
    shell_fill: 0x0021_2429,
    shell_low: 0x001a_1d21,
    system_shell: 0x001e_2126,
    folder_shell: 0x0026_231c,
    folder_edge: 0x003b_3527,
    tray_shell: 0x001c_2329,
    tray_edge: 0x0033_434e,
    recessed: 0x0018_1b1f,
    recessed_edge: 0x003a_4048,
    edge: 0x0030_353b,
    edge_strong: 0x003c_424a,
    project_edge: 0x0035_4247,
    text: 0x00e6_eaee,
    text_dim: 0x009a_a4ae,
    text_faint: 0x0070_7a84,
    accent: 0x004f_c3d4,
    accent_soft: 0x0013_3a40,
    activity: 0x0044_c68c,
    activity_soft: 0x0012_3428,
    danger: 0x00ff_8a7d,
    danger_soft: 0x003a_1c18,
    hover_fill: 0x002b_2f35,
    top_light: 0x0034_393f,
    well_fill: 0x0012_1418,
    strong_ink: 0x000f_1216,
};

/// The palette this process draws with. Set once, before the first frame; every read before that
/// — and every read in a test — gets [`LIGHT`].
///
/// // ponytail: startup-time palette. Switching theme takes a shell restart, which is half a
/// second (`templar-shell.service` restarts; only the compositor cannot). A live switch means
/// invalidating every cached widget bitmap and re-rendering, which is worth doing when someone
/// asks for it and not before.
static PALETTE: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();

/// Choose the palette. The first call wins; later calls are ignored rather than racing the
/// painters, which is why this returns nothing.
pub fn set_palette(palette: Palette) {
    let _ = PALETTE.set(palette);
}

pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(|| LIGHT)
}

/// The same palette with a different accent. The soft accent is derived rather than asked for:
/// one chosen colour is a setting a person can hold in their head, and a second one they have to
/// keep consistent with the first is not.
#[must_use]
pub fn with_accent(palette: Palette, accent: u32) -> Palette {
    Palette {
        accent,
        accent_soft: mix(accent, palette.shell_fill, 0.86),
        ..palette
    }
}

/// `a` and `b` blended, `t` of the way from `a` to `b`, per channel.
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let (from, to) = (((a >> shift) & 0xff) as f32, ((b >> shift) & 0xff) as f32);
        ((from + (to - from) * t).round().clamp(0.0, 255.0) as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

pub fn ground() -> u32 {
    palette().ground
}
pub fn shell_fill() -> u32 {
    palette().shell_fill
}
pub fn shell_low() -> u32 {
    palette().shell_low
}
pub fn system_shell() -> u32 {
    palette().system_shell
}
pub fn folder_shell() -> u32 {
    palette().folder_shell
}
pub fn folder_edge() -> u32 {
    palette().folder_edge
}
pub fn tray_shell() -> u32 {
    palette().tray_shell
}
pub fn tray_edge() -> u32 {
    palette().tray_edge
}
pub fn recessed() -> u32 {
    palette().recessed
}
pub fn recessed_edge() -> u32 {
    palette().recessed_edge
}
pub fn edge() -> u32 {
    palette().edge
}
pub fn edge_strong() -> u32 {
    palette().edge_strong
}
pub fn project_edge() -> u32 {
    palette().project_edge
}
pub fn text() -> u32 {
    palette().text
}
pub fn text_dim() -> u32 {
    palette().text_dim
}
pub fn text_faint() -> u32 {
    palette().text_faint
}
pub fn accent() -> u32 {
    palette().accent
}
pub fn accent_soft() -> u32 {
    palette().accent_soft
}
pub fn activity() -> u32 {
    palette().activity
}
pub fn activity_soft() -> u32 {
    palette().activity_soft
}
pub fn danger() -> u32 {
    palette().danger
}
pub fn danger_soft() -> u32 {
    palette().danger_soft
}
pub fn well_fill() -> u32 {
    palette().well_fill
}
pub fn strong_ink() -> u32 {
    palette().strong_ink
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
pub const TILE_RADIUS: i32 = 8;
pub const WELL_RADIUS: i32 = 5;
pub const CHIP_RADIUS: i32 = 4;
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
    theme_for(palette())
}

/// The same, for a palette that is not (or not yet) the process's own — what the Settings page
/// and the palette tests use to look at one without setting it.
pub fn theme_for(palette: &Palette) -> Theme {
    let mut theme = Theme::default();
    theme.background = palette.ground;
    theme.panel = palette.shell_fill;
    theme.panel_alt = palette.system_shell;
    theme.panel_edge = palette.edge;
    theme.text = palette.text;
    theme.text_dim = palette.text_dim;
    theme.alarm = palette.danger;
    theme.alarm_soft = palette.danger_soft;
    theme.good = palette.activity;
    theme.good_soft = palette.activity_soft;
    theme.accent = palette.accent;
    theme.accent_soft = palette.accent_soft;
    theme.hover = palette.hover_fill;
    theme.selection = palette.accent_soft;
    theme.row_height = HEADING_HEIGHT;
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
        Elevation::Hover => ui.painter.shadow_rect(rect, ui.px(2), ui.px(3), text(), 42),
        Elevation::Lifted => ui
            .painter
            .shadow_rect(rect, ui.px(10), ui.px(12), text(), 110),
    }
    let fill = if elevation == Elevation::Hover {
        palette().hover_fill
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
            palette().top_light,
            180,
        );
        ui.painter.blend_rect(
            Rect::new(inner.x, inner.bottom() - 1, inner.w, 1),
            palette().shell_low,
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

/// The label over one part of a tile's body — smaller than the tile's own title and tracked, so
/// it reads as a divider rather than as another row of content.
pub fn section_label_style(ui: &Ui<'_>) -> TextStyle {
    TextStyle::tracked(ui.theme.body.face, 10.0, 0.12)
}

pub fn system_label_style(ui: &Ui<'_>) -> TextStyle {
    TextStyle::tracked(ui.theme.body.face, 13.0, 0.14)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_field_theme_uses_the_design_roles() {
        let theme = theme_for(&LIGHT);
        assert_eq!(theme.background, LIGHT.ground);
        assert_eq!(theme.panel, LIGHT.shell_fill);
        assert_eq!(theme.panel_edge, LIGHT.edge);
        assert_eq!(theme.accent, LIGHT.accent);
        assert_eq!(theme.good, LIGHT.activity);
        assert_eq!(theme.alarm, LIGHT.danger);
        assert_eq!(theme.corner_radius, TILE_RADIUS);
    }

    /// The dark palette has to differ in *every* role: one forgotten field is a light colour on a
    /// dark ground, which is exactly the bug a struct-of-roles exists to make impossible to ship
    /// silently. And light's roles are pinned, so a change to the default look is a change to
    /// this test and never a side effect of one.
    #[test]
    fn dark_palette_has_every_role_and_light_is_pinned() {
        // The one role that has moved since the palette refactor: a warm mineral off-white
        // substrate rather than a cool grey one.
        assert_eq!(LIGHT.ground, 0x00ee_ebe4);
        assert_eq!(LIGHT.shell_fill, 0x00fb_fcfd);
        assert_eq!(LIGHT.text, 0x0017_1b20);
        assert_eq!(LIGHT.accent, 0x000e_7482);
        assert_eq!(
            LIGHT.hover_fill, 0x00ff_ffff,
            "the hover fill was a hardcoded white"
        );
        assert_eq!(LIGHT.top_light, 0x00ff_ffff, "so was the upper-left light");
        assert_eq!(LIGHT.well_fill, 0x00ff_ffff, "and so was a preview well");
        assert_eq!(LIGHT.strong_ink, 0x00ff_ffff, "and ink on the accent");

        // Field by field, through the same Debug the struct derives: naming them one at a time
        // here would let a role added later slip past this test.
        let light = format!("{LIGHT:?}");
        let dark = format!("{DARK:?}");
        for (l, d) in light.split(", ").zip(dark.split(", ")) {
            assert_ne!(l, d, "a role is identical in both palettes");
        }
        assert_eq!(light.matches(':').count(), dark.matches(':').count());
    }

    #[test]
    fn theme_for_dark_uses_dark_roles() {
        let theme = theme_for(&DARK);
        assert_eq!(theme.background, DARK.ground);
        assert_eq!(theme.text, DARK.text);
        assert_eq!(theme.hover, DARK.hover_fill);
        assert_ne!(theme.background, LIGHT.ground);
    }

    /// An accent is one colour; the soft accent is derived from it so the two cannot drift apart.
    #[test]
    fn with_accent_derives_the_soft_accent_and_leaves_the_rest_alone() {
        let recoloured = with_accent(LIGHT, 0x0080_4020);
        assert_eq!(recoloured.accent, 0x0080_4020);
        assert_ne!(recoloured.accent_soft, LIGHT.accent_soft);
        assert_eq!(recoloured.ground, LIGHT.ground);
        // Soft means mostly the shell it sits on, with the accent tinting it.
        assert_eq!(mix(0x0000_0000, 0x00ff_ffff, 0.0), 0x0000_0000);
        assert_eq!(mix(0x0000_0000, 0x00ff_ffff, 1.0), 0x00ff_ffff);
        assert_eq!(mix(0x0000_0000, 0x00ff_ffff, 0.5), 0x0080_8080);
    }

    /// Nothing has set the palette in a unit test, so every accessor answers with the light one.
    #[test]
    fn an_unset_palette_is_the_light_one() {
        assert_eq!(ground(), LIGHT.ground);
        assert_eq!(text_dim(), LIGHT.text_dim);
        assert_eq!(accent_soft(), LIGHT.accent_soft);
    }
}
