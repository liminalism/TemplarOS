//! Pure Templar appearance policy. No renderer, toolkit, session or windowing dependency.

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
    /// The upper-left light drawn inside the top edge.
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

pub const THEME_KEY: &str = "shell.appearance.theme";
pub const ACCENT_KEY: &str = "shell.appearance.accent";
pub const REDUCED_MOTION_KEY: &str = "shell.home-field.reduced-motion";

pub const ACCENTS: &[(&str, u32)] = &[
    ("teal", 0x000e_7482),
    ("blue", 0x001d_5fb8),
    ("violet", 0x006a_3fb5),
    ("green", 0x001f_7a4a),
    ("amber", 0x008a_5a09),
    ("red", 0x00a8_2f28),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub palette: Palette,
    pub reduced_motion: bool,
}
impl Default for Appearance {
    fn default() -> Self {
        Self {
            palette: LIGHT,
            reduced_motion: false,
        }
    }
}
impl Appearance {
    pub fn from_settings(theme: &str, accent: &str, reduced_motion: bool) -> Self {
        let palette = if theme == "dark" { DARK } else { LIGHT };
        let accent = ACCENTS
            .iter()
            .find_map(|(name, colour)| (*name == accent).then_some(*colour));
        Self {
            palette: accent.map_or(palette, |accent| with_accent(palette, accent)),
            reduced_motion,
        }
    }
    pub fn motion_duration_ms(self, duration: u64) -> u64 {
        if self.reduced_motion { 0 } else { duration }
    }
}

pub mod typography {
    pub const FAMILY: &str = "DejaVu Sans";
    pub const BODY: f32 = 13.0;
    pub const TITLE: f32 = 13.0;
    pub const HEADING: f32 = 32.0;
    pub const STATE: f32 = 11.0;
    pub const SECTION: f32 = 10.0;
}
pub mod spacing {
    pub const PAD: i32 = 12;
    pub const ROW: i32 = 30;
    pub const SECTION_GAP: i32 = 10;
    pub const TILE_RADIUS: i32 = 8;
    pub const WELL_RADIUS: i32 = 5;
    pub const CHIP_RADIUS: i32 = 4;
    pub const BORDER: i32 = 1;
    pub const FOCUS: i32 = 2;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accent_never_replaces_folder_tray_or_category_material() {
        for theme in ["light", "dark"] {
            let base = Appearance::from_settings(theme, "default", false);
            let changed = Appearance::from_settings(theme, "blue", true);
            assert_eq!(changed.palette.folder_shell, base.palette.folder_shell);
            assert_eq!(changed.palette.folder_edge, base.palette.folder_edge);
            assert_eq!(changed.palette.tray_shell, base.palette.tray_shell);
            assert_eq!(changed.palette.activity, base.palette.activity);
            assert_ne!(changed.palette.accent, base.palette.accent);
            assert_eq!(changed.motion_duration_ms(140), 0);
            assert_eq!(base.motion_duration_ms(140), 140);
        }
    }
}
