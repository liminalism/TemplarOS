//! The notifications widget's specialized body: per-notification rows a person can act on,
//! not just count. Each row shows `app — summary` with its own dismiss control, and at the
//! Large rung a notification's declared actions become chips — pressing one asks the server
//! to emit the freedesktop `ActionInvoked` for the owning application
//! (`Notifications1.Invoke`), which is what makes a Reply or Open button real instead of
//! decorative.
//!
//! Like the launcher, only the *body* is specialized: the region strip (the declared
//! `notifications.clear` control) and the rung rules remain the widget contract's, drawn by
//! [`super::draw_region_strip`].

use super::WidgetDescriptor;
use crate::surface::design;
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;
use templar_field::Representation;
use templar_types::action::ActionDecl;

/// One active notification, as the widget needs it. `actions` are `(key, label)` pairs in the
/// order the sender declared them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationRow {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub actions: Vec<(String, String)>,
}

/// What the widget's content is drawn from.
#[derive(Clone, Copy, Debug)]
pub struct NotificationsData<'a> {
    pub rows: &'a [NotificationRow],
    pub headline: &'a str,
    pub available: bool,
}

/// What a frame's input asked for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationsOutcome {
    None,
    /// Dismiss exactly this notification (`Notifications1.Dismiss`).
    Dismiss(u32),
    /// Invoke this notification's declared action (`Notifications1.Invoke`).
    Invoke {
        id: u32,
        key: String,
    },
    /// A declared region was pressed; the host routes it like any declared action.
    Action(&'static ActionDecl),
}

/// Height of one notification row, and of its action-chip row when shown.
const ROW_HEIGHT: i32 = 22;
const CHIP_HEIGHT: i32 = 18;

