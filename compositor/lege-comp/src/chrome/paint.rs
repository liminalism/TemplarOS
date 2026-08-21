//! Pixel-level drawing for every chrome widget, via `pixelkit-raster`'s [`Painter`] and, when a
//! font was found, `pixelkit-text`'s [`TextCache`]. Every function here draws onto a buffer that
//! is *already* the whole widget's own canvas at `(0, 0)` — `chrome/cache.rs` owns sizing,
//! upload and reuse; this module only ever fills pixels.
//!
//! Every background here is drawn fully opaque. That is deliberate, not an oversight: the
//! recovery tab in particular exists because "the client may draw over that area"
//! (`lege_window::geometry`'s doc on `RecoveryAffordance`) — a translucent tab would still let
//! client content show through and undermine the one property it exists to guarantee. The same
//! reasoning applies to the pressure overlay and the lock screen: chrome that exists for
//! emergencies must be legible against *anything* that might be under it, and opaque plus a
//! deliberately contrast-picked ink (`chrome::layout::text_ink_for`) is what makes that true by
//! construction rather than by hoping the client painted something dim.
//!
//! Every function takes `fonts: Option<(&mut TextCache, FaceId)>` — `None` is
//! [`text::ChromeFonts::discover`] having found nothing, and draws the plain filled rectangle
//! alone, matching this module's pre-pixelkit placeholder behaviour exactly. No function here
//! panics on that path, and none of them ever construct a `TextStyle` from a `FaceId` that did
//! not come from the same call's `fonts` — the one real panic risk `pixelkit-text` has
//! (`FontSet::face` on an unregistered id) never arises because a `FaceId` is never stored or
//! reused across a font swap.

use crate::chrome::layout::{self, PressureChoice};
use lege_window::Rect as LRect;
use pixelkit_raster::{Painter, Rect as PxRect};
use pixelkit_text::{FaceId, TextCache, TextStyle, cache::Align};

/// Text and cache together, or nothing at all — the one parameter every drawing function below
/// threads through instead of taking `Option<&ChromeFonts>` directly, so this module never needs
/// to know `ChromeFonts`'s own shape.
pub type Fonts<'a> = Option<(&'a mut TextCache, FaceId)>;

pub const RECOVERY_BAND_COLOR: u32 = pixelkit_raster::color::from_rgb(0x2E, 0x33, 0x3D);
pub const RECOVERY_TAB_COLOR: u32 = pixelkit_raster::color::from_rgb(0xD9, 0x8C, 0x1A);
pub const PRESSURE_OVERLAY_COLOR: u32 = pixelkit_raster::color::from_rgb(0x73, 0x14, 0x14);
pub const SCATTER_CARD_COLOR: u32 = pixelkit_raster::color::from_rgb(0x1F, 0x1F, 0x29);
pub const SCATTER_LABEL_COLOR: u32 = pixelkit_raster::color::from_rgb(0x14, 0x15, 0x1B);
pub const SCATTER_FOCUS_ACCENT: u32 = pixelkit_raster::color::from_rgb(0x4E, 0x9A, 0xE8);
pub const SWITCHER_PANEL_COLOR: u32 = pixelkit_raster::color::from_rgb(0x1A, 0x1C, 0x22);
pub const SWITCHER_SELECTED_COLOR: u32 = pixelkit_raster::color::from_rgb(0x35, 0x47, 0x63);
pub const LOCK_SCREEN_COLOR: u32 = pixelkit_raster::color::from_rgb(0x14, 0x16, 0x1A);
pub const BUTTON_COLOR: u32 = pixelkit_raster::color::from_rgb(0x3A, 0x3F, 0x4B);
pub const BUTTON_SELECTED_COLOR: u32 = pixelkit_raster::color::from_rgb(0x4E, 0x9A, 0xE8);

