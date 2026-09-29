//! The styles a Place can be drawn in, and the themes that choose among them
//! (`shell.home-garden.theme`).
//!
//! A theme answers one question per form — *which structure, in which material* — and nothing
//! else: the ledger still decides the form, the layout engine (`templar_treemap::garden`) still
//! decides the geometry, and the palette still decides every colour's family. What a theme adds
//! is the drawing's grammar: a branch drawn as a tree or as stones, a shelf as a row or as a
//! cluster, and the grain the material carries.
//!
//! Every colour is derived from the running palette, not written here: an ink is the palette's
//! ground pulled toward its text and tinted a little, so a theme reads on the light and the dark
//! palette alike and never introduces a hue louder than the substrate it is grown on. Colour that
//! means something — the accent — is state, and stays the painter's to spend on blooms and hover.

use templar_treemap::garden::LayoutKind;

use crate::surface::design::{self, Palette};

/// The theme a fresh session draws with, and the one every unknown name falls back to.
pub const DEFAULT_THEME: &str = "mineral";

/// A seeded texture laid over the drawn body. Lightness only: grain is material, never hue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grain {
    /// Per-pixel speckle, as a fraction of lightness (paper tooth).
    pub fine: f32,
    /// Soft low-frequency mottling, as a fraction of lightness (stone, pulp).
    pub coarse: f32,
    /// The mottling's cell, in logical pixels.
    pub cell: f32,
    /// How much longer the mottling runs along x than along y: 1 is isotropic, more reads as
    /// fibre or wood.
    pub stretch: f32,
}

/// How one form is drawn: the structure, and the material it is made of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub kind: LayoutKind,
    /// Directories: the tint their ink leans toward.
    pub dir_tint: u32,
    /// Files: the tint their ink leans toward.
    pub file_tint: u32,
    /// How far the ink leans from neutral toward the tint (0 = pure tone, 1 = the tint).
    pub chroma: f32,
    /// How far a fill is taken from the tile's surface toward the ink, at its strongest.
    pub strength: f32,
    /// Van Wijk cushion shading on rectangles that carry a surface.
    pub cushion: bool,
    /// A soft top-left light across circles and polygons, as a fraction of lightness.
    pub relief: f32,
    /// Corner radius of rectangles, logical px.
    pub radius: f32,
    /// The gap left between neighbouring shapes, logical px — the grout between stones, the
    /// hairline between books.
    pub gap: f32,
    /// A leaf at the tip of each file stroke (the organic branch forms).
    pub leaves: bool,
    pub grain: Grain,
}

/// A family of styles: one per form, plus what a plain place large enough to browse draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    pub branch: Style,
    pub shelf: Style,
    pub folio: Style,
    /// `None`: a plain place that is large enough to browse is the ordinary folder listing.
    pub plain: Option<Style>,
}

impl Theme {
    /// The style for a place in `form`. `large` is whether the tile is at the Large rung, where a
    /// plain place becomes browsable; a plain place below it draws nothing, and so does a form
    /// this build has not learned.
    #[must_use]
    pub fn style_for(&self, form: &str, large: bool) -> Option<&Style> {
        match form {
            "branch" => Some(&self.branch),
            "shelf" => Some(&self.shelf),
            "folio" => Some(&self.folio),
            _ if large => self.plain.as_ref(),
            _ => None,
        }
    }
}

const PAPER: Grain = Grain {
    fine: 0.025,
    coarse: 0.015,
    cell: 5.0,
    stretch: 1.0,
};
const FIBRE: Grain = Grain {
    fine: 0.025,
    coarse: 0.018,
    cell: 2.5,
    stretch: 3.0,
};
const STONE: Grain = Grain {
    fine: 0.03,
    coarse: 0.028,
    cell: 6.0,
    stretch: 1.3,
};
const SMOOTH: Grain = Grain {
    fine: 0.012,
    coarse: 0.0,
    cell: 4.0,
    stretch: 1.0,
};

const fn style(kind: LayoutKind, dir_tint: u32, file_tint: u32, grain: Grain) -> Style {
    Style {
        kind,
        dir_tint,
        file_tint,
        chroma: 0.4,
        strength: 0.5,
        cushion: false,
        relief: 0.0,
        radius: 0.0,
        gap: 1.0,
        leaves: false,
        grain,
    }
}

// Tints: earth, lichen and slate, all low in chroma; the palette's own tone does most of the work.
const WARM_STONE: u32 = 0x0086_7a64;
const SLATE_SAGE: u32 = 0x0070_817c;
const BARK: u32 = 0x0078_6048;
const MOSS: u32 = 0x006a_7c46;
const LIMESTONE: u32 = 0x008c_8270;
const SLATE: u32 = 0x006c_7780;
const HEATHER: u32 = 0x007f_6f88;
const LICHEN: u32 = 0x006b_8286;

