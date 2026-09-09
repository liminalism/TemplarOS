//! The category palette and the pastel↔bold axis. Directories take their colour from what they
//! contain, so a tray reads its composition before its contents are read.

use crate::tree::extensions::{CATEGORY_COUNT, CATEGORY_ORDER, FileCategory, category_index};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    Category,
    #[default]
    CategoryExtension,
    ExtensionHash,
}

/// Ends of the pastel <-> bold slider. Everything that reads or writes
/// `ColorSettings::vibrancy` clamps to this range, so it lives here rather than
/// being repeated as literals across the UI.
pub const VIBRANCY_MIN: f32 = 0.6;
pub const VIBRANCY_MAX: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorSettings {
    pub mode: ColorMode,
    /// Position on the pastel <-> bold axis, in `VIBRANCY_MIN..=VIBRANCY_MAX`.
    pub vibrancy: f32,
}

impl Default for ColorSettings {
    fn default() -> Self {
        Self {
            mode: ColorMode::CategoryExtension,
            // Sits on the bold side of centre: the palette ships rich, with
            // headroom in both directions.
            vibrancy: 1.55,
        }
    }
}

/// Normalise a raw vibrancy setting to `0.0` (fully pastel) ..= `1.0` (fully bold).
pub fn vibrancy_t(vibrancy: f32) -> f32 {
    ((vibrancy.clamp(VIBRANCY_MIN, VIBRANCY_MAX) - VIBRANCY_MIN) / (VIBRANCY_MAX - VIBRANCY_MIN))
        .clamp(0.0, 1.0)
}

/// Our custom color representation for easy manipulation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl AppColor {
    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// `0x00RRGGBB`, the packing pixelkit draws with.
    pub fn to_rgb_u32(self) -> u32 {
        let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        (channel(self.r) << 16) | (channel(self.g) << 8) | channel(self.b)
    }

    /// Create a lighter version (for cushion highlight).
    #[must_use]
    pub fn lighten(self, amount: f32) -> Self {
        Self {
            r: (self.r + amount).min(1.0),
            g: (self.g + amount).min(1.0),
            b: (self.b + amount).min(1.0),
            a: self.a,
        }
    }

    /// Create a darker version (for cushion shadow).
    #[must_use]
    pub fn darken(self, amount: f32) -> Self {
        Self {
            r: (self.r - amount).max(0.0),
            g: (self.g - amount).max(0.0),
            b: (self.b - amount).max(0.0),
            a: self.a,
        }
    }
}

/// Dark mode color palette for file categories.
/// Categories follow a readable spectrum:
/// docs -> code -> images -> video -> audio -> archives -> system -> misc.
pub fn category_color(category: FileCategory) -> AppColor {
    let (h, s, v) = match category {
        FileCategory::Document => (215.0 / 360.0, 0.72, 0.95),
        FileCategory::Code => (175.0 / 360.0, 0.72, 0.91),
        FileCategory::Image => (132.0 / 360.0, 0.80, 0.95),
        FileCategory::Video => (42.0 / 360.0, 0.84, 0.97),
        FileCategory::Audio => (336.0 / 360.0, 0.76, 0.94),
        FileCategory::Archive => (282.0 / 360.0, 0.66, 0.90),
        FileCategory::System => return AppColor::new(0.56, 0.60, 0.68),
        FileCategory::Misc => (22.0 / 360.0, 0.48, 0.88),
    };
    hsv_to_rgb(h, s, v)
}

pub fn file_color(category: FileCategory, ext: &str, settings: &ColorSettings) -> AppColor {
    let base = category_color(category);
    let ext_norm = ext.trim_start_matches('.').to_ascii_lowercase();
    let adjusted = match settings.mode {
        ColorMode::Category => base,
        ColorMode::CategoryExtension => {
            let hue_jitter = hash01(&ext_norm) * 0.11 - 0.055;
            let sat_mul = 0.94 + hash01(&(ext_norm.clone() + "#sat")) * 0.16;
            shift_hsv(base, hue_jitter, sat_mul)
        }
        ColorMode::ExtensionHash => {
            let h = hash01(&ext_norm);
            hsv_to_rgb(h, 0.78, 0.88)
        }
    };
    apply_color_axis(adjusted, vibrancy_t(settings.vibrancy))
}

/// Backward-compatible helper for file color lookups by extension.
pub fn extension_color(ext: &str, settings: &ColorSettings) -> AppColor {
    let category = crate::tree::extensions::categorize_extension(ext);
    file_color(category, ext, settings)
}

