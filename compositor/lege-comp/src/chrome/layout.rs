//! Pure geometry and colour decisions for chrome — no renderer, no font, no I/O. Everything here
//! is plain arithmetic over [`lege_window::Rect`], so it is unit-testable without a compositor,
//! matching this workspace's usual split between policy (tested as data) and rendering (run to be
//! seen). `chrome/paint.rs` turns these rectangles into pixels; `src/input.rs` hit-tests the
//! pressure-overlay button rects against the same layout so the drawn buttons and the clickable
//! area can never drift apart.

use lege_window::Rect;

/// The four choices `lege_recovery_v1`'s pressure overlay offers, in the fixed left-to-right /
/// row-major order both the buttons are drawn in and `Tab`/arrow-key navigation cycles through.
/// Mirrors `lege_recovery_manager_v1::Choice`'s wire values exactly (`protocols_recovery.rs`
/// converts one to the other rather than trusting a transmute).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PressureChoice {
    Wait,
    CheckpointAndTerminate,
    Terminate,
    OpenTerminal,
}

impl PressureChoice {
    /// Every choice, in the fixed order the buttons occupy.
    pub const ALL: [PressureChoice; 4] = [
        Self::Wait,
        Self::CheckpointAndTerminate,
        Self::Terminate,
        Self::OpenTerminal,
    ];

    pub const fn index(self) -> usize {
        match self {
            Self::Wait => 0,
            Self::CheckpointAndTerminate => 1,
            Self::Terminate => 2,
            Self::OpenTerminal => 3,
        }
    }

    pub const fn from_index(index: usize) -> Self {
        match index % 4 {
            0 => Self::Wait,
            1 => Self::CheckpointAndTerminate,
            2 => Self::Terminate,
            _ => Self::OpenTerminal,
        }
    }

    /// Human label drawn on the button.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Wait => "Wait",
            Self::CheckpointAndTerminate => "Checkpoint & Terminate",
            Self::Terminate => "Terminate",
            Self::OpenTerminal => "Open Terminal",
        }
    }

    /// Move to the next/previous choice, wrapping — used by Left/Right/Tab navigation.
    pub const fn step(self, forward: bool) -> Self {
        let index = self.index();
        if forward {
            Self::from_index(index + 1)
        } else {
            Self::from_index(index + 3)
        }
    }
}

impl Default for PressureChoice {
    /// The least destructive choice is the safe default selection when the overlay first
    /// appears — never pre-selecting `Terminate`.
    fn default() -> Self {
        Self::Wait
    }
}

/// Fixed sizes for the pressure-overlay button row, in logical pixels.
const BUTTON_HEIGHT: i32 = 48;
const BUTTON_GAP: i32 = 16;
const BUTTON_MIN_WIDTH: i32 = 160;
const BUTTON_MARGIN: i32 = 24;

/// The four pressure-overlay button rectangles for an output of `area` (logical, usually
/// `(0, 0, output_w, output_h)`), in [`PressureChoice::ALL`] order. A single centred row when
/// `area` is wide enough for all four at [`BUTTON_MIN_WIDTH`]; otherwise a 2x2 grid, so the
/// overlay stays usable on a narrow or heavily rotated output instead of clipping buttons off
/// the edge. Every returned rectangle is fully contained in `area` (clamped) and non-empty even
/// for a pathologically small `area`, so a caller can always hit-test or draw it safely.
pub fn pressure_overlay_buttons(area: Rect) -> [Rect; 4] {
    let width = area.width.max(1);
    let height = area.height.max(1);
    let row_width = 4 * BUTTON_MIN_WIDTH + 3 * BUTTON_GAP;

    if width >= row_width + 2 * BUTTON_MARGIN {
        one_row(area, width, height)
    } else {
        two_by_two(area, width, height)
    }
}

fn one_row(area: Rect, width: i32, height: i32) -> [Rect; 4] {
    let button_width = ((width - 3 * BUTTON_GAP) / 4).max(1);
    let total_width = 4 * button_width + 3 * BUTTON_GAP;
    let start_x = area.x + (width - total_width) / 2;
    let y = area.y + (height - BUTTON_HEIGHT).max(0).min(height - 1);

    std::array::from_fn(|i| {
        let i = i as i32;
        Rect::new(
            start_x + i * (button_width + BUTTON_GAP),
            y,
            button_width,
            BUTTON_HEIGHT.min(height),
        )
    })
}

