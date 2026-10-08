//! A Gregorian calendar painted with the shell's own Pixelkit controls.
//!
//! Chrono owns date arithmetic; the host supplies OS-local time, just as it does for the clock.
//! No desktop calendar application, timezone database, or second GUI toolkit is involved.

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, Timelike};
use pixelkit_raster::Rect;
use pixelkit_shell::KeyInput;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use crate::surface::design;
use crate::surface::glyphs::{self, Glyph};

pub(crate) const WIDTH: i32 = 336;
pub(crate) const HEIGHT: i32 = 408;
const PAD: i32 = 10;
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const DAY_LABELS: [&str; 31] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16", "17",
    "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30", "31",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Day,
    Previous,
    Today,
    Next,
    Settings,
}

#[derive(Debug)]
pub(crate) struct Calendar {
    month: NaiveDate,
    days: [Option<NaiveDate>; 42],
    selected: NaiveDate,
    focus: Focus,
    month_label: String,
    selected_label: String,
    local_minute: Option<NaiveDateTime>,
    local_date: Option<NaiveDate>,
    clock_label: String,
    date_label: String,
    settings_requested: bool,
}

/// All geometry is derived from the actual frame, including on narrow or fractional-scale hosts.
#[derive(Debug)]
pub(crate) struct Layout {
    pub frame: Rect,
    clock: Rect,
    date: Rect,
    month: Rect,
    pub previous: Rect,
    pub today: Rect,
    pub next: Rect,
    weekdays: Rect,
    grid: Rect,
    selected: Rect,
    hint: Rect,
    pub settings: Rect,
}

impl Layout {
    pub fn new(ui: &Ui<'_>, frame: Rect) -> Self {
        let inner = frame.inset(ui.px(PAD).min(frame.w / 8).min(frame.h / 8));
        // Split by proportions rather than rounded row sums: no control extends beyond the
        // frame even at 1.25x or when a small output has shortened it.
        let mut y = inner.y;
        let mut band = |weight: i32| {
            let bottom = y + inner.h * weight / (HEIGHT - 2 * PAD);
            let row = Rect::new(inner.x, y, inner.w, bottom - y);
            y = bottom;
            row
        };
        let clock = band(30);
        let date = band(24);
        let navigation = band(34);
        let weekdays = band(24);
        let grid = band(192);
        let selected = band(26);
        let settings = band(34);
        let hint = Rect::new(inner.x, y, inner.w, inner.bottom() - y);
        let arrow = ui.px(30).min(navigation.w / 8);
        let today_w = ui.px(50).min(navigation.w / 5);
        let previous = Rect::new(navigation.x, navigation.y, arrow, navigation.h);
        let next = Rect::new(
            navigation.right() - arrow,
            navigation.y,
            arrow,
            navigation.h,
        );
        let today = Rect::new(next.x - today_w, navigation.y, today_w, navigation.h);
        let month = Rect::new(
            previous.right(),
            navigation.y,
            today.x - previous.right(),
            navigation.h,
        );
        Self {
            frame,
            clock,
            date,
            month,
            previous,
            today,
            next,
            weekdays,
            grid,
            selected,
            hint,
            settings,
        }
    }

    pub fn cell(&self, index: usize) -> Rect {
        let column = index as i32 % 7;
        let row = index as i32 / 7;
        let x = self.grid.x + self.grid.w * column / 7;
        let right = self.grid.x + self.grid.w * (column + 1) / 7;
        let y = self.grid.y + self.grid.h * row / 6;
        let bottom = self.grid.y + self.grid.h * (row + 1) / 6;
        Rect::new(x, y, right - x, bottom - y)
    }
}

impl Calendar {
    pub fn new(today: NaiveDate) -> Self {
        let month = today.with_day(1).expect("every month has a first day");
        let mut calendar = Self {
            month,
            days: Self::month_days(month),
            selected: today,
            focus: Focus::Day,
            month_label: String::new(),
            selected_label: String::new(),
            local_minute: None,
            local_date: None,
            clock_label: "Local time unavailable".to_owned(),
            date_label: String::new(),
            settings_requested: false,
        };
        calendar.update_labels();
        calendar
    }

