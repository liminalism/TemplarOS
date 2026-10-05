//! Shared window marks and aspect-preserving previews; identity is display data, never grouping.
use super::design;
use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_text::Align;
use pixelkit_ui::Ui;
use std::collections::HashMap;

/// Fit without cropping or stretching, including portrait captures and tiny wells.
#[must_use]
pub fn letterbox(well: Rect, width: u32, height: u32) -> Rect {
    if width == 0 || height == 0 || well.w <= 0 || well.h <= 0 {
        return well;
    }
    let k = (f64::from(well.w) / f64::from(width)).min(f64::from(well.h) / f64::from(height));
    let w = ((f64::from(width) * k).round() as i32).clamp(1, well.w);
    let h = ((f64::from(height) * k).round() as i32).clamp(1, well.h);
    Rect::new(well.x + (well.w - w) / 2, well.y + (well.h - h) / 2, w, h)
}

pub fn icon(ui: &mut Ui<'_>, rect: Rect, app: Option<&str>, icons: &HashMap<String, Bitmap>) {
    if let Some(bitmap) = app.and_then(|app| icons.get(app)) {
        ui.painter.blit(
            bitmap,
            letterbox(rect, bitmap.width, bitmap.height),
            ScaleFilter::Bilinear,
        );
    } else {
        ui.painter
            .fill_rounded_rect(rect, ui.px(design::CHIP_RADIUS), design::accent_soft());
        ui.label(rect, "?", design::text_dim(), Align::Centre);
    }
}

/// Applied last so dimming also covers the picture, not only the title ink.
pub fn marks(ui: &mut Ui<'_>, rect: Rect, parked: bool, active: bool, verified: bool) {
    if parked {
        ui.painter.blend_rect(rect, design::shell_fill(), 110);
    } else {
        ui.painter.fill_rect(
            Rect::new(
                rect.x,
                rect.y,
                ui.px(design::ACTIVITY_EDGE_WIDTH).min(rect.w),
                rect.h,
            ),
            design::activity(),
        );
    }
    if active {
        ui.painter.fill_rect(
            Rect::new(rect.x, rect.bottom() - ui.px(2), rect.w, ui.px(2)),
            design::accent(),
        );
    }
    if !verified {
        let side = ui.px(14).min(rect.w).min(rect.h);
        ui.label(
            Rect::new(rect.right() - side, rect.y, side, side),
            "?",
            design::text(),
            Align::Centre,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aspect_and_centres_survive_landscape_portrait_and_empty_captures() {
        let well = Rect::new(10, 20, 120, 80);
        assert_eq!(letterbox(well, 200, 100), Rect::new(10, 30, 120, 60));
        assert_eq!(letterbox(well, 100, 200), Rect::new(50, 20, 40, 80));
        assert_eq!(letterbox(well, 0, 100), well);
        assert_eq!(
            letterbox(Rect::new(0, 0, 1, 1), 1000, 1),
            Rect::new(0, 0, 1, 1)
        );
    }
}
