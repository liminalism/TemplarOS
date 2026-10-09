use super::paint::Fonts;
use pixelkit_raster::{Painter, Rect};
use pixelkit_text::{TextStyle, cache::Align};
use templar_types::display::Insets;

/// A real full-signal layer. Calibration remains transparent centrally so Settings controls
/// remain usable while all four visible signal corners are continuously marked.
pub(super) fn paint(
    painter: &mut Painter<'_>,
    width: i32,
    height: i32,
    number: usize,
    label: &str,
    calibration: bool,
    insets: Insets,
    fonts: Fonts<'_>,
    palette: &templar_types::appearance::Palette,
) {
    let white = 0xff00_0000 | super::paint::opaque(palette.text);
    painter.fill_rect(
        Rect::new(0, 0, width, height),
        if calibration {
            0
        } else {
            0xff00_0000 | super::paint::opaque(palette.system_shell)
        },
    );
    let left = insets.left as i32;
    let right = width - insets.right as i32;
    let top = insets.top as i32;
    let bottom = height - insets.bottom as i32;
    if calibration {
        let reach = ((right - left).min(bottom - top) / 8).clamp(8, 128);
        for (x, y, sx, sy) in [
            (left, top, 1, 1),
            (right - 1, top, -1, 1),
            (left, bottom - 1, 1, -1),
            (right - 1, bottom - 1, -1, -1),
        ] {
            painter.fill_rect(
                Rect::new(if sx > 0 { x } else { x - reach + 1 }, y - 1, reach, 3),
                white,
            );
            painter.fill_rect(
                Rect::new(x - 1, if sy > 0 { y } else { y - reach + 1 }, 3, reach),
                white,
            );
        }
        let center = ((left + right) / 2, (top + bottom) / 2);
        painter.fill_rect(Rect::new(center.0 - 12, center.1, 25, 1), white);
        painter.fill_rect(Rect::new(center.0, center.1 - 12, 1, 25), white);
    } else {
        // Device numbers do not depend on optional font discovery.
        let unit = (height.min(width) / 40).clamp(2, 32);
        let mut digits = [0u8; 20];
        let mut value = number;
        let mut count = 0;
        loop {
            digits[count] = (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let digit_width = unit * 5;
        let start = (width - digit_width * count as i32) / 2;
        for position in 0..count {
            digit(
                painter,
                start + position as i32 * digit_width,
                height / 2 - unit * 5,
                unit,
                digits[count - position - 1],
                white,
            );
        }
    }
    if let Some((cache, face)) = fonts {
        let style = TextStyle::new(face, (height as f32 / 30.0).clamp(14.0, 48.0));
        cache.draw_fitted(
            painter,
            label,
            Rect::new(
                left + 24,
                top + 24,
                (right - left - 48).max(1),
                (bottom - top - 48).max(1),
            ),
            if calibration {
                top + 24
            } else {
                height * 3 / 4
            },
            style,
            white,
            Align::Centre,
        );
    }
}
fn digit(painter: &mut Painter<'_>, x: i32, y: i32, unit: i32, digit: u8, color: u32) {
    const MASKS: [u8; 10] = [
        0b1111110, 0b0110000, 0b1101101, 0b1111001, 0b0110011, 0b1011011, 0b1011111, 0b1110000,
        0b1111111, 0b1111011,
    ];
    let segments = [
        (x, y, 4 * unit, unit),
        (x + 3 * unit, y, unit, 5 * unit),
        (x + 3 * unit, y + 4 * unit, unit, 5 * unit),
        (x, y + 8 * unit, 4 * unit, unit),
        (x, y + 4 * unit, unit, 5 * unit),
        (x, y, unit, 5 * unit),
        (x, y + 4 * unit, 4 * unit, unit),
    ];
    for (index, (x, y, width, height)) in segments.into_iter().enumerate() {
        if MASKS[usize::from(digit)] & (1 << (6 - index)) != 0 {
            painter.fill_rect(Rect::new(x, y, width, height), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calibration_preserves_transparent_center_and_real_visible_corner_marks_without_fonts() {
        let mut buffer = pixelkit_raster::WindowBuffer::new(320, 200);
        paint(
            &mut Painter::new(&mut buffer),
            320,
            200,
            1,
            "HDMI",
            true,
            Insets::symmetric(20, 10),
            None,
            &templar_types::appearance::DARK,
        );
        assert_eq!(buffer.pixels[100 * 320 + 80], 0);
        assert_ne!(buffer.pixels[10 * 320 + 20], 0);
        assert_ne!(buffer.pixels[189 * 320 + 299], 0);
    }
}
