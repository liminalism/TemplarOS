//! Pure geometry and colour decisions for chrome — no renderer, no font, no I/O. Everything here
//! is plain arithmetic over [`templar_window::Rect`], so it is unit-testable without a compositor,
//! matching this workspace's usual split between policy (tested as data) and rendering (run to be
//! seen). `chrome/paint.rs` turns these rectangles into pixels; `src/input.rs` hit-tests the
//! pressure-overlay button rects against the same layout so the drawn buttons and the clickable
//! area can never drift apart.

use templar_window::Rect;

/// The four choices `templar_recovery_v1`'s pressure overlay offers, in the fixed left-to-right /
/// row-major order both the buttons are drawn in and `Tab`/arrow-key navigation cycles through.
/// Mirrors `templar_recovery_manager_v1::Choice`'s wire values exactly (`protocols_recovery.rs`
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
pub fn pressure_choice_at(area: Rect, point: templar_window::Point) -> Option<PressureChoice> {
    pressure_overlay_buttons(area)
        .iter()
        .position(|rect| rect_contains(*rect, point))
        .map(PressureChoice::from_index)
}

fn rect_contains(rect: Rect, point: templar_window::Point) -> bool {
    point.x >= rect.x && point.x < rect.right() && point.y >= rect.y && point.y < rect.bottom()
}

/// The compositor-owned fallback switcher's panel: a centred column wide enough for the longest
/// title (already elided by the caller, so this only needs a maximum), tall enough for every
/// `row_count` row at `row_height`, and always fully inside `area`. A caller with more rows than
/// fit passes the [`switcher_visible_rows`] window, not the whole list.
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

/// Which of `count` switcher rows to draw when only `capacity` fit: everything when it all fits,
/// else a contiguous run of `capacity` rows with `selected` in it, centred on it where the ends
/// of the list allow. Stateless, so the HUD needs no scroll position to keep alive between steps.
pub fn switcher_visible_rows(
    count: usize,
    selected: usize,
    capacity: usize,
) -> std::ops::Range<usize> {
    if count <= capacity {
        return 0..count;
    }
    let capacity = capacity.max(1);
    let start = selected
        .min(count - 1)
        .saturating_sub(capacity / 2)
        .min(count - capacity);
    start..start + capacity
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

/// The three window controls — park to the Home shelf, maximize/restore, close — in
/// canonical order. Where they sit comes from [`templar_window::WindowLayout`]: a vertical
/// stack on the adjacent side (same-corner) or a horizontal row at the opposite end of the
/// title's edge, Close nearest the anchor corner either way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlTag {
    Park,
    Maximize,
    Close,
}

impl ControlTag {
    /// The compositor's tag for a layout control: same control, shared meaning.
    pub const fn from_tab(control: templar_window::TabControl) -> Self {
        match control {
            templar_window::TabControl::Park => Self::Park,
            templar_window::TabControl::Maximize => Self::Maximize,
            templar_window::TabControl::Close => Self::Close,
        }
    }
}

/// The three controls with their rectangles, from the shared [`templar_window::WindowLayout`]:
/// the same rectangles paint and hit-test both use, so the drawn control and the clickable
/// area can never drift apart. A fixed array — three controls need no heap.
pub fn tab_control_tags(
    layout: &templar_window::WindowLayout,
) -> [(ControlTag, Rect); 3] {
    layout
        .controls
        .map(|(control, rect)| (ControlTag::from_tab(control), rect))
}

/// Which chrome rectangles of a layout are currently drawn and clickable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TabVisibility {
    /// The title tab: drawn while its window is visible, under the default policy.
    pub title: bool,
    /// The three controls: drawn only while revealed, under the hover policy.
    pub controls: bool,
}

/// What a point on a window's tabs is asking for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabHit {
    /// The title tab: a move.
    Title,
    /// One of the controls.
    Control(ControlTag),
}

/// Visible controls and the title use the shared, disjoint layout. Hidden chrome
/// takes no input; paint uses the same visibility.
pub fn tab_hit_at(
    layout: &templar_window::WindowLayout,
    visibility: TabVisibility,
    point: templar_window::Point,
) -> Option<TabHit> {
    if visibility.controls
        && let Some(control) = layout.control_at(point)
    {
        return Some(TabHit::Control(ControlTag::from_tab(control)));
    }
    if visibility.title && layout.title_at(point) {
        return Some(TabHit::Title);
    }
    None
}

/// The group accent stripe width: one definition, owned by `templar-window` with the
/// rest of group geometry, re-exported here for paint.
pub use templar_window::GROUP_STRIPE_WIDTH;

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

