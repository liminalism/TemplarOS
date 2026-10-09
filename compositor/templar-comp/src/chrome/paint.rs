//! Pixel-level drawing for every chrome widget, via `pixelkit-raster`'s [`Painter`] and, when a
//! font was found, `pixelkit-text`'s [`TextCache`]. Every function here draws onto a buffer that
//! is *already* the whole widget's own canvas at `(0, 0)` — `chrome/cache.rs` owns sizing,
//! upload and reuse; this module only ever fills pixels.
//!
//! Every background here is drawn fully opaque. That is deliberate, not an oversight: the
//! recovery tab in particular exists because "the client may draw over that area"
//! (`templar_window::geometry`'s doc on `RecoveryAffordance`) — a translucent tab would still let
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
use pixelkit_raster::{Painter, Rect as PxRect};
use pixelkit_text::{FaceId, TextCache, TextStyle, cache::Align};
use templar_types::appearance::Palette;
use templar_window::Rect as LRect;

/// Text and cache together, or nothing at all — the one parameter every drawing function below
/// threads through instead of taking `Option<&ChromeFonts>` directly, so this module never needs
/// to know `ChromeFonts`'s own shape.
pub type Fonts<'a> = Option<(&'a mut TextCache, FaceId)>;

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

/// A window's title tab (the title rectangle of its [`templar_window::WindowLayout`]):
/// opaque `background`, the window's title elided to fit, in contrast-picked ink. With no font,
/// exactly the flat rectangle chrome drew before this module existed.
///
/// `group` is the window's group accent and name, if it is in one: the accent becomes a stripe
/// down the left of the tab, and the name is drawn before the title. The stripe is what makes a
/// group visible with no font at all, and legible at a glance on a tab that may be 96 pixels
/// wide — the name is the elaboration, not the signal.
#[allow(clippy::too_many_arguments)]
pub fn name_tag(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    background: u32,
    title: &str,
    group: Option<(u32, &str)>,
    tab_count: Option<u32>,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, background);
    let ink = layout::text_ink_for(background);
    let mut title_right = area.right();

    let mut left = area.x;
    if let Some((accent, _)) = group {
        let stripe = ((layout::GROUP_STRIPE_WIDTH as f32 * scale).round() as i32).max(1);
        painter.fill_rect(PxRect::new(area.x, area.y, stripe, area.h), opaque(accent));
        left += stripe;
    }

    let Some((cache, face)) = fonts else { return };
    if let Some(count) = tab_count.filter(|count| *count > 0) {
        // Format without a heap allocation; a count badge is at most ten decimal digits.
        let mut digits = [0u8; 10];
        let mut start = digits.len();
        let mut value = count;
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let label = std::str::from_utf8(&digits[start..]).unwrap_or("");
        let padding = (4.0 * scale).round() as i32;
        let badge_width = ((label.len() as f32 * 8.0 + 8.0) * scale).round() as i32;
        let badge = PxRect::new(
            area.right() - badge_width - padding,
            area.y + padding,
            badge_width,
            (area.h - 2 * padding).max(0),
        );
        if badge.x >= left + padding && badge.h > 0 {
            painter.fill_rect(badge, ink);
            let style = TextStyle::new(face, (12.0 * scale).max(8.0));
            let y = badge.y + ((badge.h - cache.line_height(style)) / 2).max(0);
            cache.draw_fitted(painter, label, badge, y, style, background, Align::Centre);
            title_right = badge.x - padding;
        }
    }
    let label = match group {
        Some((_, name)) if !name.is_empty() && !title.is_empty() => format!("{name} — {title}"),
        Some((_, name)) if !name.is_empty() => name.to_owned(),
        _ => title.to_owned(),
    };
    let title = label.as_str();
    if title.is_empty() {
        return;
    }

    let style = TextStyle::new(face, (14.0 * scale).max(8.0));
    let padding = (8.0 * scale).round() as i32;
    let text_area = PxRect::new(
        left + padding,
        area.y,
        (title_right - left - 2 * padding).max(0),
        area.h,
    );
    let line_height = cache.line_height(style);
    let y = area.y + ((area.h - line_height) / 2).max(0);
    cache.draw_fitted(painter, title, text_area, y, style, ink, Align::Left);
}