/// Every shipped theme, in the order the setting offers them.
pub const THEMES: &[Theme] = &[
    // The default: the plain, legible drawing — a tree of ink strokes for a place worked
    // through, a row of books for a place looked at, a pile of sheets for a working set — on
    // the mineral paper the field is already made of.
    Theme {
        name: "mineral",
        branch: Style {
            strength: 0.62,
            ..style(LayoutKind::Branches, WARM_STONE, SLATE_SAGE, PAPER)
        },
        shelf: Style {
            radius: 1.5,
            gap: 1.5,
            strength: 0.42,
            ..style(LayoutKind::Strip, WARM_STONE, SLATE_SAGE, FIBRE)
        },
        folio: Style {
            strength: 0.3,
            gap: 0.0,
            ..style(LayoutKind::Folio, WARM_STONE, SLATE_SAGE, PAPER)
        },
        plain: None,
    },
    // Grown rather than drawn: a branching tree with leaves for a project, fruit-like clusters
    // for a place of things.
    Theme {
        name: "orchard",
        branch: Style {
            strength: 0.66,
            chroma: 0.55,
            leaves: true,
            ..style(LayoutKind::Fractal, BARK, MOSS, FIBRE)
        },
        shelf: Style {
            strength: 0.4,
            chroma: 0.5,
            relief: 0.07,
            gap: 1.5,
            ..style(LayoutKind::CirclePack, BARK, MOSS, PAPER)
        },
        folio: Style {
            strength: 0.3,
            chroma: 0.5,
            gap: 0.0,
            ..style(LayoutKind::Folio, BARK, MOSS, PAPER)
        },
        plain: Some(Style {
            strength: 0.36,
            chroma: 0.5,
            relief: 0.07,
            gap: 1.5,
            ..style(LayoutKind::CirclePack, BARK, MOSS, PAPER)
        }),
    },
    // SequoiaView's cushions in the garden's own materials: every rectangle a soft pillow, so
    // the nesting reads from the light alone.
    Theme {
        name: "cushion",
        branch: Style {
            cushion: true,
            strength: 0.38,
            gap: 0.0,
            ..style(LayoutKind::Squarified, WARM_STONE, SLATE_SAGE, SMOOTH)
        },
        shelf: Style {
            cushion: true,
            strength: 0.35,
            gap: 0.0,
            ..style(LayoutKind::Squarified, WARM_STONE, SLATE_SAGE, SMOOTH)
        },
        folio: Style {
            cushion: true,
            strength: 0.35,
            gap: 0.0,
            ..style(LayoutKind::Squarified, WARM_STONE, SLATE_SAGE, SMOOTH)
        },
        plain: Some(Style {
            cushion: true,
            strength: 0.32,
            gap: 0.0,
            ..style(LayoutKind::Squarified, WARM_STONE, SLATE_SAGE, SMOOTH)
        }),
    },
    // Dry-stone: every place a wall of cells, each as large as its share, set in grout.
    Theme {
        name: "stone",
        branch: Style {
            strength: 0.42,
            relief: 0.06,
            gap: 1.6,
            ..style(LayoutKind::Voronoi, LIMESTONE, SLATE, STONE)
        },
        shelf: Style {
            strength: 0.4,
            relief: 0.06,
            gap: 1.6,
            ..style(LayoutKind::Voronoi, LIMESTONE, SLATE, STONE)
        },
        folio: Style {
            strength: 0.38,
            relief: 0.06,
            gap: 1.6,
            ..style(LayoutKind::Voronoi, LIMESTONE, SLATE, STONE)
        },
        plain: Some(Style {
            strength: 0.36,
            relief: 0.06,
            gap: 1.6,
            ..style(LayoutKind::Voronoi, LIMESTONE, SLATE, STONE)
        }),
    },
    // Rings and strata: the hierarchy read outward from the root, or downward in bands.
    Theme {
        name: "radial",
        branch: Style {
            strength: 0.46,
            gap: 1.0,
            chroma: 0.6,
            ..style(LayoutKind::Sunburst, HEATHER, LICHEN, PAPER)
        },
        shelf: Style {
            strength: 0.42,
            gap: 1.0,
            chroma: 0.6,
            radius: 1.0,
            ..style(LayoutKind::Icicle, HEATHER, LICHEN, FIBRE)
        },
        folio: Style {
            strength: 0.4,
            gap: 1.0,
            chroma: 0.6,
            radius: 1.0,
            ..style(LayoutKind::Icicle, HEATHER, LICHEN, PAPER)
        },
        plain: None,
    },
];

/// The theme called `name`, or [`DEFAULT_THEME`]'s when there is none: a setting written by a
/// newer build, or by hand, must still draw.
#[must_use]
pub fn theme_named(name: &str) -> &'static Theme {
    THEMES
        .iter()
        .find(|theme| theme.name == name)
        .unwrap_or(&THEMES[0])
}

/// The ink a style draws `is_dir`'s items in, from `palette`: the ground pulled most of the way
/// toward the text and leaned toward the style's tint. On the light palette a warm mid-grey; on
/// the dark one a lighter one — the same relationship to the ground either way.
#[must_use]
pub fn ink(palette: &Palette, style: &Style, is_dir: bool) -> u32 {
    let tone = design::mix(palette.ground, palette.text, 0.55);
    design::mix(
        tone,
        if is_dir {
            style.dir_tint
        } else {
            style.file_tint
        },
        style.chroma,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_theme_is_drawn_as_mineral() {
        assert_eq!(theme_named("orchard").name, "orchard");
        assert_eq!(theme_named("baroque").name, DEFAULT_THEME);
        assert_eq!(theme_named("").name, DEFAULT_THEME);
    }
}