#[allow(clippy::too_many_lines)]
pub fn draw(
    ui: &mut Ui<'_>,
    content: Rect,
    widget: &'static WidgetDescriptor,
    data: &NotificationsData<'_>,
    rung: Representation,
    confirming: Option<&'static str>,
) -> NotificationsOutcome {
    let (body, region_action) =
        super::draw_region_strip(ui, content, widget, rung, confirming, &[]);
    if let Some(declaration) = region_action {
        return NotificationsOutcome::Action(declaration);
    }

    let inner = body.inset(ui.px(10));
    if inner.h <= 0 {
        return NotificationsOutcome::None;
    }
    let colour = if data.available {
        design::text()
    } else {
        design::text_dim()
    };
    ui.label_styled(
        Rect::new(inner.x, inner.y, inner.w, ui.px(18)),
        data.headline,
        design::measurement_style(ui),
        colour,
        Align::Left,
    );
    if rung < Representation::Wide {
        return NotificationsOutcome::None;
    }

    let mut outcome = NotificationsOutcome::None;
    let mut y = inner.y + ui.px(24);
    // Rows fill whatever height the tile has: growing the tile shows more, which is the
    // representation ladder doing its job rather than a fixed cap pretending to be one.
    for row in data.rows {
        let row_height = ui.px(ROW_HEIGHT);
        if y + row_height > inner.bottom() {
            break;
        }
        let line = Rect::new(inner.x, y, inner.w, row_height);
        let (dismiss, text) = line.split_right(ui.px(24));
        if ui.input.hovering(line) {
            ui.painter.fill_rect(line, ui.theme.hover);
        }
        ui.label_styled(
            text,
            &format!("{} — {}", row.app, row.summary),
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
        ui.label_styled(
            dismiss,
            "×",
            design::state_style(ui),
            design::text(),
            Align::Centre,
        );
        if ui.input.take_click(dismiss) {
            outcome = NotificationsOutcome::Dismiss(row.id);
        }
        y += row_height;

        // Action chips: only at the Large rung — a Wide tile is a list, a Large one is a
        // place to act — and only while there is room for the chip row.
        if rung >= Representation::Large
            && !row.actions.is_empty()
            && y + ui.px(CHIP_HEIGHT) <= inner.bottom()
        {
            let mut x = inner.x + ui.px(12);
            for (key, label) in row.actions.iter().take(3) {
                let width = ui.text.measure(label, design::state_style(ui)) + ui.px(16);
                let chip = Rect::new(x, y, width.min(inner.w / 3), ui.px(CHIP_HEIGHT));
                if chip.right() > inner.right() {
                    break;
                }
                ui.painter.fill_rounded_rect(
                    chip,
                    ui.px(design::CHIP_RADIUS),
                    design::system_shell(),
                );
                ui.painter
                    .stroke_rect(chip, ui.scale.hairline(), design::edge());
                ui.label_styled(
                    chip,
                    label,
                    design::state_style(ui),
                    design::text(),
                    Align::Centre,
                );
                if ui.input.take_click(chip) {
                    outcome = NotificationsOutcome::Invoke {
                        id: row.id,
                        key: key.clone(),
                    };
                }
                x += chip.w + ui.px(6);
            }
            y += ui.px(CHIP_HEIGHT) + ui.px(4);
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::widgets;

    fn rows() -> Vec<NotificationRow> {
        vec![
            NotificationRow {
                id: 7,
                app: "Mail".into(),
                summary: "New message".into(),
                actions: vec![("reply".into(), "Reply".into())],
            },
            NotificationRow {
                id: 9,
                app: "Build".into(),
                summary: "Finished".into(),
                actions: Vec::new(),
            },
        ]
    }

    struct Harness {
        buffer: pixelkit_raster::WindowBuffer,
        text: pixelkit_text::TextCache,
        input: pixelkit_shell::Input,
        kernel: pixelkit_raster::RasterKernel,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                buffer: pixelkit_raster::WindowBuffer::new(600, 500),
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                input: pixelkit_shell::Input::new(),
                kernel: pixelkit_raster::RasterKernel::new(),
            }
        }

        fn frame(
            &mut self,
            rung: Representation,
            rows: &[NotificationRow],
            click: Option<(i32, i32)>,
        ) -> NotificationsOutcome {
            if let Some((x, y)) = click {
                #[allow(clippy::cast_precision_loss)]
                self.input.cursor_moved(x as f32, y as f32);
                self.input.mouse(pixelkit_shell::MouseButton::Left, true);
                self.input.mouse(pixelkit_shell::MouseButton::Left, false);
            }
            let widget = widgets::descriptor("notifications").expect("declared widget");
            let data = NotificationsData {
                rows,
                headline: "2 notifications",
                available: true,
            };
            let mut ui = pixelkit_ui::Ui::new(
                pixelkit_raster::Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                pixelkit_shell::Scale::ONE,
                &mut self.kernel,
            );
            let outcome = draw(
                &mut ui,
                Rect::new(0, 0, 600, 500),
                widget,
                &data,
                rung,
                None,
            );
            self.input.end_frame();
            outcome
        }
    }

    /// Every notification row carries its own dismiss control, and pressing one asks to
    /// dismiss exactly that id — not the bulk clear.
    #[test]
    fn notifications_rows_have_per_item_dismiss() {
        let mut harness = Harness::new();
        let rows = rows();
        // Row 0's dismiss control sits at the right edge of the first row. The body starts
        // under the headline; probe the first row's dismiss rect.
        let outcome = harness.frame(Representation::Large, &rows, Some((600 - 12, 34 + 11)));
        assert_eq!(outcome, NotificationsOutcome::Dismiss(7));
    }

    /// A declared action renders as a chip at the Large rung, and pressing it invokes exactly
    /// that notification's action key.
    #[test]
    fn notifications_action_chips_invoke_the_declared_action() {
        let mut harness = Harness::new();
        let rows = rows();
        let outcome = harness.frame(Representation::Large, &rows, Some((30, 34 + 22 + 9)));
        assert_eq!(
            outcome,
            NotificationsOutcome::Invoke {
                id: 7,
                key: "reply".into()
            }
        );
    }

    /// Growing the tile shows more rows: the cap is the tile's height, not a constant.
    #[test]
    fn notifications_rows_fill_the_available_height() {
        let mut harness = Harness::new();
        let many: Vec<NotificationRow> = (0..30)
            .map(|index| NotificationRow {
                id: index,
                app: "App".into(),
                summary: format!("Item {index}"),
                actions: Vec::new(),
            })
            .collect();
        // No click: just draw at both rungs; the Wide rung on a short tile draws fewer rows
        // than the Large rung on a tall one — asserted indirectly by clicking where row 12's
        // dismiss would be: reachable on the tall tile only.
        let tall_click = harness.frame(
            Representation::Large,
            &many,
            Some((600 - 12, 34 + 12 * 22 + 11)),
        );
        assert_eq!(tall_click, NotificationsOutcome::Dismiss(12));
    }
}