/// The three window controls: `tags` are in the widget's own coordinates
/// (`chrome::collect_control_tags`), already scaled — a vertical stack for the same-corner
/// arrangement, a horizontal row for opposite ends.
pub fn control_tags(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    background: u32,
    tags: &[(layout::ControlTag, PxRect)],
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.erase_rect(area);
    // This widget is uploaded as ARGB so gaps can stay transparent. The raster
    // painter's ordinary RGB mode preserves the caller's alpha lane verbatim.
    let ink = 0xff00_0000 | layout::text_ink_for(background);
    for (tag, rect) in tags {
        painter.fill_rect(*rect, background | 0xff00_0000);
        control_tag_glyph(painter, *tag, *rect, ink, scale);
    }
}

/// One control tag, as a glyph anyone recognises without a font: a bar to park, a square to
/// maximize, a cross to close. Drawn with filled rectangles only, so it is legible at the tag's
/// own 32 logical pixels and identical whether or not a font was found.
fn control_tag_glyph(
    painter: &mut Painter<'_>,
    button: layout::ControlTag,
    rect: PxRect,
    ink: u32,
    scale: f32,
) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    painter.push_clip(rect);
    let side = rect.w.min(rect.h);
    let stroke = ((2.0 * scale).round() as i32).clamp(1, side);
    let reach = ((10.0 * scale).round() as i32).clamp(stroke, (side - 2 * stroke).max(stroke));
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    let left = cx - reach / 2;
    let top = cy - reach / 2;
    match button {
        layout::ControlTag::Park => {
            painter.fill_rect(PxRect::new(left, cy - stroke / 2, reach, stroke), ink);
        }
        layout::ControlTag::Maximize => {
            painter.fill_rect(PxRect::new(left, top, reach, stroke), ink);
            painter.fill_rect(PxRect::new(left, top + reach - stroke, reach, stroke), ink);
            painter.fill_rect(PxRect::new(left, top, stroke, reach), ink);
            painter.fill_rect(PxRect::new(left + reach - stroke, top, stroke, reach), ink);
        }
        layout::ControlTag::Close => {
            // Two diagonals as staircases of `stroke`-sized squares, the way the grip draws its
            // rules: `Painter` fills rectangles, so a diagonal is stepped rather than stroked.
            for step in 0..reach {
                painter.fill_rect(PxRect::new(left + step, top + step, stroke, stroke), ink);
                painter.fill_rect(
                    PxRect::new(left + reach - stroke - step, top + step, stroke, stroke),
                    ink,
                );
            }
        }
    }
    painter.pop_clip();
}

/// The pressure overlay (`templar_recovery_v1.show_pressure_overlay`): the message, the offending
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
    palette: &Palette,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, opaque(palette.danger_soft));

    draw_buttons(painter, buttons, selected, scale, palette);

    let Some((cache, face)) = fonts else { return };
    let ink = layout::text_ink_for(opaque(palette.danger_soft));
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
            opaque(palette.accent)
        } else {
            opaque(palette.shell_low)
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
    palette: &Palette,
) {
    let radius = (6.0 * scale).round() as i32;
    for (index, rect) in buttons.iter().enumerate() {
        let choice = PressureChoice::from_index(index);
        let color = if choice == selected {
            opaque(palette.accent)
        } else {
            opaque(palette.shell_low)
        };
        painter.fill_rounded_rect(*rect, radius, color);
    }
}