/// Scale a *logical* rectangle up to the physical pixel space a widget's own buffer is drawn in
/// (`chrome/cache.rs`'s `buffer_scale`) — used for anything laid out once in logical coordinates
/// (`chrome::layout`) and then drawn into a buffer that was rasterised at `scale`x that size.
pub fn scaled_px(rect: LRect, scale: f32) -> PxRect {
    PxRect::new(
        (rect.x as f32 * scale).round() as i32,
        (rect.y as f32 * scale).round() as i32,
        (rect.width as f32 * scale).round() as i32,
        (rect.height as f32 * scale).round() as i32,
    )
}

/// The whole-buffer rectangle a widget's paint closure receives, as a [`PxRect`].
fn canvas(pixel_w: i32, pixel_h: i32) -> PxRect {
    PxRect::new(0, 0, pixel_w, pixel_h)
}

/// A window's recovery band or recovery tab (`lege_window::geometry::recovery_affordance`):
/// opaque `background`, the window's title elided to fit, in contrast-picked ink. With no font,
/// exactly the flat rectangle chrome drew before this module existed.
pub fn recovery_band(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    background: u32,
    title: &str,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, background);
    let Some((cache, face)) = fonts else { return };
    if title.is_empty() {
        return;
    }

    let ink = layout::text_ink_for(background);
    let style = TextStyle::new(face, (14.0 * scale).max(8.0));
    let padding = (8.0 * scale).round() as i32;
    let text_area = PxRect::new(
        area.x + padding,
        area.y,
        (area.w - 2 * padding).max(0),
        area.h,
    );
    let line_height = cache.line_height(style);
    let y = area.y + ((area.h - line_height) / 2).max(0);
    cache.draw_fitted(painter, title, text_area, y, style, ink, Align::Left);
}

/// The pressure overlay (`lege_recovery_v1.show_pressure_overlay`): the message, the offending
/// application if named, and the four choices as buttons — the selected one visually distinct so
/// keyboard navigation (`src/input.rs`) is visible without a pointer. `buttons` are already in
/// this buffer's own physical pixel space (`scaled_px` over `chrome::layout::pressure_overlay_buttons`).
#[allow(clippy::too_many_arguments)]
pub fn pressure_overlay(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    message: &str,
    offender: Option<&str>,
    buttons: &[PxRect; 4],
    selected: PressureChoice,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, PRESSURE_OVERLAY_COLOR);

    draw_buttons(painter, buttons, selected, scale);

    let Some((cache, face)) = fonts else { return };
    let ink = layout::text_ink_for(PRESSURE_OVERLAY_COLOR);
    let margin = (48.0 * scale).round() as i32;
    let body_style = TextStyle::new(face, (18.0 * scale).max(9.0));
    let offender_style = TextStyle::new(face, (14.0 * scale).max(8.0));

    let text_top = area.y + margin;
    let text_bottom = buttons.iter().map(|b| b.y).min().unwrap_or(area.bottom()) - margin;
    let text_area = PxRect::new(
        area.x + margin,
        text_top,
        (area.w - 2 * margin).max(0),
        (text_bottom - text_top).max(0),
    );

    let mut y = text_area.y;
    let line_height = cache.line_height(body_style).max(1);
    // Bounded rather than open-ended: a pathologically long message must not push chrome into
    // drawing forever off the bottom of the overlay — see this module's doc.
    let max_lines = (text_area.h / line_height).clamp(1, 12) as usize;
    for range in cache
        .wrap(message, text_area.w, body_style)
        .iter()
        .take(max_lines)
    {
        cache.draw(
            painter,
            &message[range.clone()],
            text_area.x,
            y,
            body_style,
            ink,
        );
        y += line_height;
    }

    if let Some(offender) = offender.filter(|s| !s.is_empty()) {
        y += line_height / 2;
        cache.draw_fitted(
            painter,
            offender,
            text_area,
            y,
            offender_style,
            ink,
            Align::Left,
        );
    }

    // Button labels, drawn after the backgrounds above so they are never occluded.
    for (index, rect) in buttons.iter().enumerate() {
        let choice = PressureChoice::from_index(index);
        let background = if choice == selected {
            BUTTON_SELECTED_COLOR
        } else {
            BUTTON_COLOR
        };
        let label_ink = layout::text_ink_for(background);
        let label_style = TextStyle::new(face, (14.0 * scale).max(8.0));
        let label_height = cache.line_height(label_style);
        let label_y = rect.y + ((rect.h - label_height) / 2).max(0);
        cache.draw_fitted(
            painter,
            choice.label(),
            *rect,
            label_y,
            label_style,
            label_ink,
            Align::Centre,
        );
    }
}