    pub fn month(&self) -> NaiveDate {
        self.month
    }
    pub fn selected(&self) -> NaiveDate {
        self.selected
    }

    fn update_labels(&mut self) {
        self.month_label = self.month.format("%B %Y").to_string();
        self.selected_label = self.selected.format("%A, %-d %B %Y").to_string();
    }

    fn select(&mut self, date: NaiveDate) {
        if self.selected == date {
            return;
        }
        self.selected = date;
        let month = date.with_day(1).expect("every month has a first day");
        if self.month != month {
            self.month = month;
            self.days = Self::month_days(month);
            self.month_label = self.month.format("%B %Y").to_string();
        }
        self.selected_label = self.selected.format("%A, %-d %B %Y").to_string();
    }

    pub fn move_month(&mut self, forward: bool) {
        let next = if forward {
            self.selected.checked_add_months(Months::new(1))
        } else {
            self.selected.checked_sub_months(Months::new(1))
        };
        if let Some(next) = next {
            self.select(next);
        }
    }

    pub fn today(&mut self, today: NaiveDate) {
        self.select(today);
    }

    /// Monday-first cells, with genuine empty cells outside the displayed month.
    pub fn day_at(&self, index: usize) -> Option<NaiveDate> {
        self.days.get(index).copied().flatten()
    }
    fn month_days(month: NaiveDate) -> [Option<NaiveDate>; 42] {
        let start = month.weekday().num_days_from_monday() as usize;
        std::array::from_fn(|index| {
            let day = u32::try_from(index.checked_sub(start)? + 1).ok()?;
            month.with_day(day)
        })
    }

    fn move_days(&mut self, delta: i32) {
        let next = if delta >= 0 {
            self.selected.checked_add_days(Days::new(delta as u64))
        } else {
            self.selected
                .checked_sub_days(Days::new(u64::from(delta.unsigned_abs())))
        };
        if let Some(next) = next {
            self.select(next);
        }
    }

    /// Returns true when Escape dismissed the popup. Navigation never reaches the window behind.
    pub fn key(&mut self, key: &KeyInput, shift: bool, today: Option<NaiveDate>) -> bool {
        match key {
            KeyInput::Escape => return true,
            KeyInput::PageUp => self.move_month(false),
            KeyInput::PageDown => self.move_month(true),
            KeyInput::Home | KeyInput::Character('t' | 'T') => {
                if let Some(today) = today {
                    self.today(today);
                }
            }
            KeyInput::Tab => {
                self.focus = match (self.focus, shift) {
                    (Focus::Day, false) | (Focus::Today, true) => Focus::Previous,
                    (Focus::Previous, false) | (Focus::Next, true) => Focus::Today,
                    (Focus::Today, false) | (Focus::Settings, true) => Focus::Next,
                    (Focus::Next, false) | (Focus::Day, true) => Focus::Settings,
                    (Focus::Settings, false) | (Focus::Previous, true) => Focus::Day,
                };
            }
            KeyInput::Enter | KeyInput::Character(' ') => match self.focus {
                Focus::Previous => self.move_month(false),
                Focus::Next => self.move_month(true),
                Focus::Today => {
                    if let Some(today) = today {
                        self.today(today);
                    }
                }
                Focus::Day => {}
                Focus::Settings => self.settings_requested = true,
            },
            KeyInput::Left => {
                self.focus = Focus::Day;
                self.move_days(-1);
            }
            KeyInput::Right => {
                self.focus = Focus::Day;
                self.move_days(1);
            }
            KeyInput::Up => {
                self.focus = Focus::Day;
                self.move_days(-7);
            }
            KeyInput::Down => {
                self.focus = Focus::Day;
                self.move_days(7);
            }
            _ => {}
        }
        false
    }

