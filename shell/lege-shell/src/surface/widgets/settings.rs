//! The Settings widget's specialized body: three hardware readings, one line each.
//!
//! This tile replaced three — Battery, Volume and Brightness — and the reason it needs a body of
//! its own is the reason they were merged. The default body ([`super::draw`]) shows one headline
//! and hides its detail lines below the Wide rung, which is right for a widget with a single
//! figure and wrong for one whose whole content is three of them: at the floor rung it would have
//! shown the charge and silently dropped the other two. Here all three are the headline.
//!
//! The layout is a label column and a value column, so the figures line up down the tile and each
//! label appears once. "Battery 72% discharging" — not "Battery / Battery: 72% (discharging)",
//! which is what a heading plus a detail line plus a status word amounted to across three tiles.
//!
//! Like the launcher and the notifications list, only the *body* is specialized: the declared
//! region strip and the rung rules are the widget contract's ([`super::draw_region_strip`]), and
//! the wheel is the contract's too ([`super::scrolled_action`]).

use super::{WidgetDescriptor, WidgetSnapshot};
use crate::surface::design;
use lege_field::Representation;
use lege_types::action::ActionDecl;
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

/// The readings this widget shows, in the order [`snapshot`] packs them and [`rows`] reads them
/// back. Naming them once here is what keeps the packing and the unpacking from drifting apart.
pub const READINGS: [&str; 3] = ["Battery", "Volume", "Brightness"];

/// One line height for a reading.
const LINE_HEIGHT: i32 = 18;
/// The label column's width, before it is clamped to half the tile.
const LABEL_WIDTH: i32 = 74;

/// Pack three already-formatted readings into the snapshot the widget draws.
///
/// A [`WidgetSnapshot`] has one headline and a list of details, and the fetch that produces this
/// has three equals. Battery takes the headline and the other two the detail list, decided here
/// rather than at the call site so the body can read them back by position with confidence.
pub fn snapshot(readings: [String; 3], refused: Vec<String>) -> WidgetSnapshot {
    let [battery, volume, brightness] = readings;
    WidgetSnapshot {
        headline: battery,
        detail: vec![volume, brightness],
        available: true,
        refused,
    }
}

/// The `(label, reading)` pairs `snapshot` carries, in [`READINGS`] order.
///
/// Empty for a snapshot whose service could not be reached: there is nothing to label, and the
/// caller shows the reason instead of three blank rows.
fn rows(snapshot: &WidgetSnapshot) -> Vec<(&'static str, &str)> {
    if !snapshot.available {
        return Vec::new();
    }
    let values = std::iter::once(snapshot.headline.as_str())
        .chain(snapshot.detail.iter().map(String::as_str));
    READINGS.into_iter().zip(values).collect()
}

/// Draw the widget and report the action a click or a wheel asked for.
///
/// The same shape as [`super::draw`], so the field screen's system-tile arm treats this like any
/// other widget: everything specialized is inside.
pub fn draw(
    ui: &mut Ui<'_>,
    content: Rect,
    widget: &'static WidgetDescriptor,
    snapshot: Option<&WidgetSnapshot>,
    rung: Representation,
    confirming: Option<&'static str>,
) -> Option<&'static ActionDecl> {
    // No snapshot yet means nothing is *known* to be refused — the controls stay live rather than
    // greying out while the first fetch is still in flight.
    let refused = snapshot.map_or(&[][..], |snapshot| &snapshot.refused);
    let (body, chosen) = super::draw_region_strip(ui, content, widget, rung, confirming, refused);
    draw_readings(ui, body, snapshot);
    // A click on a region wins over a wheel in the same frame: the click is the deliberate one.
    chosen.or_else(|| super::scrolled_action(ui, body, widget, refused))
}