/// Just the button backgrounds (rounded rects), independent of whether text can be drawn on top —
/// this is what keeps keyboard navigation visible even on the no-font fallback path.
fn draw_buttons(
    painter: &mut Painter<'_>,
    buttons: &[PxRect; 4],
    selected: PressureChoice,
    scale: f32,
) {
    let radius = (6.0 * scale).round() as i32;
    for (index, rect) in buttons.iter().enumerate() {
        let choice = PressureChoice::from_index(index);
        let color = if choice == selected {
            BUTTON_SELECTED_COLOR
        } else {
            BUTTON_COLOR
        };
        painter.fill_rounded_rect(*rect, radius, color);
    }
}

/// One "show every window" tile's card (the placeholder standing in for the window) and, drawn
/// separately by [`scatter_label`], its title strip underneath.
pub fn scatter_card(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    focused: bool,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, SCATTER_CARD_COLOR);
    if focused {
        let thickness = (3.0 * scale).round().max(1.0) as i32;
        painter.stroke_rect(area, thickness, SCATTER_FOCUS_ACCENT);
    }
}

/// The label strip under a scatter tile's card — the window's title, elided to fit, so tiles "can
/// be told apart" (planning doc 01 §3) even before pixelkit landed here.
pub fn scatter_label(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    title: &str,
    focused: bool,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    let background = if focused {
        SCATTER_FOCUS_ACCENT
    } else {
        SCATTER_LABEL_COLOR
    };
    painter.fill_rect(area, background);
    let Some((cache, face)) = fonts else { return };
    if title.is_empty() {
        return;
    }
    let ink = layout::text_ink_for(background);
    let style = TextStyle::new(face, (13.0 * scale).max(8.0));
    let padding = (6.0 * scale).round() as i32;
    let text_area = PxRect::new(
        area.x + padding,
        area.y,
        (area.w - 2 * padding).max(0),
        area.h,
    );
    let line_height = cache.line_height(style);
    let y = area.y + ((area.h - line_height) / 2).max(0);
    cache.draw_fitted(painter, title, text_area, y, style, ink, Align::Left);
}

/// One row of the fallback switcher (`WindowService::cycle_focus`) — a title, highlighted if it
/// is the current selection.
pub fn switcher_row(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    title: &str,
    selected: bool,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    let background = if selected {
        SWITCHER_SELECTED_COLOR
    } else {
        SWITCHER_PANEL_COLOR
    };
    painter.fill_rect(area, background);
    let Some((cache, face)) = fonts else { return };
    if title.is_empty() {
        return;
    }
    let ink = layout::text_ink_for(background);
    let style = TextStyle::new(face, (15.0 * scale).max(8.0));
    let padding = (10.0 * scale).round() as i32;
    let text_area = PxRect::new(
        area.x + padding,
        area.y,
        (area.w - 2 * padding).max(0),
        area.h,
    );
    let line_height = cache.line_height(style);
    let y = area.y + ((area.h - line_height) / 2).max(0);
    cache.draw_fitted(painter, title, text_area, y, style, ink, Align::Left);
}