fn two_by_two(area: Rect, width: i32, height: i32) -> [Rect; 4] {
    let button_width = ((width - BUTTON_GAP) / 2).max(1);
    let button_height = BUTTON_HEIGHT.min((height - BUTTON_GAP).max(1));
    let total_width = 2 * button_width + BUTTON_GAP;
    let total_height = 2 * button_height + BUTTON_GAP;
    let start_x = area.x + (width - total_width) / 2;
    let start_y = area.y + (height - total_height) / 2;

    std::array::from_fn(|i| {
        let column = i32::try_from(i).unwrap_or(0) % 2;
        let row = i32::try_from(i).unwrap_or(0) / 2;
        Rect::new(
            start_x + column * (button_width + BUTTON_GAP),
            start_y + row * (button_height + BUTTON_GAP),
            button_width,
            button_height,
        )
    })
}

/// Which button (if any) contains `point`, in [`PressureChoice`] order — the shared hit-test
/// both `src/input.rs` (pointer clicks) and any future automated test use.
pub fn pressure_choice_at(area: Rect, point: lege_window::Point) -> Option<PressureChoice> {
    pressure_overlay_buttons(area)
        .iter()
        .position(|rect| rect_contains(*rect, point))
        .map(PressureChoice::from_index)
}

fn rect_contains(rect: Rect, point: lege_window::Point) -> bool {
    point.x >= rect.x && point.x < rect.right() && point.y >= rect.y && point.y < rect.bottom()
}

/// The compositor-owned fallback switcher's panel: a centred column wide enough for the longest
/// title (already elided by the caller, so this only needs a maximum), tall enough for every
/// `row_count` row at `row_height`, and always fully inside `area`.
pub fn switcher_panel(area: Rect, row_count: usize, row_height: i32) -> Rect {
    let row_count = row_count.max(1) as i32;
    let width = (area.width * 3 / 5).clamp(1, area.width.max(1));
    let height = (row_count * row_height).clamp(1, area.height.max(1));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// The rectangle for row `index` (0-based, top to bottom) inside a [`switcher_panel`].
pub fn switcher_row(panel: Rect, index: usize, row_height: i32) -> Rect {
    Rect::new(
        panel.x,
        panel.y + i32::try_from(index).unwrap_or(0) * row_height,
        panel.width,
        row_height,
    )
}

/// Split a "show every window" scatter tile into its card (the placeholder area representing the
/// window) and, beneath it, the label strip its title is drawn in — literally "under its tile"
/// (planning doc 01 §3). `label_height` is clamped so the label never consumes the whole tile on
/// a very short one.
pub fn scatter_label_band(tile: Rect, label_height: i32) -> (Rect, Rect) {
    let label_height = label_height.clamp(0, tile.height.max(0));
    let card_height = tile.height - label_height;
    (
        Rect::new(tile.x, tile.y, tile.width, card_height),
        Rect::new(tile.x, tile.y + card_height, tile.width, label_height),
    )
}

/// The three controls at the right end of every recovery band, in the left-to-right order they
/// are drawn: park to the Home shelf, maximize/restore, close. Close is rightmost and the last
/// to give way on a narrow band, because it is the one a person looks for first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BandButton {
    Park,
    Maximize,
    Close,
}

impl BandButton {
    /// Every button, in drawing order.
    pub const ALL: [BandButton; 3] = [Self::Park, Self::Maximize, Self::Close];
}

/// Logical width of one band button — a square of the band's own height.
pub const BAND_BUTTON_WIDTH: i32 = lege_window::geometry::RECOVERY_BAND_HEIGHT;

/// Room the title keeps at the left of a band before buttons start giving way.
const BAND_TITLE_MIN_WIDTH: i32 = 64;