    fn button(ui: &mut Ui<'_>, rect: Rect, label: &str, focused: bool) -> bool {
        if focused || ui.input.hovering(rect) {
            ui.painter.fill_rounded_rect(
                rect.inset(ui.px(2)),
                ui.px(design::CHIP_RADIUS),
                design::accent_soft(),
            );
        }
        if focused {
            ui.painter
                .stroke_rect(rect.inset(ui.px(2)), ui.px(1), design::accent());
        }
        ui.label_styled(
            rect,
            label,
            design::state_style(ui),
            design::text(),
            Align::Centre,
        );
        ui.input.take_click(rect)
    }

    fn update_local_clock(&mut self, local_now: Option<NaiveDateTime>) {
        let today = local_now.map(|now| now.date());
        let minute = local_now.and_then(|now| now.with_second(0)?.with_nanosecond(0));
        if self.local_minute != minute {
            self.local_minute = minute;
            self.clock_label = minute.map_or_else(
                || "Local time unavailable".to_owned(),
                |now| now.format("%H:%M").to_string(),
            );
        }
        if self.local_date != today {
            self.local_date = today;
            self.date_label =
                today.map_or_else(String::new, |date| date.format("%A, %-d %B %Y").to_string());
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        layout: &Layout,
        local_now: Option<NaiveDateTime>,
    ) -> bool {
        self.update_local_clock(local_now);
        let today = self.local_date;
        ui.label_styled(
            layout.clock,
            &self.clock_label,
            ui.text_style(design::title_style(ui).with_size(22.0)),
            design::text(),
            Align::Centre,
        );
        ui.label_styled(
            layout.date,
            &self.date_label,
            design::state_style(ui),
            design::text_dim(),
            Align::Centre,
        );
        if Self::button(ui, layout.previous, "<", self.focus == Focus::Previous) {
            self.focus = Focus::Previous;
            self.move_month(false);
        }
        if Self::button(ui, layout.next, ">", self.focus == Focus::Next) {
            self.focus = Focus::Next;
            self.move_month(true);
        }
        if Self::button(ui, layout.today, "Today", self.focus == Focus::Today)
            && let Some(today) = today
        {
            self.focus = Focus::Today;
            self.today(today);
        }
        // The clock on the bar stays words. The date plate is this popup's own mark, set into
        // the month row when the row has room for it beside the month's name.
        let mark = ui.px(18).min(layout.month.h.saturating_sub(2));
        let month_text = if layout.month.w > mark.saturating_mul(5) {
            let cell = Rect::new(
                layout.month.x,
                layout.month.y + (layout.month.h - mark) / 2,
                mark,
                mark,
            );
            glyphs::draw(
                &mut ui.painter,
                ui.kernel,
                cell,
                Glyph::Calendar,
                design::text(),
            );
            Rect::new(
                layout.month.x + mark + ui.px(4),
                layout.month.y,
                layout.month.w - mark - ui.px(4),
                layout.month.h,
            )
        } else {
            layout.month
        };
        ui.label_styled(
            month_text,
            &self.month_label,
            design::state_style(ui),
            design::text(),
            Align::Centre,
        );
        for (column, weekday) in WEEKDAYS.iter().enumerate() {
            let left = layout.weekdays.x + layout.weekdays.w * column as i32 / 7;
            let right = layout.weekdays.x + layout.weekdays.w * (column as i32 + 1) / 7;
            ui.label_styled(
                Rect::new(left, layout.weekdays.y, right - left, layout.weekdays.h),
                weekday,
                design::state_style(ui),
                design::text_faint(),
                Align::Centre,
            );
        }
        for index in 0..42 {
            let Some(day) = self.day_at(index) else {
                continue;
            };
            let cell = layout.cell(index);
            let selected = day == self.selected;
            if selected || ui.input.hovering(cell) {
                ui.painter.fill_rounded_rect(
                    cell.inset(ui.px(2)),
                    ui.px(design::CHIP_RADIUS),
                    design::accent_soft(),
                );
            }
            if Some(day) == today || (selected && self.focus == Focus::Day) {
                ui.painter
                    .stroke_rect(cell.inset(ui.px(2)), ui.px(1), design::accent());
            }
            ui.label_styled(
                cell,
                DAY_LABELS[day.day() as usize - 1],
                design::state_style(ui),
                if Some(day) == today {
                    design::accent()
                } else {
                    design::text()
                },
                Align::Centre,
            );
            if ui.input.take_click(cell) {
                self.focus = Focus::Day;
                self.select(day);
            }
        }
        ui.label_styled(
            layout.selected,
            &self.selected_label,
            design::state_style(ui),
            design::text_dim(),
            Align::Centre,
        );
        if ui.button(layout.settings, "Date & Time settings") {
            self.settings_requested = true;
        }
        if self.focus == Focus::Settings {
            ui.painter
                .stroke_rect(layout.settings.inset(ui.px(2)), ui.px(1), design::accent());
        }
        ui.label_styled(
            layout.hint,
            "Arrows: day   PgUp/PgDn: month   Esc: close",
            design::state_style(ui),
            design::text_faint(),
            Align::Centre,
        );
        // The frame's padding and blank cells belong to the popup, not the desktop behind it.
        let _ = ui.input.take_click(layout.frame);
        let _ = ui.input.take_context_click(layout.frame);
        std::mem::take(&mut self.settings_requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn leap_and_century_february_grids_have_correct_weekday_alignment() {
        for (year, days) in [(2024, 29), (1900, 28), (2000, 29), (2100, 28)] {
            let calendar = Calendar::new(date(year, 2, 1));
            let grid: Vec<_> = (0..42).filter_map(|cell| calendar.day_at(cell)).collect();
            assert_eq!(grid.len(), days);
            assert_eq!(grid[0], date(year, 2, 1));
            assert_eq!(grid[days - 1], date(year, 2, days as u32));
            let first = calendar.month.weekday().num_days_from_monday() as usize;
            assert_eq!(calendar.day_at(first), Some(calendar.month));
            assert!((0..first).all(|cell| calendar.day_at(cell).is_none()));
        }
    }

    #[test]
    fn month_navigation_clamps_selection_and_crosses_year_boundaries() {
        let mut calendar = Calendar::new(date(2024, 1, 31));
        calendar.move_month(true);
        assert_eq!(calendar.selected(), date(2024, 2, 29));
        calendar.today(date(2025, 1, 15));
        calendar.move_month(false);
        assert_eq!(calendar.month(), date(2024, 12, 1));
        calendar.move_month(true);
        assert_eq!(calendar.selected(), date(2025, 1, 15));
    }

    #[test]
    fn keyboard_selection_crosses_months_and_today_resets_both_month_and_day() {
        let today = date(2024, 2, 29);
        let mut calendar = Calendar::new(today);
        assert!(!calendar.key(&KeyInput::Right, false, Some(today)));
        assert_eq!(calendar.selected(), date(2024, 3, 1));
        calendar.key(&KeyInput::PageUp, false, Some(today));
        assert_eq!(calendar.selected(), date(2024, 2, 1));
        calendar.key(&KeyInput::Home, false, Some(today));
        assert_eq!(calendar.month(), date(2024, 2, 1));
        assert_eq!(calendar.selected(), today);
        assert!(calendar.key(&KeyInput::Escape, false, Some(today)));
    }

    #[test]
    fn reverse_tab_reaches_settings_without_changing_selected_date() {
        let today = date(2026, 10, 7);
        let mut calendar = Calendar::new(today);
        calendar.key(&KeyInput::Tab, true, Some(today));
        assert_eq!(calendar.focus, Focus::Settings);
        calendar.key(&KeyInput::Enter, false, Some(today));
        assert!(calendar.settings_requested);
        assert_eq!(calendar.selected(), today);
        calendar.key(&KeyInput::Tab, false, Some(today));
        assert_eq!(calendar.focus, Focus::Day);
    }
}