fn draw_readings(ui: &mut Ui<'_>, body: Rect, snapshot: Option<&WidgetSnapshot>) {
    let inner = body.inset(ui.px(10));
    if inner.h <= 0 {
        return;
    }
    let Some(snapshot) = snapshot else {
        ui.label(inner, "…", design::text_faint(), Align::Left);
        return;
    };
    let readings = rows(snapshot);
    if readings.is_empty() {
        // An unreachable service says so, rather than showing three plausible zeroes.
        ui.label_styled(
            inner,
            &snapshot.headline,
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
        return;
    }
    let line = ui.px(LINE_HEIGHT);
    let label_width = ui.px(LABEL_WIDTH).min(inner.w / 2).max(0);
    for (index, (label, value)) in readings.iter().enumerate() {
        let y = inner.y + line * i32::try_from(index).unwrap_or(0);
        if y + line > inner.bottom() {
            break;
        }
        ui.label_styled(
            Rect::new(inner.x, y, label_width, line),
            label,
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
        ui.label_styled(
            Rect::new(inner.x + label_width, y, inner.w - label_width, line),
            value,
            design::measurement_style(ui),
            design::text(),
            Align::Left,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::widgets;

    fn reading(battery: &str, volume: &str, brightness: &str) -> WidgetSnapshot {
        snapshot(
            [battery.to_owned(), volume.to_owned(), brightness.to_owned()],
            Vec::new(),
        )
    }

    /// Every reading is labelled exactly once, and the label is not repeated inside the value.
    #[test]
    fn each_reading_is_one_labelled_line() {
        let snapshot = reading("72% discharging", "45%", "60%");
        assert_eq!(
            rows(&snapshot),
            vec![
                ("Battery", "72% discharging"),
                ("Volume", "45%"),
                ("Brightness", "60%"),
            ]
        );
    }

    /// What the fetch packs is what the body reads back — the one thing that could silently
    /// scramble the tile into reporting the brightness as the charge.
    #[test]
    fn packing_and_unpacking_agree_on_the_order() {
        let snapshot = reading("battery-value", "volume-value", "brightness-value");
        for (label, value) in rows(&snapshot) {
            assert_eq!(value, format!("{}-value", label.to_lowercase()));
        }
    }

    #[test]
    fn an_unreachable_service_shows_its_reason_and_no_readings() {
        let snapshot = WidgetSnapshot::unavailable("Settings — no session bus");
        assert!(rows(&snapshot).is_empty());
        assert!(!snapshot.available);
    }

    /// A machine with no backlight refuses the brightness controls and keeps the rest: the
    /// refusal travels with the one snapshot now, so it has to stay per-action.
    #[test]
    fn refusals_stay_attached_to_the_control_that_was_refused() {
        let snapshot = snapshot(
            ["No battery".to_owned(), "45%".to_owned(), "—".to_owned()],
            vec![
                "brightness.increase".to_owned(),
                "brightness.decrease".to_owned(),
            ],
        );
        assert!(snapshot.refuses("brightness.increase"));
        assert!(!snapshot.refuses("volume.increase"));
        assert!(snapshot.available, "a missing backlight is not a failure");
    }

    /// The floor rung shows all three readings. The default body would have shown one — which is
    /// exactly what made three separate tiles seem necessary.
    #[test]
    fn every_reading_is_drawn_at_the_floor_rung() {
        let widget = widgets::descriptor("settings").expect("declared widget");
        let snapshot = reading("72%", "45%", "60%");
        let mut buffer = pixelkit_raster::WindowBuffer::new(324, 214);
        let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
        let mut kernel = pixelkit_raster::RasterKernel::new();
        let mut input = pixelkit_shell::Input::new();
        let painted = {
            let mut ui = Ui::new(
                pixelkit_raster::Painter::new(&mut buffer),
                &mut text,
                &mut input,
                design::theme(),
                pixelkit_shell::Scale::ONE,
                &mut kernel,
            );
            let content = Rect::new(0, 0, 324, 214);
            draw(
                &mut ui,
                content,
                widget,
                Some(&snapshot),
                Representation::Small,
                None,
            );
            // Three lines of text fit above the region strip at the tile's own arrival size.
            let (body, _) = crate::surface::widgets::split_regions(content, &ui, 1);
            body.inset(ui.px(10)).h >= ui.px(LINE_HEIGHT) * 3
        };
        assert!(painted, "three readings do not fit at the arrival size");
    }
}