/// The label strip under a scatter tile's card — the window's title, elided to fit, so tiles "can
/// be told apart" (planning doc 01 §3) even before pixelkit landed here.
pub fn scatter_label(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    title: &str,
    background: u32,
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

/// One row of the fallback switcher (`TemplarCompState::step_switcher`) — a window's label,
/// highlighted if it is the current selection, in muted ink if the window is parked on Home's
/// shelf (choosing it brings it back).
#[allow(clippy::too_many_arguments)]
pub fn switcher_row(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    label: &str,
    selected: bool,
    parked: bool,
    fonts: Fonts<'_>,
    scale: f32,
    palette: &Palette,
) {
    let area = canvas(pixel_w, pixel_h);
    let background = if selected {
        opaque(palette.accent_soft)
    } else {
        opaque(palette.system_shell)
    };
    painter.fill_rect(area, background);
    let Some((cache, face)) = fonts else { return };
    if label.is_empty() {
        return;
    }
    let ink = if parked {
        layout::muted_ink_for(background)
    } else {
        layout::text_ink_for(background)
    };
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
    cache.draw_fitted(painter, label, text_area, y, style, ink, Align::Left);
}

/// The switcher's title row: the group's name on its own accent, or "Groups" on the panel
/// colour. Same shape as a [`switcher_row`], one size smaller and always in contrast-picked ink,
/// so the HUD reads as a labelled list rather than as a list with an odd first entry.
pub fn switcher_heading(
    painter: &mut Painter<'_>,
    pixel_w: i32,
    pixel_h: i32,
    heading: &str,
    background: u32,
    fonts: Fonts<'_>,
    scale: f32,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, background);
    let Some((cache, face)) = fonts else { return };
    if heading.is_empty() {
        return;
    }
    let ink = layout::text_ink_for(background);
    let style = TextStyle::new(face, (13.0 * scale).max(8.0));
    let padding = (10.0 * scale).round() as i32;
    let text_area = PxRect::new(
        area.x + padding,
        area.y,
        (area.w - 2 * padding).max(0),
        area.h,
    );
    let line_height = cache.line_height(style);
    let y = area.y + ((area.h - line_height) / 2).max(0);
    cache.draw_fitted(painter, heading, text_area, y, style, ink, Align::Left);
}

/// One bar of a window group's outline: a flat rectangle in the group's accent
/// (`chrome::layout::outline_bars`). Four of these, drawn just outside the union of the group's
/// windows, are the whole of rule 7's outline — no corners to join, nothing to anti-alias, and
/// nothing that can fail to load.
pub fn group_outline_bar(painter: &mut Painter<'_>, pixel_w: i32, pixel_h: i32, colour: u32) {
    painter.fill_rect(canvas(pixel_w, pixel_h), colour);
}