/// What a locked session shows before the greeter (a real `ext-session-lock-v1` client) takes
/// over — deliberately plain (task brief): a flat colour and, if a font is available, one short
/// line of static text. No branding, no imagery, nothing that could itself fail to load.
pub fn lock_screen(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, LOCK_SCREEN_COLOR);
    let Some((cache, face)) = fonts else { return };
    let ink = layout::text_ink_for(LOCK_SCREEN_COLOR);
    let style = TextStyle::new(face, (20.0 * scale).max(10.0));
    let text = "Session locked";
    let width = cache.measure(text, style);
    let line_height = cache.line_height(style);
    let x = area.x + ((area.w - width) / 2).max(0);
    let y = area.y + ((area.h - line_height) / 2).max(0);
    cache.draw(painter, text, x, y, style, ink);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelkit_raster::WindowBuffer;

    fn pixel_at(buffer: &WindowBuffer, x: i32, y: i32) -> u32 {
        buffer.pixels[(y as u32 * buffer.width + x as u32) as usize]
    }

    #[test]
    fn recovery_band_with_no_fonts_is_opaque_flat_background() {
        let mut buffer = WindowBuffer::new(200, 32);
        {
            let mut painter = Painter::new(&mut buffer);
            recovery_band(
                &mut painter,
                200,
                32,
                RECOVERY_BAND_COLOR,
                "Waterfox",
                None,
                1.0,
            );
        }
        for (x, y) in [(0, 0), (199, 0), (100, 16), (0, 31), (199, 31)] {
            assert_eq!(pixel_at(&buffer, x, y), RECOVERY_BAND_COLOR, "at ({x},{y})");
        }
    }

    #[test]
    fn recovery_tab_fills_its_whole_area_even_with_no_title() {
        let mut buffer = WindowBuffer::new(64, 32);
        {
            let mut painter = Painter::new(&mut buffer);
            recovery_band(&mut painter, 64, 32, RECOVERY_TAB_COLOR, "", None, 1.0);
        }
        for x in 0..64 {
            for y in 0..32 {
                assert_eq!(pixel_at(&buffer, x, y), RECOVERY_TAB_COLOR);
            }
        }
    }

    #[test]
    fn pressure_overlay_paints_a_distinct_colour_for_the_selected_button() {
        let mut buffer = WindowBuffer::new(400, 300);
        let buttons = [
            PxRect::new(10, 200, 80, 40),
            PxRect::new(100, 200, 80, 40),
            PxRect::new(190, 200, 80, 40),
            PxRect::new(280, 200, 80, 40),
        ];
        {
            let mut painter = Painter::new(&mut buffer);
            pressure_overlay(
                &mut painter,
                400,
                300,
                "Something happened.",
                None,
                &buttons,
                PressureChoice::Terminate,
                None,
                1.0,
            );
        }
        // Background is opaque everywhere outside the buttons.
        assert_eq!(pixel_at(&buffer, 5, 5), PRESSURE_OVERLAY_COLOR);
        // The selected (Terminate, index 2) button's centre is the selected colour; an
        // unselected button's centre is not.
        assert_eq!(pixel_at(&buffer, 230, 220), BUTTON_SELECTED_COLOR);
        assert_eq!(pixel_at(&buffer, 50, 220), BUTTON_COLOR);
    }

    #[test]
    fn scatter_card_draws_a_focus_border_only_when_focused() {
        let mut unfocused = WindowBuffer::new(100, 100);
        {
            let mut painter = Painter::new(&mut unfocused);
            scatter_card(&mut painter, 100, 100, false, 1.0);
        }
        assert_eq!(pixel_at(&unfocused, 0, 50), SCATTER_CARD_COLOR);

        let mut focused = WindowBuffer::new(100, 100);
        {
            let mut painter = Painter::new(&mut focused);
            scatter_card(&mut painter, 100, 100, true, 1.0);
        }
        assert_eq!(pixel_at(&focused, 0, 50), SCATTER_FOCUS_ACCENT);
        // Interior stays the card colour — the border is a stroke, not a full refill.
        assert_eq!(pixel_at(&focused, 50, 50), SCATTER_CARD_COLOR);
    }

    #[test]
    fn switcher_row_highlights_only_the_selected_row() {
        let mut selected = WindowBuffer::new(200, 28);
        {
            let mut painter = Painter::new(&mut selected);
            switcher_row(&mut painter, 200, 28, "Terminal", true, None, 1.0);
        }
        assert_eq!(pixel_at(&selected, 0, 0), SWITCHER_SELECTED_COLOR);

        let mut plain = WindowBuffer::new(200, 28);
        {
            let mut painter = Painter::new(&mut plain);
            switcher_row(&mut painter, 200, 28, "Terminal", false, None, 1.0);
        }
        assert_eq!(pixel_at(&plain, 0, 0), SWITCHER_PANEL_COLOR);
    }

    #[test]
    fn lock_screen_is_opaque_and_plain_with_no_font() {
        let mut buffer = WindowBuffer::new(300, 200);
        {
            let mut painter = Painter::new(&mut buffer);
            lock_screen(&mut painter, 300, 200, None, 1.0);
        }
        for (x, y) in [(0, 0), (299, 199), (150, 100)] {
            assert_eq!(pixel_at(&buffer, x, y), LOCK_SCREEN_COLOR);
        }
    }

    #[test]
    fn scaled_px_multiplies_every_field() {
        let rect = LRect::new(10, 20, 30, 40);
        let converted = scaled_px(rect, 2.0);
        assert_eq!(
            (converted.x, converted.y, converted.w, converted.h),
            (20, 40, 60, 80)
        );
    }

    /// `pixelkit-text/test-fonts` (dev-dependency only, `Cargo.toml`) embeds a real face
    /// specifically so downstream tests get deterministic glyphs without depending on whatever
    /// fonts the host running `cargo test` happens to have — see `chrome/text.rs`'s doc. These
    /// tests exercise the *with-font* path the ones above deliberately do not.
    mod with_a_real_font {
        use super::*;
        use pixelkit_text::font::test_fonts;

        #[test]
        fn recovery_band_draws_more_than_just_the_background() {
            let mut cache = TextCache::new(test_fonts::set());
            let mut buffer = WindowBuffer::new(200, 32);
            {
                let mut painter = Painter::new(&mut buffer);
                recovery_band(
                    &mut painter,
                    200,
                    32,
                    RECOVERY_BAND_COLOR,
                    "Waterfox",
                    Some((&mut cache, test_fonts::LATIN)),
                    1.0,
                );
            }
            assert!(
                buffer.pixels.iter().any(|&p| p != RECOVERY_BAND_COLOR),
                "expected some pixels changed by drawn glyphs, got a flat background"
            );
            // The far corner, well outside any plausible glyph, is still the plain background —
            // text drawing must not have overrun and repainted the whole buffer.
            assert_eq!(buffer.pixels[buffer.pixels.len() - 1], RECOVERY_BAND_COLOR);
        }

        #[test]
        fn pressure_overlay_button_labels_are_drawn_when_a_font_is_available() {
            let mut cache = TextCache::new(test_fonts::set());
            let mut buffer = WindowBuffer::new(400, 300);
            let buttons = [
                PxRect::new(10, 200, 80, 40),
                PxRect::new(100, 200, 80, 40),
                PxRect::new(190, 200, 80, 40),
                PxRect::new(280, 200, 80, 40),
            ];
            {
                let mut painter = Painter::new(&mut buffer);
                pressure_overlay(
                    &mut painter,
                    400,
                    300,
                    "Waterfox was terminated after sustained memory pressure.",
                    Some("org.waterfox.Waterfox"),
                    &buttons,
                    PressureChoice::Wait,
                    Some((&mut cache, test_fonts::LATIN)),
                    1.0,
                );
            }
            // At least one button's interior differs from its own flat background colour — the
            // label was actually drawn, not merely the button rectangle.
            let selected_button = buttons[0];
            let region_has_ink = (selected_button.y..selected_button.bottom())
                .flat_map(|y| (selected_button.x..selected_button.right()).map(move |x| (x, y)))
                .any(|(x, y)| pixel_at(&buffer, x, y) != BUTTON_SELECTED_COLOR);
            assert!(
                region_has_ink,
                "expected a drawn label inside the selected button"
            );
        }

        #[test]
        fn long_window_titles_are_elided_to_fit_the_band() {
            let mut cache = TextCache::new(test_fonts::set());
            let style = TextStyle::new(test_fonts::LATIN, 14.0);
            let long_title =
                "A Very Long Application Window Title That Will Never Fit In A Recovery Band";
            let truncated = cache.truncate(long_title, 120, style);
            assert!(
                truncated.len() < long_title.len(),
                "expected the title to be shortened"
            );
            assert!(cache.measure(&truncated, style) <= 120);
        }
    }
}