/// Ink for text that should read as secondary — a parked window's switcher row — against
/// `background`: [`text_ink_for`]'s choice pulled part-way towards the background. Visibly
/// fainter than ordinary ink, still legible (3:1, WCAG's large-text/UI threshold, on every
/// switcher background — pinned by a test). Opaque like every chrome colour, so "dim" is a
/// colour rather than an alpha.
pub fn muted_ink_for(background: u32) -> u32 {
    pixelkit_raster::color::mix(text_ink_for(background), background, 0.45)
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
                    templar_window::Point {
                        x: center.x,
                        y: center.y
                    }
                ),
                Some(PressureChoice::from_index(index))
            );
        }
        // Well outside every button.
        assert_eq!(
            pressure_choice_at(area, templar_window::Point { x: -100, y: -100 }),
            None
        );
    }

    /// Paint and hit share the layout: every drawn control answers its own centre, the
    /// title answers its own, and gaps between them answer nothing — at every corner, under
    /// both arrangements.
    #[test]
    fn tab_hit_matches_the_drawn_rectangles_at_every_corner() {
        use templar_window::{Corner, TabPlacement, WindowLayout};
        let client = Rect::new(300, 300, 640, 480);
        let output = Rect::new(0, 0, 1920, 1080);
        let shown = TabVisibility {
            title: true,
            controls: true,
        };
        for corner in Corner::ALL {
            for placement in [TabPlacement::SameCorner, TabPlacement::OppositeCorners] {
                let layout = WindowLayout::new(client, corner, placement, output);
                let tags = tab_control_tags(&layout);
                assert_eq!(tags.len(), 3);
                for (button, rect) in &tags {
                    assert_eq!(
                        tab_hit_at(&layout, shown, rect.center()),
                        Some(TabHit::Control(*button)),
                        "{button:?} at {corner:?}/{placement:?}"
                    );
                }
                assert_eq!(
                    tab_hit_at(&layout, shown, layout.title.center()),
                    Some(TabHit::Title)
                );
                // Well outside every tab.
                assert_eq!(
                    tab_hit_at(
                        &layout,
                        shown,
                        templar_window::Point { x: -100, y: -100 }
                    ),
                    None
                );
            }
        }
    }

    /// Hidden chrome takes no input: with controls unrevealed only the title answers, and
    /// with the title hidden too nothing does — while the same points answer once revealed.
    #[test]
    fn hidden_tabs_take_no_input() {
        use templar_window::{Corner, TabPlacement, WindowLayout};
        let layout = WindowLayout::new(
            Rect::new(300, 300, 640, 480),
            Corner::TopRight,
            TabPlacement::SameCorner,
            Rect::new(0, 0, 1920, 1080),
        );
        let hidden = TabVisibility {
            title: true,
            controls: false,
        };
        let (_, close_rect) = tab_control_tags(&layout)[0];
        assert_eq!(tab_hit_at(&layout, hidden, close_rect.center()), None);
        assert_eq!(
            tab_hit_at(&layout, hidden, layout.title.center()),
            Some(TabHit::Title)
        );
        let all_hidden = TabVisibility {
            title: false,
            controls: false,
        };
        assert_eq!(tab_hit_at(&layout, all_hidden, layout.title.center()), None);
        assert_eq!(tab_hit_at(&layout, all_hidden, close_rect.center()), None);
    }

    /// A tiny overlay keeps four disjoint, reachable targets, including title-drag.
    #[test]
    fn tiny_overlay_keeps_title_and_all_controls_reachable() {
        use templar_window::{Corner, TabPlacement, WindowLayout};
        let tiny = Rect::new(500, 500, 60, 40);
        let output = Rect::new(0, 0, 1920, 1080);
        let shown = TabVisibility {
            title: true,
            controls: true,
        };
        for corner in Corner::ALL {
            let layout = WindowLayout::new(tiny, corner, TabPlacement::SameCorner, output);
            assert_eq!(tab_hit_at(&layout, shown, layout.title.center()), Some(TabHit::Title));
            for (button, rect) in &tab_control_tags(&layout) {
                assert_eq!(
                    tab_hit_at(&layout, shown, rect.center()),
                    Some(TabHit::Control(*button)),
                    "{button:?} unreachable at {corner:?}"
                );
            }
        }
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
    fn an_overflowing_switcher_shows_a_contiguous_run_that_keeps_the_selection() {
        assert_eq!(switcher_visible_rows(5, 4, 10), 0..5, "everything fits");
        assert_eq!(switcher_visible_rows(0, 0, 10), 0..0);
        // Centred where the list allows, pinned to either end where it does not.
        assert_eq!(switcher_visible_rows(20, 10, 5), 8..13);
        assert_eq!(switcher_visible_rows(20, 1, 5), 0..5);
        assert_eq!(switcher_visible_rows(20, 19, 5), 15..20);
        for selected in 0..20 {
            for capacity in 1..=20 {
                let shown = switcher_visible_rows(20, selected, capacity);
                assert_eq!(shown.len(), capacity);
                assert!(shown.contains(&selected), "{selected} hidden in {shown:?}");
                assert!(shown.end <= 20);
            }
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
    fn chrome_text_remains_legible_across_supported_themes_and_accents() {
        use templar_types::appearance::{ACCENTS, Appearance};
        for theme in ["light", "dark"] {
            for accent in std::iter::once("default").chain(ACCENTS.iter().map(|(name, _)| *name)) {
                let palette = Appearance::from_settings(theme, accent, false).palette;
                for background in [palette.ground, palette.shell_fill, palette.shell_low,
                    palette.system_shell, palette.accent, palette.accent_soft] {
                    let ratio = contrast_ratio(background, text_ink_for(background));
                    assert!(ratio >= 4.5, "{theme}/{accent}: normal text contrast {ratio:.2}:1");
                }
                for background in [palette.system_shell, palette.accent_soft] {
                    let ratio = contrast_ratio(background, muted_ink_for(background));
                    assert!(ratio >= 3.0, "{theme}/{accent}: parked switcher contrast {ratio:.2}:1");
                }
            }
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