/// A window group's accent (0x00RRGGBB, `templar_window::group`) as a colour this module can fill
/// with. The model stores colours with no alpha channel at all — it has no idea what a renderer
/// is — and every chrome widget is opaque, so this is the one place the two meet.
pub fn opaque(accent: u32) -> u32 {
    pixelkit_raster::color::from_rgb(
        ((accent >> 16) & 0xFF) as u8,
        ((accent >> 8) & 0xFF) as u8,
        (accent & 0xFF) as u8,
    )
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
    palette: &Palette,
) {
    let area = canvas(pixel_w, pixel_h);
    painter.fill_rect(area, opaque(palette.ground));
    let Some((cache, face)) = fonts else { return };
    let ink = layout::text_ink_for(opaque(palette.ground));
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
    use templar_types::appearance::LIGHT;
    const RECOVERY_BAND_COLOR: u32 = 0xff00_0000 | LIGHT.shell_low;
    const RECOVERY_TAB_COLOR: u32 = 0xff00_0000 | LIGHT.activity;

    fn pixel_at(buffer: &WindowBuffer, x: i32, y: i32) -> u32 {
        buffer.pixels[(y as u32 * buffer.width + x as u32) as usize]
    }

    #[test]
    fn name_tag_with_no_fonts_is_opaque_flat_background() {
        let mut buffer = WindowBuffer::new(200, 32);
        {
            let mut painter = Painter::new(&mut buffer);
            name_tag(
                &mut painter,
                200,
                32,
                RECOVERY_BAND_COLOR,
                "Waterfox",
                None,
                None,
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
            name_tag(
                &mut painter,
                64,
                32,
                RECOVERY_TAB_COLOR,
                "",
                None,
                None,
                None,
                1.0,
            );
        }
        for x in 0..64 {
            for y in 0..32 {
                assert_eq!(pixel_at(&buffer, x, y), RECOVERY_TAB_COLOR);
            }
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

    #[test]
    fn control_gaps_are_transparent_instead_of_painted_dead_targets() {
        let mut buffer = WindowBuffer::new(28, 92);
        let tags = [
            (layout::ControlTag::Close, PxRect::new(0, 0, 28, 28)),
            (layout::ControlTag::Maximize, PxRect::new(0, 32, 28, 28)),
            (layout::ControlTag::Park, PxRect::new(0, 64, 28, 28)),
        ];
        control_tags(
            &mut Painter::new(&mut buffer),
            28,
            92,
            RECOVERY_BAND_COLOR,
            &tags,
            1.0,
        );
        for y in [28, 29, 30, 31, 60, 61, 62, 63] {
            for x in 0..28 {
                assert_eq!(pixel_at(&buffer, x, y), 0);
            }
        }
        assert_eq!(pixel_at(&buffer, 0, 0), RECOVERY_BAND_COLOR);
        for (_, rect) in tags {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    assert_eq!(pixel_at(&buffer, x, y) >> 24, 255);
                }
            }
        }
    }

    #[test]
    fn tiny_control_glyphs_stay_opaque_and_do_not_paint_gap_pixels() {
        for size in 1..=28 {
            let mut buffer = WindowBuffer::new(size as u32, (3 * size + 8) as u32);
            let tags = [
                (layout::ControlTag::Close, PxRect::new(0, 0, size, size)),
                (
                    layout::ControlTag::Maximize,
                    PxRect::new(0, size + 4, size, size),
                ),
                (
                    layout::ControlTag::Park,
                    PxRect::new(0, 2 * size + 8, size, size),
                ),
            ];
            control_tags(
                &mut Painter::new(&mut buffer),
                size,
                3 * size + 8,
                0,
                &tags,
                1.0,
            );
            for y in 0..3 * size + 8 {
                for x in 0..size {
                    let control_pixel = tags.iter().any(|(_, rect)| rect.contains(x, y));
                    assert_eq!(
                        pixel_at(&buffer, x, y) >> 24,
                        if control_pixel { 255 } else { 0 }
                    );
                }
            }
        }
    }

    /// `pixelkit-text/test-fonts` (dev-dependency only, `Cargo.toml`) embeds a real face
    /// specifically so downstream tests get deterministic glyphs without depending on whatever
    /// fonts the host running `cargo test` happens to have — see `chrome/text.rs`'s doc. These
    /// tests exercise the *with-font* path the ones above deliberately do not.
    mod with_a_real_font {
        use super::*;
        use pixelkit_text::font::test_fonts;

        #[test]
        fn tab_count_badge_changes_pixels_without_changing_title() {
            let mut cache = TextCache::new(test_fonts::set());
            let mut unknown = WindowBuffer::new(200, 32);
            let mut known = WindowBuffer::new(200, 32);
            for (buffer, count) in [(&mut unknown, None), (&mut known, Some(7))] {
                name_tag(
                    &mut Painter::new(buffer),
                    200,
                    32,
                    RECOVERY_BAND_COLOR,
                    "same title",
                    None,
                    count,
                    Some((&mut cache, test_fonts::LATIN)),
                    1.0,
                );
            }
            assert_ne!(unknown.pixels, known.pixels);
            assert_eq!(pixel_at(&known, 0, 0), RECOVERY_BAND_COLOR);
        }

        #[test]
        fn name_tag_draws_more_than_just_the_background() {
            let mut cache = TextCache::new(test_fonts::set());
            let mut buffer = WindowBuffer::new(200, 32);
            {
                let mut painter = Painter::new(&mut buffer);
                name_tag(
                    &mut painter,
                    200,
                    32,
                    RECOVERY_BAND_COLOR,
                    "Waterfox",
                    None,
                    None,
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