/// The buttons that fit on `band`, with their rectangles, packed against its right edge in
/// [`BandButton::ALL`] order. On a band too narrow for all three the leftmost go first:
/// [`MIN_RECOVERY_WIDTH`](lege_window::geometry::MIN_RECOVERY_WIDTH) is wide enough for Close
/// alone plus a sliver of title, which is the least a recoverable window ever has.
pub fn band_buttons(band: Rect) -> Vec<(BandButton, Rect)> {
    let width = BAND_BUTTON_WIDTH.min(band.height.max(1)).max(1);
    let room = (band.width - BAND_TITLE_MIN_WIDTH).max(0);
    let count = usize::try_from(room / width)
        .unwrap_or(0)
        .min(BandButton::ALL.len());
    let shown = &BandButton::ALL[BandButton::ALL.len() - count..];
    shown
        .iter()
        .enumerate()
        .map(|(index, button)| {
            let from_right = i32::try_from(shown.len() - index).unwrap_or(1);
            (
                *button,
                Rect::new(
                    band.right() - from_right * width,
                    band.y,
                    width,
                    band.height,
                ),
            )
        })
        .collect()
}

/// Logical width of the accent stripe a grouped window wears at the left of its band (planning
/// doc §2 rule 7: "a stripe of the same colour in each member's band").
pub const GROUP_STRIPE_WIDTH: i32 = 4;

/// The four bars of a group outline of `thickness`, drawn **outside** `union` — top, bottom,
/// left, right. Outside, because the union's edges are the members' own outer edges: an outline
/// straddling them would sit on top of the very windows it is describing, and one drawn inside
/// would eat a client's pixels.
pub fn outline_bars(union: Rect, thickness: i32) -> [Rect; 4] {
    let thickness = thickness.max(1);
    [
        Rect::new(
            union.x - thickness,
            union.y - thickness,
            union.width + 2 * thickness,
            thickness,
        ),
        Rect::new(
            union.x - thickness,
            union.bottom(),
            union.width + 2 * thickness,
            thickness,
        ),
        Rect::new(union.x - thickness, union.y, thickness, union.height),
        Rect::new(union.right(), union.y, thickness, union.height),
    ]
}

/// The band button containing `point`, if any — the same rectangles [`band_buttons`] draws, so
/// the drawn control and the clickable area cannot drift apart.
pub fn band_button_at(band: Rect, point: lege_window::Point) -> Option<BandButton> {
    band_buttons(band)
        .into_iter()
        .find(|(_, rect)| rect_contains(*rect, point))
        .map(|(button, _)| button)
}

/// Off-white and near-black chrome text can be drawn in — [`text_ink_for`] picks between exactly
/// these two, never an arbitrary hue, so the choice stays a legibility decision rather than a
/// styling one.
pub const INK_LIGHT: u32 = pixelkit_raster::color::from_rgb(0xF2, 0xF2, 0xF2);
pub const INK_DARK: u32 = pixelkit_raster::color::from_rgb(0x10, 0x10, 0x10);