/// Directories inherit their palette from the file types they contain so color remains
/// stable as you move between overview and drill-down levels.
pub fn directory_color(weights: &[u64; CATEGORY_COUNT], settings: &ColorSettings) -> AppColor {
    // Directories sit slightly toward the pastel end of wherever the slider is,
    // so a mixed folder reads softer than the files it contains. This shifts the
    // normalised position rather than the raw setting: scaling the raw value
    // would do nothing at the pastel end (it clamps) and a lot at the bold end.
    let t = vibrancy_t(settings.vibrancy);
    let total: u64 = weights.iter().sum();
    if total == 0 {
        return apply_color_axis(category_color(FileCategory::Misc), t * 0.9);
    }

    let top = top_categories(weights, 4);
    let mut r = 0.0;
    let mut g = 0.0;
    let mut b = 0.0;
    let mut sum = 0.0;
    for (category, weight) in top.iter().copied() {
        let share = weight as f32 / total as f32;
        let emphasis = share.powf(0.72);
        let c = category_color(category);
        r += c.r * emphasis;
        g += c.g * emphasis;
        b += c.b * emphasis;
        sum += emphasis;
    }

    if sum <= 1e-6 {
        return apply_color_axis(category_color(FileCategory::Misc), t * 0.9);
    }

    let dominant_share: f32 = top
        .first()
        .map_or(1.0, |(_, weight)| *weight as f32 / total as f32);
    // The more mixed the folder, the further toward pastel it drifts.
    let stability = 0.92 - (1.0 - dominant_share).min(0.45) * 0.25;
    apply_color_axis(AppColor::new(r / sum, g / sum, b / sum), t * stability)
}

pub fn top_categories(weights: &[u64; CATEGORY_COUNT], limit: usize) -> Vec<(FileCategory, u64)> {
    let mut entries: Vec<(FileCategory, u64)> = CATEGORY_ORDER
        .iter()
        .copied()
        .filter_map(|category| {
            let weight = weights[category_index(category)];
            (weight > 0).then_some((category, weight))
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    entries.truncate(limit.min(entries.len()));
    entries
}

pub fn mode_name(mode: ColorMode) -> &'static str {
    match mode {
        ColorMode::Category => "Category",
        ColorMode::CategoryExtension => "Cat+Ext",
        ColorMode::ExtensionHash => "Ext Hash",
    }
}

/// Move a colour along the pastel <-> bold axis.
///
/// The slider used to scale saturation directly, which wasted most of its
/// travel: the base palette already sits at s = 0.66..0.84, so anything above
/// ~1.2x clipped to fully saturated and every category collapsed onto the same
/// wall of colour. Nothing above the midpoint did anything.
///
/// Instead we interpolate between two derived palettes, moving saturation and
/// value together so the ends differ in weight and not only in colourfulness:
///
/// * pastel end - saturation pulled well down, value lifted toward white, for
///   soft tints that stay distinguishable by hue.
/// * bold end - saturation pushed most of the way to full, value deepened, for
///   rich colour that still leaves the cushion shading room to read.
///
/// Because both ends are derived from the base colour rather than clamped
/// against a ceiling, relative differences between categories survive at every
/// position on the slider.
fn apply_color_axis(color: AppColor, t: f32) -> AppColor {
    let (h, s, v) = rgb_to_hsv(color);
    let t = t.clamp(0.0, 1.0);

    let s_pastel = s * 0.30;
    let s_bold = s + (1.0 - s) * 0.55;
    let s_out = (s_pastel + (s_bold - s_pastel) * t).clamp(0.0, 1.0);

    // Deepening the bold end keeps saturated colour from glaring, and lifting
    // the pastel end is what makes it read as a tint rather than a faded one.
    let v_pastel = v + (1.0 - v) * 0.55;
    let v_bold = v * 0.86;
    let v_out = (v_pastel + (v_bold - v_pastel) * t).clamp(0.0, 1.0);

    hsv_to_rgb(h, s_out, v_out)
}

fn shift_hsv(color: AppColor, hue_delta: f32, sat_mul: f32) -> AppColor {
    let (mut h, mut s, v) = rgb_to_hsv(color);
    h = (h + hue_delta).rem_euclid(1.0);
    s = (s * sat_mul).clamp(0.0, 1.0);
    hsv_to_rgb(h, s, v)
}

fn rgb_to_hsv(c: AppColor) -> (f32, f32, f32) {
    let max = c.r.max(c.g.max(c.b));
    let min = c.r.min(c.g.min(c.b));
    let d = max - min;
    let h = if d <= 1e-6 {
        0.0
    } else if (max - c.r).abs() <= 1e-6 {
        ((c.g - c.b) / d).rem_euclid(6.0) / 6.0
    } else if (max - c.g).abs() <= 1e-6 {
        (((c.b - c.r) / d) + 2.0) / 6.0
    } else {
        (((c.r - c.g) / d) + 4.0) / 6.0
    };
    let s = if max <= 1e-6 { 0.0 } else { d / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> AppColor {
    let h6 = (h * 6.0).rem_euclid(6.0);
    let i = h6.floor() as i32;
    let f = h6 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    AppColor { r, g, b, a: 1.0 }
}

fn hash01(s: &str) -> f32 {
    let mut h: u32 = 2_166_136_261;
    for &b in s.as_bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(16_777_619);
    }
    ((h >> 8) as f32) / ((u32::MAX >> 8) as f32)
}