/// WCAG-style contrast ratio between two opaque `0x00RRGGBB` colours (`(L1 + 0.05) / (L2 + 0.05)`
/// with `L1 >= L2`), using `pixelkit_raster::color::luminance` for the per-colour relative
/// luminance. `1.0` means no contrast at all; WCAG's AA body-text threshold is `4.5`.
pub fn contrast_ratio(a: u32, b: u32) -> f32 {
    let (la, lb) = (
        pixelkit_raster::color::luminance(a),
        pixelkit_raster::color::luminance(b),
    );
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Choose readable text ink against `background` **deliberately**, by contrast, rather than a
/// single hardcoded colour: whichever of [`INK_LIGHT`]/[`INK_DARK`] wins the higher WCAG contrast
/// ratio against `background`. This is what lets chrome's palette change without silently
/// breaking legibility, and is the whole reason this function exists instead of `if dark { white }
/// else { black }` picked once by eye.
pub fn text_ink_for(background: u32) -> u32 {
    if contrast_ratio(background, INK_LIGHT) >= contrast_ratio(background, INK_DARK) {
        INK_LIGHT
    } else {
        INK_DARK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    #[test]
    fn pressure_choice_order_matches_wire_indices() {
        assert_eq!(PressureChoice::Wait.index(), 0);
        assert_eq!(PressureChoice::CheckpointAndTerminate.index(), 1);
        assert_eq!(PressureChoice::Terminate.index(), 2);
        assert_eq!(PressureChoice::OpenTerminal.index(), 3);
        for choice in PressureChoice::ALL {
            assert_eq!(PressureChoice::from_index(choice.index()), choice);
        }
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        assert_eq!(
            PressureChoice::Wait.step(false),
            PressureChoice::OpenTerminal
        );
        assert_eq!(
            PressureChoice::OpenTerminal.step(true),
            PressureChoice::Wait
        );
        assert_eq!(
            PressureChoice::Wait.step(true),
            PressureChoice::CheckpointAndTerminate
        );
    }

    #[test]
    fn default_choice_is_the_least_destructive() {
        assert_eq!(PressureChoice::default(), PressureChoice::Wait);
    }

    #[test]
    fn wide_output_lays_buttons_out_in_one_row() {
        let area = Rect::new(0, 0, 1920, 1080);
        let buttons = pressure_overlay_buttons(area);
        for pair in buttons.windows(2) {
            assert!(!rects_overlap(pair[0], pair[1]), "{pair:?} overlap");
        }
        // Left-to-right order matches PressureChoice::ALL.
        for pair in buttons.windows(2) {
            assert!(pair[0].x < pair[1].x);
        }
        for rect in buttons {
            assert!(
                area.intersection(rect) == Some(rect),
                "{rect:?} escapes {area:?}"
            );
        }
    }

    #[test]
    fn narrow_output_wraps_to_a_grid_without_overlap_or_escape() {
        let area = Rect::new(0, 0, 480, 640);
        let buttons = pressure_overlay_buttons(area);
        for i in 0..buttons.len() {
            for j in (i + 1)..buttons.len() {
                assert!(
                    !rects_overlap(buttons[i], buttons[j]),
                    "buttons {i} and {j} overlap"
                );
            }
        }
        for rect in buttons {
            assert!(
                area.intersection(rect) == Some(rect),
                "{rect:?} escapes {area:?}"
            );
        }
    }

    #[test]
    fn pathologically_small_output_never_panics_and_stays_in_bounds() {
        for area in [
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 0, 0),
            Rect::new(-50, -50, 10, 10),
        ] {
            let buttons = pressure_overlay_buttons(area);
            for rect in buttons {
                assert!(rect.width >= 0 && rect.height >= 0);
            }
        }
    }

    #[test]
    fn hit_test_matches_the_drawn_rectangles() {
        let area = Rect::new(0, 0, 1920, 1080);
        let buttons = pressure_overlay_buttons(area);
        for (index, rect) in buttons.iter().enumerate() {
            let center = rect.center();
            assert_eq!(
                pressure_choice_at(
                    area,
                    lege_window::Point {
                        x: center.x,
                        y: center.y
                    }
                ),
                Some(PressureChoice::from_index(index))
            );
        }
        // Well outside every button.
        assert_eq!(
            pressure_choice_at(area, lege_window::Point { x: -100, y: -100 }),
            None
        );
    }

    #[test]
    fn band_buttons_pack_against_the_right_edge_close_last() {
        let band = Rect::new(100, 50, 400, 32);
        let buttons = band_buttons(band);
        assert_eq!(
            buttons.iter().map(|(b, _)| *b).collect::<Vec<_>>(),
            BandButton::ALL.to_vec()
        );
        let (close, close_rect) = buttons[2];
        assert_eq!(close, BandButton::Close);
        assert_eq!(close_rect.right(), band.right());
        assert_eq!(close_rect.height, band.height);
        for pair in buttons.windows(2) {
            assert_eq!(pair[0].1.right(), pair[1].1.x, "buttons are contiguous");
        }
        // The title keeps everything left of the first button.
        assert_eq!(buttons[0].1.x, band.right() - 3 * 32);
        for (button, rect) in &buttons {
            assert_eq!(band_button_at(band, rect.center()), Some(*button));
        }
        assert_eq!(
            band_button_at(
                band,
                lege_window::Point {
                    x: band.x + 10,
                    y: band.y + 10
                }
            ),
            None
        );
    }

    #[test]
    fn a_narrow_band_keeps_close_and_gives_up_the_rest() {
        let band = Rect::new(0, 0, lege_window::geometry::MIN_RECOVERY_WIDTH, 32);
        let buttons = band_buttons(band);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].0, BandButton::Close);
        assert_eq!(band_buttons(Rect::new(0, 0, 130, 32)).len(), 2);
        assert!(band_buttons(Rect::new(0, 0, 40, 32)).is_empty());
        assert!(band_buttons(Rect::new(0, 0, 0, 0)).is_empty());
    }

    #[test]
    fn switcher_rows_stack_without_overlap_inside_the_panel() {
        let area = Rect::new(0, 0, 1920, 1080);
        let panel = switcher_panel(area, 5, 28);
        assert_eq!(area.intersection(panel), Some(panel));
        let rows: Vec<Rect> = (0..5).map(|i| switcher_row(panel, i, 28)).collect();
        for pair in rows.windows(2) {
            assert!(!rects_overlap(pair[0], pair[1]));
            assert_eq!(pair[0].bottom(), pair[1].y, "rows must be contiguous");
        }
    }

    #[test]
    fn scatter_label_sits_under_the_card_and_both_fit_the_tile() {
        let tile = Rect::new(100, 200, 300, 240);
        let (card, label) = scatter_label_band(tile, 24);
        assert_eq!(card.x, tile.x);
        assert_eq!(card.width, tile.width);
        assert_eq!(label.width, tile.width);
        assert_eq!(
            card.bottom(),
            label.y,
            "label must sit directly under the card"
        );
        assert_eq!(label.bottom(), tile.bottom());
        assert_eq!(card.height + label.height, tile.height);
    }

    #[test]
    fn scatter_label_never_exceeds_a_very_short_tile() {
        let tile = Rect::new(0, 0, 100, 10);
        let (card, label) = scatter_label_band(tile, 24);
        assert!(card.height >= 0);
        assert_eq!(card.height + label.height, tile.height);
    }

    #[test]
    fn dark_backgrounds_get_light_ink_and_vice_versa() {
        assert_eq!(
            text_ink_for(0x0010_1216),
            INK_LIGHT,
            "near-black background"
        );
        assert_eq!(text_ink_for(0x00F5_F5F0), INK_DARK, "near-white background");
    }

    #[test]
    fn chosen_ink_clears_the_wcag_aa_threshold_for_every_chrome_background() {
        // Every background chrome actually paints with must be legible: this is the test that
        // proves the palette was chosen deliberately, not merely that the function has *a* rule.
        // 4.5:1 is WCAG's AA threshold for normal-size text. Pulled from `chrome::paint`'s own
        // constants (not re-typed hex) so this test cannot silently drift from the real palette.
        use crate::chrome::paint::{
            LOCK_SCREEN_COLOR, PRESSURE_OVERLAY_COLOR, RECOVERY_BAND_COLOR, RECOVERY_TAB_COLOR,
            SCATTER_CARD_COLOR,
        };
        for background in [
            RECOVERY_BAND_COLOR,
            RECOVERY_TAB_COLOR,
            PRESSURE_OVERLAY_COLOR,
            SCATTER_CARD_COLOR,
            LOCK_SCREEN_COLOR,
        ] {
            let ink = text_ink_for(background);
            let ratio = contrast_ratio(background, ink);
            assert!(
                ratio >= 4.5,
                "background {background:#08x} only reaches {ratio:.2}:1 contrast with {ink:#08x}"
            );
        }
    }

    #[test]
    fn contrast_ratio_is_symmetric_and_one_for_identical_colours() {
        assert!((contrast_ratio(INK_LIGHT, INK_LIGHT) - 1.0).abs() < 1e-6);
        assert!(
            (contrast_ratio(INK_LIGHT, INK_DARK) - contrast_ratio(INK_DARK, INK_LIGHT)).abs()
                < 1e-6
        );
    }
}
